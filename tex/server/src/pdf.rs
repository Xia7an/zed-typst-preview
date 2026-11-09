//! PDF pages: sizes, content hashes and rasterization (`hayro`, pure Rust,
//! on the CPU).

use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::hayro_syntax::object::{Dict, Name, Stream};
use hayro::hayro_syntax::page::Page;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings};
use parking_lot::Mutex;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageInfo {
    /// Points.
    pub width: f32,
    pub height: f32,
    /// Changes when (and, as far as possible, only when) the page looks
    /// different.
    pub hash: u128,
}

/// A compiled document.
pub struct PdfDoc {
    pub pdf: Pdf,
    pub pages: Vec<PageInfo>,
}

impl PdfDoc {
    pub fn load(bytes: Vec<u8>) -> anyhow::Result<Self> {
        let pdf = Pdf::new(bytes).map_err(|e| anyhow::anyhow!("cannot read the PDF: {e:?}"))?;
        let pages = pdf.pages().iter().map(page_info).collect();
        Ok(Self { pdf, pages })
    }
}

fn page_info(page: &Page<'_>) -> PageInfo {
    let (width, height) = page.render_dimensions();
    PageInfo { width, height, hash: page_hash(page, width, height) }
}

/// Hashes what decides the look of a page: its content stream, size, the
/// fonts (by name, without the subset tag, which changes whenever another
/// glyph is used anywhere in the document) and the XObjects (figures, forms).
fn page_hash(page: &Page<'_>, width: f32, height: f32) -> u128 {
    let mut a = DefaultHasher::new();
    let mut b = DefaultHasher::new();
    0xa5u8.hash(&mut b);
    let mut feed = |bytes: &[u8]| {
        bytes.hash(&mut a);
        bytes.hash(&mut b);
    };
    feed(&width.to_le_bytes());
    feed(&height.to_le_bytes());
    feed(format!("{:?}", page.rotation()).as_bytes());
    feed(page.page_stream().unwrap_or_default());
    let resources = page.resources();
    for (name, _) in resources.fonts.entries() {
        feed(name.as_str().as_bytes());
        let font: Option<Dict<'_>> = resources.fonts.get(name.as_str());
        let base = font.and_then(|f| f.get::<Name<'_>>(b"BaseFont")).map(|n| n.as_str().to_string()).unwrap_or_default();
        let base = match base.split_once('+') {
            Some((tag, rest)) if tag.len() == 6 && tag.bytes().all(|c| c.is_ascii_uppercase()) => rest.to_string(),
            _ => base,
        };
        feed(base.as_bytes());
    }
    for (name, _) in resources.x_objects.entries() {
        feed(name.as_str().as_bytes());
        if let Some(stream) = resources.x_objects.get::<Stream<'_>>(name.as_str()) {
            feed(&stream.raw_data());
        }
    }
    ((a.finish() as u128) << 64) | b.finish() as u128
}

/// Bitmap size of a page, rounded like the pane does (`typst-render`).
pub fn page_px(page: &PageInfo, scale_milli: u32) -> (u32, u32) {
    let s = (scale_milli as f64 / 1000.0) as f32;
    let px = |pt: f32| (s * pt).round().max(1.0) as u32;
    (px(page.width), px(page.height))
}

/// An opaque page image, BGRA8 in memory (`0xFFRRGGBB` little endian).
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

/// Renders page `index` at `scale_milli` (pixels per point × 1000).
pub fn render(doc: &PdfDoc, index: usize, scale_milli: u32, invert: bool) -> Option<Bitmap> {
    let page = doc.pdf.pages().get(index)?;
    let info = doc.pages.get(index)?;
    let (w, h) = page_px(info, scale_milli);
    let (w, h) = (w.min(u16::MAX as u32), h.min(u16::MAX as u32));
    let cache = RenderCache::new();
    let settings = RenderSettings {
        // Exactly the rounded size, so that the page fills the bitmap.
        x_scale: w as f32 / info.width,
        y_scale: h as f32 / info.height,
        width: Some(w as u16),
        height: Some(h as u16),
        bg_color: WHITE,
    };
    let pixmap = hayro::render(page, &cache, &InterpreterSettings::default(), &settings);
    let mut bytes = Vec::with_capacity((w * h * 4) as usize);
    for p in pixmap.data() {
        // Premultiplied over an opaque white base: alpha is 255.
        let (r, g, b) = (p.r as u32, p.g as u32, p.b as u32);
        let (r, g, b) = if invert { invert_lightness(r, g, b) } else { (r, g, b) };
        bytes.extend_from_slice(&[b as u8, g as u8, r as u8, 0xff]);
    }
    Some(Bitmap { width: w, height: h, bytes })
}

/// Inverts lightness while keeping hue: c' = c + 255 - 2·luma.
#[inline]
fn invert_lightness(r: u32, g: u32, b: u32) -> (u32, u32, u32) {
    let luma = ((r * 77 + g * 150 + b * 29) >> 8) as i32;
    let f = |c: u32| (c as i32 + 255 - 2 * luma).clamp(0, 255) as u32;
    (f(r), f(g), f(b))
}

/// Recently rendered whole pages: the pane asks for a page strip by strip,
/// and hayro can only render whole pages.
#[derive(Default)]
pub struct PageCache {
    entries: Mutex<VecDeque<((u128, u32, bool), Arc<Bitmap>)>>,
}

impl PageCache {
    const MAX_BYTES: usize = 160 << 20;

    pub fn get(&self, key: (u128, u32, bool)) -> Option<Arc<Bitmap>> {
        let mut entries = self.entries.lock();
        let i = entries.iter().position(|(k, _)| *k == key)?;
        let entry = entries.remove(i)?;
        let bitmap = entry.1.clone();
        entries.push_back(entry);
        Some(bitmap)
    }

    pub fn insert(&self, key: (u128, u32, bool), bitmap: Arc<Bitmap>) {
        let mut entries = self.entries.lock();
        entries.retain(|(k, _)| *k != key);
        entries.push_back((key, bitmap));
        while entries.len() > 2 && entries.iter().map(|(_, b)| b.bytes.len()).sum::<usize>() > Self::MAX_BYTES {
            entries.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invert_black_white() {
        assert_eq!(invert_lightness(255, 255, 255), (0, 0, 0));
        assert_eq!(invert_lightness(0, 0, 0), (255, 255, 255));
    }

    #[test]
    fn rounding_matches_the_pane() {
        let p = PageInfo { width: 595.28, height: 841.89, hash: 0 };
        assert_eq!(page_px(&p, 2000), (1191, 1684));
    }
}
