//! Page rasterization on a pool of worker threads (CPU, via `typst-render`).

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use typst::foundations::{Content, Smart};
use typst::layout::{Abs, Frame, FrameItem, GroupItem, Point, Size};
use typst::syntax::Span;
use typst::utils::Scalar;
use typst::visualize::{Geometry, Paint};
use typst_layout::{Page, PagedDocument};
use typst_render::RenderOptions;

use crate::diff;
use crate::viewer::{UiEvent, UiSender};

/// Identifies a rendered page bitmap: page content hash + resolution + style.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct RenderKey {
    pub hash: u128,
    /// Pixels per point × 1000.
    pub scale_milli: u32,
    pub invert: bool,
}

impl RenderKey {
    pub fn scale(&self) -> f64 {
        self.scale_milli as f64 / 1000.0
    }
}

/// An opaque page image in `0x00RRGGBB` format.
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

impl Bitmap {
    pub fn bytes(&self) -> usize {
        self.pixels.len() * 4
    }
}

/// A run of horizontal strips of a page: rows `first * strip_px` up to
/// `end * strip_px` (clipped to the page).
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct Strips {
    pub strip_px: u32,
    pub first: u32,
    pub end: u32,
}

impl Strips {
    pub fn rows(&self, height_px: u32) -> Range<u32> {
        (self.first * self.strip_px).min(height_px)..(self.end * self.strip_px).min(height_px)
    }
}

/// Groups sorted strip indices into runs of consecutive strips.
pub fn runs(strips: &[u32], strip_px: u32) -> Vec<Strips> {
    let mut out: Vec<Strips> = Vec::new();
    for &k in strips {
        match out.last_mut() {
            Some(run) if run.end == k => run.end += 1,
            _ => out.push(Strips { strip_px, first: k, end: k + 1 }),
        }
    }
    out
}

struct Job {
    key: RenderKey,
    doc: Arc<PagedDocument>,
    index: usize,
    /// `None`: the whole page.
    strips: Option<Strips>,
}

pub struct RenderPool {
    tx: Sender<Job>,
    wanted: Arc<Mutex<HashSet<RenderKey>>>,
}

impl RenderPool {
    /// With `opaque_alpha`, pixels are `0xFFRRGGBB` (BGRA bytes in memory),
    /// otherwise `0x00RRGGBB`.
    pub fn new(proxy: UiSender, opaque_alpha: bool) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<Job>();
        let wanted = Arc::new(Mutex::new(HashSet::new()));
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
        for i in 0..threads {
            let rx: Receiver<Job> = rx.clone();
            let wanted = wanted.clone();
            let proxy = proxy.clone();
            std::thread::Builder::new()
                .name(format!("render-{i}"))
                .stack_size(16 * 1024 * 1024)
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        // Skip work that is no longer visible.
                        if !wanted.lock().contains(&job.key) {
                            let _ = proxy.send_event(UiEvent::RenderSkipped(job.key, job.strips));
                            continue;
                        }
                        let Some(page) = job.doc.pages().get(job.index) else { continue };
                        let t = std::time::Instant::now();
                        let (scale, invert) = (job.key.scale(), job.key.invert);
                        let mut bitmap = match job.strips {
                            None => render_page(page, scale, invert),
                            Some(s) => render_rows(page, scale, invert, s.rows(page_px(page, scale).1)),
                        };
                        if opaque_alpha {
                            bitmap.pixels.iter_mut().for_each(|p| *p |= 0xff00_0000);
                        }
                        log::trace!("rendered page {} {:?} in {:?}", job.index + 1, job.strips, t.elapsed());
                        if !proxy.send_event(UiEvent::Rendered(job.key, job.strips, Arc::new(bitmap))) {
                            return;
                        }
                    }
                })
                .expect("spawn render thread");
        }
        Self { tx, wanted }
    }

    pub fn set_wanted(&self, keys: HashSet<RenderKey>) {
        *self.wanted.lock() = keys;
    }

    pub fn add_wanted(&self, key: RenderKey) {
        self.wanted.lock().insert(key);
    }

    pub fn submit(&self, key: RenderKey, doc: Arc<PagedDocument>, index: usize) {
        let _ = self.tx.send(Job { key, doc, index, strips: None });
    }

    pub fn submit_strips(&self, key: RenderKey, doc: Arc<PagedDocument>, index: usize, strips: Strips) {
        let _ = self.tx.send(Job { key, doc, index, strips: Some(strips) });
    }
}

