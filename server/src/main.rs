//! typst-livepreview: a native live preview for Typst.
//!
//! Modes:
//! - `typst-livepreview` (or `lsp`): language server for Zed, over stdio.
//! - `typst-livepreview preview <file.typ>`: standalone viewer that reloads on save.
//! - `typst-livepreview bench <file.typ>`: measures edit → pixels latency.

mod bench;
mod config;
mod core;
mod diff;
#[cfg(unix)]
mod embed;
mod lsp;
mod position;
mod render;
mod viewer;
mod world;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use winit::event_loop::EventLoop;

use crate::config::Config;
use crate::core::{CoreMsg, Editor};
use crate::viewer::{UiEvent, UiSender, Viewer};

/// In standalone mode, jumps are printed to stdout.
struct PrintEditor;

impl Editor for PrintEditor {
    fn show_document(&self, path: &Path, line: u32, character: u32) {
        println!("{}:{}:{}", path.display(), line + 1, character + 1);
    }
}

fn usage() -> ! {
    eprintln!(
        "typst-livepreview {}\n\nUSAGE:\n  typst-livepreview [lsp]            run as a language server (stdio)\n  typst-livepreview preview <FILE>   open a standalone preview window\n  typst-livepreview bench <FILE>     measure compile + render latency",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("typst_livepreview=info"))
        .target(env_logger::Target::Stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("lsp");
    match mode {
        "--version" | "-V" => {
            println!("typst-livepreview {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        "--help" | "-h" => usage(),
        "bench" => {
            let Some(file) = args.get(1) else { usage() };
            if let Err(e) = bench::run(Path::new(file)) {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        "lsp" | "--stdio" | "preview" => {}
        _ => usage(),
    }

    // Inside a patched Zed, show the preview in Zed's own pane.
    #[cfg(unix)]
    if mode != "preview" && std::env::var_os("TYPST_LIVEPREVIEW_NO_EMBED").is_none() {
        if let Some(stream) = embed::connect() {
            embed::run(stream);
            std::process::exit(0);
        }
    }

    let event_loop = build_event_loop();
    let proxy = event_loop.create_proxy();

    let mut viewer = if mode == "preview" {
        let Some(file) = args.get(1) else { usage() };
        let path = std::fs::canonicalize(file).unwrap_or_else(|e| {
            eprintln!("error: {file}: {e}");
            std::process::exit(1)
        });
        let cfg = Config::default();
        let root = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let (core, _) = core::spawn(
            cfg.clone(),
            vec![root],
            Box::new(PrintEditor),
            UiSender::from_proxy(proxy.clone()),
            Default::default(),
        );
        watch_file(path.clone(), core.clone());
        let _ = core.send(CoreMsg::OpenPreview { path: Some(path), split: true, explicit: true });
        Viewer::new(proxy, Some(core), cfg)
    } else {
        let lsp_proxy = proxy.clone();
        std::thread::Builder::new()
            .name("lsp".into())
            .spawn(move || {
                if let Err(e) = lsp::run(UiSender::from_proxy(lsp_proxy.clone()), false) {
                    log::error!("language server error: {e:#}");
                }
                let _ = lsp_proxy.send_event(UiEvent::Exit);
            })
            .expect("spawn lsp thread");
        Viewer::new(proxy, None, Config::default())
    };

    if let Err(e) = event_loop.run_app(&mut viewer) {
        log::error!("event loop error: {e}");
    }
    std::process::exit(0);
}

fn build_event_loop() -> EventLoop<UiEvent> {
    let mut builder = EventLoop::<UiEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        // No Dock icon while running in the background as a language server.
        builder.with_activation_policy(ActivationPolicy::Accessory).with_default_menu(false);
    }
    match builder.build() {
        Ok(el) => el,
        Err(e) => {
            log::error!("cannot create the window event loop: {e}");
            std::process::exit(1);
        }
    }
}

/// Standalone mode: reloads the file when it changes on disk.
fn watch_file(path: PathBuf, core: crossbeam_channel::Sender<CoreMsg>) {
    std::thread::spawn(move || {
        let stamp = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        let mut last = None;
        let mut tick = 0u32;
        loop {
            tick = tick.wrapping_add(1);
            let now = stamp(&path);
            if last != Some(now) {
                last = Some(now);
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if core.send(CoreMsg::Open { path: path.clone(), text }).is_err() {
                        return;
                    }
                }
            } else if tick % 10 == 0 && core.send(CoreMsg::FilesChanged).is_err() {
                // Re-check other files (images, includes) once a second.
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}
