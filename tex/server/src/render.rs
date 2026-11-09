//! Page rasterization on worker threads, answering the pane's `render`
//! requests with `bitmap` strips.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crossbeam_channel::Sender;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::pdf::{self, Bitmap, PageCache, PdfDoc};
use crate::zed::ZedOut;

/// Page hash + pixels per point × 1000 + inverted.
pub type Key = (u128, u32, bool);

pub struct Job {
    pub doc: Arc<PdfDoc>,
    pub index: usize,
    pub key: Key,
    /// `(strip_px, first, end)`, or `None` for the whole page.
    pub strips: Option<(u32, u32, u32)>,
}

pub struct RenderPool {
    tx: Sender<Job>,
    /// `(hash, scale_milli)` the pane still wants: other jobs are skipped.
    wanted: Arc<Mutex<HashSet<(u128, u32)>>>,
}

fn skipped(key: Key, strips: Option<(u32, u32, u32)>) -> Value {
    let mut v = json!({ "type": "skipped", "hash": format!("{:x}", key.0), "scale_milli": key.1 });
    if let Some((_, first, end)) = strips {
        v["first"] = first.into();
        v["end"] = end.into();
    }
    v
}

impl RenderPool {
    pub fn new(zed: ZedOut) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<Job>();
        let wanted: Arc<Mutex<HashSet<(u128, u32)>>> = Arc::default();
        let cache = Arc::new(PageCache::default());
        // One lock per page being rendered: requests for other strips of the
        // same page wait for it instead of rendering it again.
        let busy: Arc<Mutex<HashMap<Key, Arc<Mutex<()>>>>> = Arc::default();
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 6);
        for i in 0..threads {
            let (rx, wanted, cache, busy, zed) = (rx.clone(), wanted.clone(), cache.clone(), busy.clone(), zed.clone());
            std::thread::Builder::new()
                .name(format!("render-{i}"))
                .stack_size(16 << 20)
                .spawn(move || {
                    for job in rx {
                        if !wanted.lock().contains(&(job.key.0, job.key.1)) {
                            zed.send(skipped(job.key, job.strips));
                            continue;
                        }
                        let Some(bitmap) = page_bitmap(&job, &cache, &busy) else {
                            zed.send(skipped(job.key, job.strips));
                            continue;
                        };
                        send_bitmap(&zed, job.key, job.strips, bitmap);
                    }
                })
                .expect("spawn render thread");
        }
        Self { tx, wanted }
    }

    pub fn set_wanted(&self, keys: HashSet<(u128, u32)>) {
        *self.wanted.lock() = keys;
    }

    pub fn submit(&self, job: Job) {
        let _ = self.tx.send(job);
    }
}

fn page_bitmap(job: &Job, cache: &PageCache, busy: &Mutex<HashMap<Key, Arc<Mutex<()>>>>) -> Option<Arc<Bitmap>> {
    if let Some(b) = cache.get(job.key) {
        return Some(b);
    }
    let lock = busy.lock().entry(job.key).or_default().clone();
    let _guard = lock.lock();
    let result = match cache.get(job.key) {
        Some(b) => Some(b),
        None => {
            let t = std::time::Instant::now();
            let bitmap = pdf::render(&job.doc, job.index, job.key.1, job.key.2).map(Arc::new);
            log::debug!("rendered page {} at {} in {:?}", job.index + 1, job.key.1, t.elapsed());
            if let Some(b) = &bitmap {
                cache.insert(job.key, b.clone());
            }
            bitmap
        }
    };
    busy.lock().remove(&job.key);
    result
}

fn send_bitmap(zed: &ZedOut, key: Key, strips: Option<(u32, u32, u32)>, bitmap: Arc<Bitmap>) {
    let header = |height: u32| {
        json!({
            "type": "bitmap",
            "hash": format!("{:x}", key.0),
            "scale_milli": key.1,
            "width": bitmap.width,
            "height": height,
        })
    };
    match strips {
        None => zed.send_rows(header(bitmap.height), bitmap.clone(), 0..bitmap.height),
        Some((strip_px, first, end)) => {
            for k in first..end {
                let start = k * strip_px;
                let stop = (start + strip_px).min(bitmap.height);
                if start >= stop {
                    break;
                }
                let mut h = header(stop - start);
                h["strip"] = k.into();
                h["strip_px"] = strip_px.into();
                zed.send_rows(h, bitmap.clone(), start..stop);
            }
        }
    }
}
