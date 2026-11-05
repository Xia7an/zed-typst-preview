//! Wire format shared with the `typst-livepreview` server (see its
//! `docs/PROTOCOL.md`): `u32 LE header length | JSON header | u32 LE payload
//! length | payload`.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde_json::Value;

pub const PROTOCOL_VERSION: u64 = 2;

/// `/tmp/zed-typst-preview-<uid>/<zed pid>.sock`. Language servers are child
/// processes of Zed, so they find it through their parent pid.
pub fn socket_dir() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/zed-typst-preview-{uid}"))
}

pub fn socket_path() -> PathBuf {
    socket_dir().join(format!("{}.sock", std::process::id()))
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
    let header =
        serde_json::from_slice(&h).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((header, p))
}

pub fn parse_hash(v: Option<&Value>) -> Option<u128> {
    u128::from_str_radix(v?.as_str()?, 16).ok()
}
