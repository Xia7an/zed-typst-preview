//! Bridge to the preview pane built into (patched) Zed.
//!
//! When Zed runs with the `external_preview` patch, it listens on a Unix
//! socket at `/tmp/zed-preview-<uid>/<zed pid>.sock` (older patches:
//! `/tmp/zed-typst-preview-…`). The language server (a child of Zed) connects
//! to it at startup and then streams page bitmaps to the pane instead of
//! opening its own window. See `docs/PROTOCOL.md`.
//!
//! Frame format (both directions):
//! `u32 LE header length | JSON header | u32 LE payload length | payload`.

use std::collections::HashSet;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::Sender;
use serde_json::{Value, json};
use typst_layout::PagedDocument;

use crate::core::CoreMsg;
use crate::diff;
use crate::render::{self, Bitmap, RenderKey, RenderPool, Strips};
use crate::viewer::{Marker, UiEvent, UiSender};

pub const PROTOCOL_VERSION: u32 = 3;

/// Where Zed's preview pane host may listen, in order: an explicit override,
/// then the v3 socket, then the one of older patches (protocol v2).
pub fn socket_paths() -> Vec<PathBuf> {
    for var in ["ZED_PREVIEW_SOCKET", "ZED_TYPST_PREVIEW_SOCKET"] {
        if let Some(p) = std::env::var_os(var) {
            return vec![p.into()];
        }
    }
    let uid = unsafe { libc::getuid() };
    let sock = format!("{}.sock", std::os::unix::process::parent_id());
    ["zed-preview", "zed-typst-preview"]
        .iter()
        .map(|dir| PathBuf::from(format!("/tmp/{dir}-{uid}")).join(&sock))
        .collect()
}

/// Connects to Zed's preview pane host, if the editor provides one.
pub fn connect() -> Option<UnixStream> {
    socket_paths().into_iter().find_map(|path| {
        let stream = UnixStream::connect(&path).ok()?;
        log::info!("connected to Zed's preview pane host at {}", path.display());
        Some(stream)
    })
}

enum Payload {
    None,
    Bitmap(Arc<Bitmap>),
    /// Rows `start..end` of a bitmap (one strip of a rendered run).
    Rows(Arc<Bitmap>, u32, u32),
}

pub fn write_frame(w: &mut impl Write, header: &Value, payload: &[u8]) -> io::Result<()> {
    let h = serde_json::to_vec(header)?;
    w.write_all(&(h.len() as u32).to_le_bytes())?;
    w.write_all(&h)?;
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<(Value, Vec<u8>)> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut h = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut h)?;
    r.read_exact(&mut len)?;
    let mut p = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut p)?;
    let header = serde_json::from_slice(&h).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((header, p))
}

fn pixels_as_bytes(b: &Bitmap) -> &[u8] {
    // SAFETY: u32 has no padding and any byte pattern is a valid u8.
    unsafe { std::slice::from_raw_parts(b.pixels.as_ptr().cast::<u8>(), b.pixels.len() * 4) }
}

fn marker_json(m: &Marker) -> Value {
    json!({ "page": m.page, "x": m.x, "y": m.y, "height": m.height })
}

fn parse_hash(v: &Value) -> Option<u128> {
    u128::from_str_radix(v.as_str()?, 16).ok()
}

fn skipped_json(key: &RenderKey, strips: Option<Strips>) -> Value {
    let mut v = json!({ "type": "skipped", "hash": format!("{:x}", key.hash), "scale_milli": key.scale_milli });
    if let Some(s) = strips {
        v["first"] = s.first.into();
        v["end"] = s.end.into();
    }
    v
}

enum In {
    Ui(UiEvent),
    Zed(Value),
    Closed,
}

