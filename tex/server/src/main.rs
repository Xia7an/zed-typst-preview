//! tex-livepreview: a native live preview for LaTeX in (patched) Zed.
//!
//! Modes:
//! - `tex-livepreview` (or `lsp`): language server for Zed, over stdio. With
//!   Zed's preview pane patch, the pages are shown in a Zed pane.
//! - `tex-livepreview check <file.tex> [--png <out.png>] [--scale <px/pt>]`:
//!   compiles once and reports what the preview would show (for debugging).

mod config;
mod core;
mod lsp;
mod pdf;
mod project;
mod render;
mod synctex;
mod zed;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::config::Config;
use crate::project::{Job, canonical};

fn usage() -> ! {
    eprintln!(
        "tex-livepreview {}\n\nUSAGE:\n  tex-livepreview [lsp]                                       run as a language server (stdio)\n  tex-livepreview check <FILE> [--png <OUT>] [--scale <PX/PT>] [--engine <E>]  compile once and report",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("tex_livepreview=info"))
        .target(env_logger::Target::Stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("lsp") {
        "--version" | "-V" => println!("tex-livepreview {}", env!("CARGO_PKG_VERSION")),
        "--help" | "-h" => usage(),
        "check" => {
            if let Err(e) = check(&args[1..]) {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        }
        "lsp" | "--stdio" => {
            if let Err(e) = serve() {
                log::error!("language server error: {e:#}");
                std::process::exit(1);
            }
        }
        _ => usage(),
    }
}

fn serve() -> anyhow::Result<()> {
    let (connection, io_threads) = lsp_server::Connection::stdio();
    let (tx, rx) = crossbeam_channel::unbounded();
    let zed = match zed::connect() {
        Some(stream) => Some(zed::start(stream, tx.clone())?),
        None => {
            log::warn!("Zed's preview pane is not available ({}): no preview", zed::socket_path().display());
            None
        }
    };
    let embedded = zed.is_some();
    let shared = Arc::new(core::Shared::default());
    let core_thread = core::spawn(Config::default(), zed, shared.clone(), tx.clone(), rx);
    let result = lsp::run(connection, tx.clone(), shared, embedded);
    let _ = tx.send(core::CoreMsg::Shutdown);
    let _ = core_thread.join();
    io_threads.join()?;
    result
}

/// Compiles `file` once, like the preview does, and prints the result.
fn check(args: &[String]) -> anyhow::Result<()> {
    let mut file = None;
    let mut png = None;
    let mut scale = 2.0f32;
    let mut cfg = Config::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--png" => png = it.next().map(PathBuf::from),
            "--scale" => scale = it.next().and_then(|s| s.parse().ok()).unwrap_or(scale),
            "--engine" => cfg.engine = it.next().cloned().unwrap_or_default(),
            _ => file = Some(PathBuf::from(a)),
        }
    }
    let Some(file) = file else { usage() };
    let main = canonical(&file);
    let t = Instant::now();
    let outcome = project::compile(Job { id: 1, main: main.clone(), buffers: Vec::new(), cfg, reload: true });
    println!("compiled {} in {} ms ({} total)", main.display(), outcome.ms, t.elapsed().as_millis());
    for e in &outcome.errors {
        println!("error: {e}");
    }
    println!("warnings: {}", outcome.warnings);
    let mut deps: Vec<&PathBuf> = outcome.deps.iter().collect();
    deps.sort();
    println!("project files: {deps:?}");
    let Some(doc) = outcome.doc else { anyhow::bail!("no PDF") };
    for (i, p) in doc.pages.iter().enumerate() {
        println!("page {}: {:.1} x {:.1} pt, hash {:x}", i + 1, p.width, p.height, p.hash);
    }
    let scale_milli = (scale * 1000.0) as u32;
    let t = Instant::now();
    let bitmap = pdf::render(&doc, 0, scale_milli, false).ok_or_else(|| anyhow::anyhow!("cannot render"))?;
    println!("rendered page 1 at {scale} px/pt: {}x{} in {} ms", bitmap.width, bitmap.height, t.elapsed().as_millis());
    if let Some(st) = &outcome.synctex {
        for line in 1..=40 {
            if let Some(m) = st.forward(&main, line, 0.0) {
                let back = st.backward(m.page, m.x + 1.0, m.y + m.height / 2.0);
                println!(
                    "synctex: line {line} → page {} ({:.1}, {:.1}) h {:.1} → back: {:?}",
                    m.page + 1,
                    m.x,
                    m.y,
                    m.height,
                    back.map(|(p, l, f)| (p.file_name().unwrap_or_default().to_string_lossy().into_owned(), l, (f * 100.0).round()))
                );
                break;
            }
        }
    } else {
        println!("no synctex data");
    }
    if let Some(out) = png {
        write_png(&out, &bitmap)?;
        println!("wrote {}", out.display());
    }
    Ok(())
}

/// A minimal PNG writer (stored zlib blocks via flate2) for `check --png`.
fn write_png(path: &Path, b: &pdf::Bitmap) -> anyhow::Result<()> {
    use std::io::Write;
    let mut raw = Vec::with_capacity((b.width * b.height * 3 + b.height) as usize);
    for row in b.bytes.chunks_exact(b.width as usize * 4) {
        raw.push(0);
        for px in row.chunks_exact(4) {
            raw.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    z.write_all(&raw)?;
    let idat = z.finish()?;
    let crc = |data: &[u8]| {
        let mut c = !0u32;
        for &byte in data {
            c ^= byte as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    };
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        png.extend_from_slice(&body);
        png.extend_from_slice(&crc(&body).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&b.width.to_be_bytes());
    ihdr.extend_from_slice(&b.height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &idat);
    chunk(b"IEND", &[]);
    std::fs::write(path, png)?;
    Ok(())
}
