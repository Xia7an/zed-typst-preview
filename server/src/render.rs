//! Page rasterization on a pool of worker threads (CPU, via `typst-render`).

use std::collections::HashSet;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use typst::utils::Scalar;
use typst_layout::PagedDocument;
use typst_render::RenderOptions;
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

struct Job {
    key: RenderKey,
    doc: Arc<PagedDocument>,
    index: usize,
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
                            let _ = proxy.send_event(UiEvent::RenderSkipped(job.key));
                            continue;
                        }
                        let Some(page) = job.doc.pages().get(job.index) else { continue };
                        let t = std::time::Instant::now();
                        let mut bitmap = render_page(page, job.key.scale(), job.key.invert);
                        if opaque_alpha {
                            bitmap.pixels.iter_mut().for_each(|p| *p |= 0xff00_0000);
                        }
                        log::trace!("rendered page {} in {:?}", job.index + 1, t.elapsed());
                        if !proxy.send_event(UiEvent::Rendered(job.key, Arc::new(bitmap))) {
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

    pub fn submit(&self, key: RenderKey, doc: Arc<PagedDocument>, index: usize) {
        let _ = self.tx.send(Job { key, doc, index });
    }
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
    fn invert_black_white() {
        assert_eq!(invert_lightness(255, 255, 255), 0x000000);
        assert_eq!(invert_lightness(0, 0, 0), 0xffffff);
    }
}