/// Size of a page bitmap in pixels (as `typst-render` computes it).
pub fn page_px(page: &Page, scale: f64) -> (u32, u32) {
    let s = scale as f32;
    let size = page.frame.size();
    let px = |pt: Abs| (s * pt.to_pt() as f32).round().max(1.0) as u32;
    (px(size.x), px(size.y))
}

/// Renders the pixel rows `rows` of a page: the same pixels as the
/// corresponding rows of [`render_page`], at a fraction of the cost.
pub fn render_rows(page: &Page, scale: f64, invert: bool, rows: Range<u32>) -> Bitmap {
    let (_, height_px) = page_px(page, scale);
    if rows.start == 0 && rows.end >= height_px {
        return render_page(page, scale, invert);
    }
    let size = page.frame.size();
    let top = rows.start as f64 / scale;
    let height = rows.len() as f64 / scale;
    // One extra pixel on each side for anti-aliasing.
    let mut content = diff::cull(&page.frame, top - 1.0 / scale, top + height + 1.0 / scale);
    let fill = match page.fill_or_white() {
        None | Some(Paint::Solid(_)) => page.fill.clone(),
        Some(paint) => {
            // Gradients and tilings are laid out relative to the whole page.
            let mut f = Frame::hard(size);
            f.push(Point::zero(), FrameItem::Shape(Geometry::Rect(size).filled(paint), Span::detached()));
            f.push(Point::zero(), FrameItem::Group(GroupItem::new(content)));
            content = f;
            Smart::Custom(None)
        }
    };
    let mut frame = Frame::hard(Size::new(size.x, Abs::pt(height)));
    frame.push(Point::new(Abs::zero(), Abs::pt(-top)), FrameItem::Group(GroupItem::new(content)));
    let band = Page {
        frame,
        bleed: Default::default(),
        fill,
        numbering: None,
        supplement: Content::empty(),
        number: page.number,
    };
    let mut bitmap = render_page(&band, scale, invert);
    // Float rounding may leave the band one row off.
    let want = rows.len() as u32;
    if bitmap.height != want {
        let fill = bitmap.pixels.last().copied().unwrap_or(0x00ff_ffff);
        bitmap.pixels.resize((bitmap.width * want) as usize, fill);
        bitmap.height = want;
    }
    bitmap
}

pub fn render_page(page: &typst_layout::Page, scale: f64, invert: bool) -> Bitmap {
    let opts = RenderOptions { pixel_per_pt: Scalar::new(scale), ..RenderOptions::default() };
    let pixmap = typst_render::render(page, &opts);
    let (width, height) = (pixmap.width(), pixmap.height());
    let pixels = pixmap
        .data()
        .chunks_exact(4)
        .map(|p| {
            // Premultiplied RGBA → opaque RGB over white.
            let ia = 255 - p[3] as u32;
            let (r, g, b) = (p[0] as u32 + ia, p[1] as u32 + ia, p[2] as u32 + ia);
            if invert { invert_lightness(r, g, b) } else { (r << 16) | (g << 8) | b }
        })
        .collect();
    Bitmap { width, height, pixels }
}

/// Inverts lightness while keeping hue: c' = c + 255 - 2·luma.
#[inline]
fn invert_lightness(r: u32, g: u32, b: u32) -> u32 {
    let luma = (r * 77 + g * 150 + b * 29) >> 8;
    let f = |c: u32| (c as i32 + 255 - 2 * luma as i32).clamp(0, 255) as u32;
    (f(r) << 16) | (f(g) << 8) | f(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_runs() {
        let r = runs(&[0, 1, 2, 5, 7, 8], 128);
        let pairs: Vec<(u32, u32)> = r.iter().map(|s| (s.first, s.end)).collect();
        assert_eq!(pairs, vec![(0, 3), (5, 6), (7, 9)]);
        assert_eq!(r[0].rows(300), 0..300);
        assert_eq!(r[1].rows(1000), 640..768);
    }

    #[test]
    fn invert_black_white() {
        assert_eq!(invert_lightness(255, 255, 255), 0x000000);
        assert_eq!(invert_lightness(0, 0, 0), 0xffffff);
    }
}