/// Runs the language server with the preview shown inside Zed. Returns when
/// the editor shuts the server down or closes the connection.
pub fn run(stream: UnixStream) {
    let (tx, rx) = crossbeam_channel::unbounded::<In>();
    let ui = {
        let tx = tx.clone();
        UiSender::new(move |e| tx.send(In::Ui(e)).is_ok())
    };

    // Writer thread: large bitmaps never block the bridge.
    let (out_tx, out_rx) = crossbeam_channel::unbounded::<(Value, Payload)>();
    let write_stream = stream.try_clone().expect("clone socket");
    std::thread::Builder::new()
        .name("zed-writer".into())
        .spawn(move || {
            let mut w = BufWriter::with_capacity(1 << 20, write_stream);
            for (header, payload) in out_rx {
                let bytes = match &payload {
                    Payload::None => &[][..],
                    Payload::Bitmap(b) => pixels_as_bytes(b),
                    Payload::Rows(b, start, end) => {
                        let row = b.width as usize * 4;
                        &pixels_as_bytes(b)[*start as usize * row..*end as usize * row]
                    }
                };
                if write_frame(&mut w, &header, bytes).is_err() {
                    return;
                }
            }
        })
        .expect("spawn writer");

    // Reader thread.
    {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("zed-reader".into())
            .spawn(move || {
                let mut r = BufReader::new(stream);
                while let Ok((header, _)) = read_frame(&mut r) {
                    if tx.send(In::Zed(header)).is_err() {
                        return;
                    }
                }
                let _ = tx.send(In::Closed);
            })
            .expect("spawn reader");
    }

    let send = |header: Value, payload: Payload| {
        let _ = out_tx.send((header, payload));
    };
    // What this server renders (protocol v3; older panes ignore the fields).
    send(
        json!({
            "type": "hello",
            "version": PROTOCOL_VERSION,
            "pid": std::process::id(),
            "kind": "typst",
            "name": "Typst",
            "extensions": ["typ"],
            "layout": "fixed",
        }),
        Payload::None,
    );

    // The language server itself.
    {
        let ui = ui.clone();
        std::thread::Builder::new()
            .name("lsp".into())
            .spawn(move || {
                if let Err(e) = crate::lsp::run(ui.clone(), true) {
                    log::error!("language server error: {e:#}");
                }
                ui.send_event(UiEvent::Exit);
            })
            .expect("spawn lsp thread");
    }

    let pool = RenderPool::new(ui.clone(), true);
    let mut core: Option<Sender<CoreMsg>> = None;
    let mut queued: Vec<CoreMsg> = Vec::new();
    let mut doc: Option<Arc<PagedDocument>> = None;
    let mut hashes: Vec<u128> = Vec::new();
    let mut invert = false;
    // What the pane asked for last: lets us render the changed strips of a
    // new document before the pane asks for them.
    let mut last_wanted: HashSet<RenderKey> = HashSet::new();
    let mut strip_px: Option<u32> = None;
    // Strips pushed for the current document: stay wanted even if a request
    // the pane sent before it saw the document arrives late.
    let mut pushed_keys: HashSet<RenderKey> = HashSet::new();

    let to_core = |core: &Option<Sender<CoreMsg>>, queued: &mut Vec<CoreMsg>, msg: CoreMsg| match core {
        Some(c) => {
            let _ = c.send(msg);
        }
        None => queued.push(msg),
    };

    for msg in rx {
        match msg {
            In::Ui(event) => match event {
                UiEvent::Show { path, split, explicit, .. } => {
                    if let Some(path) = path {
                        send(
                            json!({ "type": "open_preview", "path": path, "split": split, "explicit": explicit }),
                            Payload::None,
                        );
                    }
                }
                UiEvent::Title { title, path } => {
                    send(json!({ "type": "target", "path": path, "title": title }), Payload::None)
                }
                UiEvent::Roots(roots) => send(json!({ "type": "roots", "roots": roots }), Payload::None),
                UiEvent::Jump { path, line, character } => send(
                    json!({ "type": "jump", "path": path, "line": line, "character": character }),
                    Payload::None,
                ),
                UiEvent::Document(update) => {
                    let old_hashes = std::mem::replace(&mut hashes, update.pages.iter().map(|p| p.hash).collect());
                    doc = Some(update.doc.clone());
                    let mut pushes = Vec::new();
                    let pages: Vec<Value> = update
                        .pages
                        .iter()
                        .enumerate()
                        .map(|(i, p)| {
                            let mut v = json!({ "width": p.width, "height": p.height, "hash": format!("{:x}", p.hash) });
                            let Some((base, bands)) = &p.diff else { return v };
                            v["base"] = format!("{base:x}").into();
                            v["dirty"] = bands.iter().map(|&(a, b)| json!([a, b])).collect::<Vec<_>>().into();
                            // The pane showed the old version: re-render the
                            // strips that changed at the same resolution now.
                            let (Some(strip_px), Some(page)) = (strip_px, update.doc.pages().get(i)) else { return v };
                            if old_hashes.get(i) != Some(base) {
                                return v;
                            }
                            let mut pushed = Vec::new();
                            for old in last_wanted.iter().filter(|k| k.hash == *base && k.invert == invert) {
                                let key = RenderKey { hash: p.hash, ..*old };
                                let (_, height_px) = render::page_px(page, key.scale());
                                let strips = diff::dirty_strips(bands, key.scale(), height_px, strip_px);
                                if strips.is_empty() {
                                    continue;
                                }
                                pushed.push(json!({ "scale_milli": key.scale_milli, "strip_px": strip_px, "strips": strips }));
                                for run in render::runs(&strips, strip_px) {
                                    pushes.push((key, i, run));
                                }
                            }
                            if !pushed.is_empty() {
                                v["pushed"] = pushed.into();
                            }
                            v
                        })
                        .collect();
                    send(
                        json!({
                            "type": "document",
                            "generation": update.generation,
                            "pages": pages,
                            "compile_ms": update.status.compile_ms,
                            "warnings": update.status.warnings,
                        }),
                        Payload::None,
                    );
                    // After the document message, so the pane knows the pages.
                    pushed_keys = pushes.iter().map(|(key, _, _)| *key).collect();
                    for (key, index, run) in pushes {
                        pool.add_wanted(key);
                        last_wanted.insert(key);
                        pool.submit_strips(key, update.doc.clone(), index, run);
                    }
                }
                UiEvent::Status(status) => send(
                    json!({
                        "type": "status",
                        "error": status.error.as_ref().map(|(_, m)| m.clone()),
                        "errors": status.error.as_ref().map(|(n, _)| *n).unwrap_or(0),
                        "compile_ms": status.compile_ms,
                        "warnings": status.warnings,
                    }),
                    Payload::None,
                ),
                UiEvent::Cursor(markers) | UiEvent::CursorNoScroll(markers) if markers.is_empty() => {}
                UiEvent::Cursor(markers) => send(
                    json!({ "type": "cursor", "scroll": true, "markers": markers.iter().map(marker_json).collect::<Vec<_>>() }),
                    Payload::None,
                ),
                UiEvent::CursorNoScroll(markers) => send(
                    json!({ "type": "cursor", "scroll": false, "markers": markers.iter().map(marker_json).collect::<Vec<_>>() }),
                    Payload::None,
                ),
                UiEvent::Reveal(m) => send(json!({ "type": "reveal", "marker": marker_json(&m) }), Payload::None),
                UiEvent::Config(cfg) => {
                    if cfg.invert_colors != invert {
                        invert = cfg.invert_colors;
                        send(json!({ "type": "invalidate" }), Payload::None);
                    }
                }
                UiEvent::Rendered(key, strips, bitmap) => {
                    if key.invert != invert {
                        continue;
                    }
                    let header = |height: u32| {
                        json!({
                            "type": "bitmap",
                            "hash": format!("{:x}", key.hash),
                            "scale_milli": key.scale_milli,
                            "width": bitmap.width,
                            "height": height,
                        })
                    };
                    match strips {
                        None => send(header(bitmap.height), Payload::Bitmap(bitmap.clone())),
                        // One frame (and one image in the pane) per strip.
                        Some(s) => {
                            for k in s.first..s.end {
                                let start = (k - s.first) * s.strip_px;
                                let end = (start + s.strip_px).min(bitmap.height);
                                if start >= end {
                                    break;
                                }
                                let mut h = header(end - start);
                                h["strip"] = k.into();
                                h["strip_px"] = s.strip_px.into();
                                send(h, Payload::Rows(bitmap.clone(), start, end));
                            }
                        }
                    }
                }
                UiEvent::RenderSkipped(key, strips) => send(skipped_json(&key, strips), Payload::None),
                UiEvent::CoreReady(tx) => {
                    for m in queued.drain(..) {
                        let _ = tx.send(m);
                    }
                    core = Some(tx);
                }
                UiEvent::ActivateEditor => {}
                UiEvent::Exit => break,
            },
            In::Zed(h) => {
                let path = |key: &str| h.get(key).and_then(Value::as_str).map(PathBuf::from);
                let num = |key: &str| h.get(key).and_then(Value::as_f64);
                match h.get("type").and_then(Value::as_str).unwrap_or("") {
                    "attach" => {
                        if let Some(path) = path("path") {
                            let path = std::fs::canonicalize(&path).unwrap_or(path);
                            to_core(&core, &mut queued, CoreMsg::Attach { path });
                        }
                    }
                    "detach" => to_core(&core, &mut queued, CoreMsg::WindowClosed),
                    "click" => {
                        if let (Some(page), Some(x), Some(y)) = (num("page"), num("x"), num("y")) {
                            to_core(&core, &mut queued, CoreMsg::Click { page: page as usize, x, y });
                        }
                    }
                    "cursor" => {
                        if let (Some(path), Some(line), Some(character)) = (path("path"), num("line"), num("character")) {
                            let path = std::fs::canonicalize(&path).unwrap_or(path);
                            to_core(
                                &core,
                                &mut queued,
                                CoreMsg::Cursor { path, line: line as u32, character: character as u32 },
                            );
                        }
                    }
                    "render" => {
                        let key = |v: &Value| {
                            Some(RenderKey {
                                hash: parse_hash(v.get("hash")?)?,
                                scale_milli: v.get("scale_milli")?.as_u64()? as u32,
                                invert,
                            })
                        };
                        let wanted: HashSet<RenderKey> = h
                            .get("wanted")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(key).collect())
                            .unwrap_or_default();
                        last_wanted = wanted.clone();
                        pool.set_wanted(wanted.into_iter().chain(pushed_keys.iter().copied()).collect());
                        let Some(doc) = &doc else { continue };
                        for req in h.get("requests").and_then(Value::as_array).into_iter().flatten() {
                            let (Some(index), Some(k)) = (req.get("index").and_then(Value::as_u64), key(req)) else {
                                continue;
                            };
                            let index = index as usize;
                            let u32_field = |name: &str| req.get(name).and_then(Value::as_u64).map(|v| v as u32);
                            let strips = match (u32_field("strip_px"), u32_field("first"), u32_field("end")) {
                                (Some(strip_px), Some(first), Some(end)) if strip_px > 0 && first < end => {
                                    Some(Strips { strip_px, first, end })
                                }
                                _ => None,
                            };
                            if let Some(s) = strips {
                                strip_px = Some(s.strip_px);
                            }
                            if hashes.get(index) != Some(&k.hash) {
                                // Stale request (the document changed meanwhile).
                                send(skipped_json(&k, strips), Payload::None);
                                continue;
                            }
                            match strips {
                                Some(s) => pool.submit_strips(k, doc.clone(), index, s),
                                None => pool.submit(k, doc.clone(), index),
                            }
                        }
                    }
                    // Typst pages have a fixed size (`layout: fixed`).
                    "viewport" => {}
                    other => log::debug!("unknown message from Zed: {other}"),
                }
            }
            In::Closed => {
                log::info!("Zed closed the preview connection");
                break;
            }
        }
    }
    if let Some(core) = core {
        let _ = core.send(CoreMsg::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &json!({ "type": "x", "n": 1 }), &[1, 2, 3]).unwrap();
        let (h, p) = read_frame(&mut &buf[..]).unwrap();
        assert_eq!(h["type"], "x");
        assert_eq!(p, vec![1, 2, 3]);
    }
}
