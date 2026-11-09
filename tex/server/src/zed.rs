//! Connection to the preview pane of (patched) Zed: `docs/PROTOCOL.md` of
//! zed-typst-livepreview (protocol v3).
//!
//! Zed listens on `/tmp/zed-preview-<uid>/<zed pid>.sock`; this server is a
//! language server started by Zed, so it finds the socket through its parent
//! pid. Frames: `u32 LE header length | JSON header | u32 LE payload length |
//! payload`.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::Sender;
use serde_json::{Value, json};

use crate::core::CoreMsg;
use crate::pdf::Bitmap;

pub const PROTOCOL_VERSION: u32 = 3;

pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("ZED_PREVIEW_SOCKET") {
        return p.into();
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/zed-preview-{uid}")).join(format!("{}.sock", std::os::unix::process::parent_id()))
}

/// Connects to Zed's preview pane host, if the editor provides one.
pub fn connect() -> Option<UnixStream> {
    let path = socket_path();
    let stream = UnixStream::connect(&path).ok()?;
    log::info!("connected to Zed's preview pane host at {}", path.display());
    Some(stream)
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

enum Payload {
    None,
    /// Rows `start..end` of a page bitmap.
    Rows(Arc<Bitmap>, u32, u32),
}

/// Sends frames to Zed (from any thread).
#[derive(Clone)]
pub struct ZedOut {
    tx: Sender<(Value, Payload)>,
}

impl ZedOut {
    pub fn send(&self, header: Value) {
        let _ = self.tx.send((header, Payload::None));
    }

    pub fn send_rows(&self, header: Value, bitmap: Arc<Bitmap>, rows: std::ops::Range<u32>) {
        let _ = self.tx.send((header, Payload::Rows(bitmap, rows.start, rows.end)));
    }
}

/// Starts the reader and writer threads, says `hello`, and forwards what Zed
/// sends to the core.
pub fn start(stream: UnixStream, core: Sender<CoreMsg>) -> io::Result<ZedOut> {
    let (tx, rx) = crossbeam_channel::unbounded::<(Value, Payload)>();
    let write_stream = stream.try_clone()?;
    std::thread::Builder::new().name("zed-writer".into()).spawn(move || {
        let mut w = BufWriter::with_capacity(1 << 20, write_stream);
        for (header, payload) in rx {
            let bytes = match &payload {
                Payload::None => &[][..],
                Payload::Rows(b, start, end) => {
                    let row = b.width as usize * 4;
                    &b.bytes[*start as usize * row..*end as usize * row]
                }
            };
            if write_frame(&mut w, &header, bytes).is_err() {
                return;
            }
        }
    })?;
    std::thread::Builder::new().name("zed-reader".into()).spawn(move || {
        let mut r = BufReader::new(stream);
        while let Ok((header, _)) = read_frame(&mut r) {
            if core.send(CoreMsg::Zed(header)).is_err() {
                return;
            }
        }
        let _ = core.send(CoreMsg::ZedClosed);
    })?;
    let out = ZedOut { tx };
    out.send(json!({
        "type": "hello",
        "version": PROTOCOL_VERSION,
        "pid": std::process::id(),
        "kind": "latex",
        "name": "LaTeX",
        "extensions": ["tex", "ltx", "latex"],
        "layout": "fixed",
    }));
    Ok(out)
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
