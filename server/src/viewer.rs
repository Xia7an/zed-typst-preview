//! The native preview window (winit + softbuffer, no web technology).
//!
//! Pages are rasterized on worker threads and blitted into the window
//! framebuffer. While a changed page is re-rendering, its previous bitmap stays
//! on screen, so updates never flicker.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use typst_layout::PagedDocument;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId, WindowLevel};

use crate::config::Config;
use crate::core::CoreMsg;
use crate::render::{Bitmap, RenderKey, RenderPool};

#[derive(Clone, Debug)]
pub struct PageInfo {
    pub width: f64,
    pub height: f64,
    pub hash: u128,
}

pub struct DocUpdate {
    pub doc: Arc<PagedDocument>,
    pub pages: Vec<PageInfo>,
    pub generation: u64,
    pub status: Status,
    /// When the edit that led to this document arrived.
    pub edit_received: Option<Instant>,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    /// Number of errors and the first message.
    pub error: Option<(usize, String)>,
    pub warnings: usize,
    pub compile_ms: f64,
}

/// A position in the document (pt). `height` is the font size, if known.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Marker {
    pub page: usize,
    pub x: f64,
    pub y: f64,
    pub height: f64,
}

/// Where core and render threads send their events: the native window's
/// event loop, or the bridge to Zed's built-in preview pane.
#[derive(Clone)]
pub struct UiSender(Arc<dyn Fn(UiEvent) -> bool + Send + Sync>);

impl UiSender {
    pub fn new(f: impl Fn(UiEvent) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn from_proxy(proxy: EventLoopProxy<UiEvent>) -> Self {
        Self::new(move |e| proxy.send_event(e).is_ok())
    }

    /// Returns `false` if the receiver is gone.
    pub fn send_event(&self, event: UiEvent) -> bool {
        (self.0)(event)
    }
}

pub enum UiEvent {
    /// Open the preview for `path`.
    Show { title: String, cfg: Config, path: Option<std::path::PathBuf> },
    /// The previewed file changed.
    Title { title: String, path: std::path::PathBuf },
    /// Workspace roots of this server (used by Zed to route panes).
    Roots(Vec<std::path::PathBuf>),
    /// Move the editor cursor (Zed pane mode only).
    Jump { path: std::path::PathBuf, line: u32, character: u32 },
    Document(DocUpdate),
    Status(Status),
    /// Cursor moved in the editor: show the indicator, scroll if hidden.
    Cursor(Vec<Marker>),
    /// Like `Cursor`, but never scrolls (used after clicking the preview).
    CursorNoScroll(Vec<Marker>),
    /// Scroll to a position and flash it (internal links).
    Reveal(Marker),
    ActivateEditor,
    Config(Config),
    Rendered(RenderKey, Arc<Bitmap>),
    RenderSkipped(RenderKey),
    CoreReady(Sender<CoreMsg>),
    Exit,
}

const MARGIN: f64 = 16.0; // logical px
const GAP: f64 = 12.0; // logical px
const CACHE_BUDGET: usize = 384 * 1024 * 1024;
const FLASH: Duration = Duration::from_millis(900);

struct WindowState {
    window: Arc<Window>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    size: (u32, u32),
}

struct CacheEntry {
    bitmap: Arc<Bitmap>,
    used: u64,
}

pub struct Viewer {
    proxy: EventLoopProxy<UiEvent>,
    core: Option<Sender<CoreMsg>>,
    win: Option<WindowState>,
    title: String,
    cfg: Config,
    pool: RenderPool,

    doc: Option<Arc<PagedDocument>>,
    pages: Vec<PageInfo>,
    generation: u64,
    status: Status,

    dpr: f64,
    /// Logical pixels per point.
    zoom: f64,
    fit_width: bool,
    scroll: (f64, f64), // physical px

    cache: HashMap<RenderKey, CacheEntry>,
    cache_bytes: usize,
    tick: u64,
    in_flight: HashSet<RenderKey>,
    /// Bitmap most recently shown for each page index (stale fallback).
    shown: Vec<Option<Arc<Bitmap>>>,

    cursor_markers: Vec<Marker>,
    flash: Option<(Marker, Instant)>,
    pending_reveal: Option<Marker>,

    mouse: (f64, f64),
    press: Option<((f64, f64), (f64, f64))>, // (press position, scroll at press)
    dragging: bool,
    mods: ModifiersState,
    test_click_done: bool,
    /// Edit whose result is not fully on screen yet.
    latency_probe: Option<Instant>,
    /// A new document is waiting for its pages to render before presenting.
    waiting_since: Option<Instant>,
}

impl Viewer {
    pub fn new(proxy: EventLoopProxy<UiEvent>, core: Option<Sender<CoreMsg>>, cfg: Config) -> Self {
        let pool = RenderPool::new(UiSender::from_proxy(proxy.clone()), false);
        Self {
            proxy,
            core,
            win: None,
            title: "Typst Live Preview".into(),
            cfg,
            pool,
            doc: None,
            pages: Vec::new(),
            generation: 0,
            status: Status::default(),
            dpr: 1.0,
            zoom: 1.0,
            fit_width: true,
            scroll: (0.0, 0.0),
            cache: HashMap::new(),
            cache_bytes: 0,
            tick: 0,
            in_flight: HashSet::new(),
            shown: Vec::new(),
            cursor_markers: Vec::new(),
            flash: None,
            pending_reveal: None,
            mouse: (0.0, 0.0),
            press: None,
            dragging: false,
            mods: ModifiersState::empty(),
            test_click_done: false,
            latency_probe: None,
            waiting_since: None,
        }
    }

    fn send(&self, msg: CoreMsg) {
        if let Some(core) = &self.core {
            let _ = core.send(msg);
        }
    }

    // ---------------------------------------------------------------- layout

    fn scale(&self) -> f64 {
        self.zoom * self.dpr
    }

    fn view_size(&self) -> (f64, f64) {
        self.win.as_ref().map(|w| (w.size.0 as f64, w.size.1 as f64)).unwrap_or((1.0, 1.0))
    }

    fn max_page_width(&self) -> f64 {
        self.pages.iter().map(|p| p.width).fold(0.0, f64::max).max(1.0)
    }

    fn fit_zoom(&self) -> f64 {
        let (w, _) = self.view_size();
        ((w / self.dpr - 2.0 * MARGIN) / self.max_page_width()).clamp(0.05, 20.0)
    }

    /// Page rectangles in content coordinates (physical px), plus the content
    /// size.
    fn layout(&self) -> (Vec<[f64; 4]>, (f64, f64)) {
        let s = self.scale();
        let (vw, _) = self.view_size();
        let margin = MARGIN * self.dpr;
        let gap = GAP * self.dpr;
        let content_w = vw.max((self.max_page_width() * s).round() + 2.0 * margin);
        let mut y = margin;
        let mut rects = Vec::with_capacity(self.pages.len());
        for p in &self.pages {
            let (w, h) = ((p.width * s).round().max(1.0), (p.height * s).round().max(1.0));
            let x = ((content_w - w) / 2.0).round();
            rects.push([x, y, w, h]);
            y += h + gap;
        }
        let content_h = if rects.is_empty() { 0.0 } else { y - gap + margin };
        (rects, (content_w, content_h))
    }

    fn clamp_scroll(&mut self) {
        let (_, (cw, ch)) = self.layout();
        let (vw, vh) = self.view_size();
        self.scroll.0 = self.scroll.0.clamp(0.0, (cw - vw).max(0.0));
        self.scroll.1 = self.scroll.1.clamp(0.0, (ch - vh).max(0.0));
    }

    /// Maps a window position (physical px) to a page and a point on it (pt).
    fn hit(&self, pos: (f64, f64)) -> Option<(usize, f64, f64)> {
        let (rects, _) = self.layout();
        let (cx, cy) = (pos.0 + self.scroll.0, pos.1 + self.scroll.1);
        let s = self.scale();
        rects.iter().enumerate().find_map(|(i, r)| {
            (cx >= r[0] && cx < r[0] + r[2] && cy >= r[1] && cy < r[1] + r[3])
                .then(|| (i, (cx - r[0]) / s, (cy - r[1]) / s))
        })
    }

    /// Content coordinates (physical px) of a point on a page.
    fn content_pos(&self, page: usize, x: f64, y: f64) -> Option<(f64, f64)> {
        let (rects, _) = self.layout();
        let r = rects.get(page)?;
        let s = self.scale();
        Some((r[0] + x * s, r[1] + y * s))
    }

    fn set_zoom(&mut self, zoom: f64, anchor: (f64, f64)) {
        let zoom = zoom.clamp(0.05, 20.0);
        let before = self.hit(anchor);
        let ratio = zoom / self.zoom;
        let old_scroll = self.scroll;
        self.zoom = zoom;
        self.fit_width = false;
        match before.and_then(|(p, x, y)| self.content_pos(p, x, y)) {
            Some((cx, cy)) => self.scroll = (cx - anchor.0, cy - anchor.1),
            None => {
                self.scroll = ((old_scroll.0 + anchor.0) * ratio - anchor.0, (old_scroll.1 + anchor.1) * ratio - anchor.1)
            }
        }
        self.clamp_scroll();
        self.redraw();
    }

    fn set_fit_width(&mut self) {
        let (_, vh) = self.view_size();
        // Keep the point at the top-center of the view in place.
        let anchor = (self.view_size().0 / 2.0, vh.min(1.0));
        let before = self.hit(anchor);
        self.fit_width = true;
        self.zoom = self.fit_zoom();
        if let Some((cx, cy)) = before.and_then(|(p, x, y)| self.content_pos(p, x, y)) {
            self.scroll.1 = cy - anchor.1;
            let _ = cx;
        }
        self.scroll.0 = 0.0;
        self.clamp_scroll();
        self.redraw();
    }

    /// Scrolls so that `m` is visible. With `center`, always centers it.
    fn reveal(&mut self, m: Marker, center: bool) -> bool {
        let Some((cx, cy)) = self.content_pos(m.page, m.x, m.y) else { return false };
        let (vw, vh) = self.view_size();
        let h = m.height.max(10.0) * self.scale();
        let (top, bottom) = (cy - h, cy + h * 0.3);
        let visible_y = top >= self.scroll.1 + vh * 0.05 && bottom <= self.scroll.1 + vh * 0.95;
        let visible_x = cx >= self.scroll.0 && cx <= self.scroll.0 + vw;
        if center || !visible_y {
            self.scroll.1 = cy - vh / 2.0;
        }
        if center || !visible_x {
            self.scroll.0 = cx - vw / 2.0;
        }
        self.clamp_scroll();
        center || !visible_y || !visible_x
    }

    // --------------------------------------------------------------- drawing

    fn redraw(&self) {
        if let Some(w) = &self.win {
            w.window.request_redraw();
        }
    }

    fn key_for(&self, i: usize) -> RenderKey {
        RenderKey {
            hash: self.pages[i].hash,
            scale_milli: (self.scale() * 1000.0).round() as u32,
            invert: self.cfg.invert_colors,
        }
    }

    fn draw(&mut self) {
        if self.doc.is_none() {
            self.present_empty();
            return;
        }
        let draw_start = Instant::now();
        self.waiting_since = None;
        self.schedule_renders();
        let (rects, _) = self.layout();
        let (vw, _) = self.view_size();
        let (sx, sy) = self.scroll;
        let visible = self.visible_pages();

        if self.shown.len() != self.pages.len() {
            self.shown.resize(self.pages.len(), None);
        }
        let mut items: Vec<([f64; 4], Option<Arc<Bitmap>>)> = Vec::new();
        let mut exact_all = true;
        for &i in &visible {
            let key = self.key_for(i);
            exact_all &= self.cache.contains_key(&key);
            let bitmap = match self.cache.get(&key) {
                Some(e) => {
                    self.shown[i] = Some(e.bitmap.clone());
                    Some(e.bitmap.clone())
                }
                None => self.shown[i].clone().or_else(|| {
                    // Same page at another zoom level.
                    self.cache.iter().find(|(k, _)| k.hash == key.hash).map(|(_, e)| e.bitmap.clone())
                }),
            };
            let r = rects[i];
            items.push(([r[0] - sx, r[1] - sy, r[2], r[3]], bitmap));
        }

        let s = self.scale();
        let bg = self.cfg.background_rgb();
        let blank = if self.cfg.invert_colors { 0x000000 } else { 0xffffff };
        let markers: Vec<[f64; 4]> = self
            .cursor_markers
            .iter()
            .filter_map(|m| {
                let r = rects.get(m.page)?;
                let size = if m.height > 0.0 { m.height } else { 11.0 } * s;
                let w = (2.0 * self.dpr).max(s * 0.8).round();
                Some([r[0] + m.x * s - sx - w / 2.0, r[1] + m.y * s - sy - size * 0.85, w, size * 1.1])
            })
            .collect();
        let flash = self.flash.and_then(|(m, t)| {
            let age = t.elapsed();
            if age > FLASH {
                return None;
            }
            let r = rects.get(m.page)?;
            let size = if m.height > 0.0 { m.height } else { 14.0 } * s;
            let alpha = 1.0 - age.as_secs_f64() / FLASH.as_secs_f64();
            Some(([r[0] - sx, r[1] + m.y * s - sy - size, r[2], size * 1.6], alpha))
        });
        let error = self.status.error.is_some();
        let dpr = self.dpr;

        let Some(win) = self.win.as_mut() else { return };
        let (w, h) = win.size;
        let t_buf = Instant::now();
        let Ok(mut buf) = win.surface.buffer_mut() else { return };
        let t_fill = Instant::now();
        let (w, h) = (w as usize, h as usize);
        buf.fill(bg);
        for (r, bitmap) in &items {
            let outline = darken(bg);
            fill_rect(&mut buf, w, h, [r[0] - 1.0, r[1] - 1.0, r[2] + 2.0, r[3] + 2.0], outline, 1.0);
            match bitmap {
                Some(b) => blit(&mut buf, w, h, *r, b),
                None => fill_rect(&mut buf, w, h, *r, blank, 1.0),
            }
        }
        if let Some((r, a)) = flash {
            fill_rect(&mut buf, w, h, r, 0xffd23f, 0.35 * a);
        }
        for r in &markers {
            fill_rect(&mut buf, w, h, *r, 0x2f80ed, 0.85);
        }
        if error {
            fill_rect(&mut buf, w, h, [0.0, 0.0, vw, (3.0 * dpr).round()], 0xe5484d, 1.0);
        }
        if let Some(path) = std::env::var_os("TYPST_LIVEPREVIEW_DUMP") {
            static DUMPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            let once = std::env::var_os("TYPST_LIVEPREVIEW_DUMP_ONCE").is_some();
            if !(once && DUMPED.swap(true, std::sync::atomic::Ordering::Relaxed)) {
                dump_png(&buf, w, h, std::path::Path::new(&path));
            }
        }
        let t_present = Instant::now();
        let _ = buf.present();
        log::trace!(
            "draw: buffer_mut {:.2} ms, compose {:.2} ms, present {:.2} ms",
            (t_fill - t_buf).as_secs_f64() * 1e3,
            (t_present - t_fill).as_secs_f64() * 1e3,
            t_present.elapsed().as_secs_f64() * 1e3
        );
        if let Some(t) = self.latency_probe {
            if items.iter().all(|(_, b)| b.is_some()) && exact_all {
                self.latency_probe = None;
                log::debug!(
                    "edit → on screen: {:.2} ms (draw {:.2} ms)",
                    t.elapsed().as_secs_f64() * 1000.0,
                    draw_start.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
        // Debugging aid: synthesize one click at "x,y" (window px) once the
        // pages are drawn.
        if !self.test_click_done && items.iter().any(|(_, b)| b.is_some()) {
            if let Some((x, y)) = std::env::var("TYPST_LIVEPREVIEW_TEST_CLICK")
                .ok()
                .and_then(|v| v.split_once(',').map(|(a, b)| (a.trim().parse().ok(), b.trim().parse().ok())))
                .and_then(|(a, b)| Some((a?, b?)))
            {
                self.test_click_done = true;
                if let Some((page, x, y)) = self.hit((x, y)) {
                    self.send(CoreMsg::Click { page, x, y });
                }
            }
        }
    }

    fn present_empty(&mut self) {
        let bg = self.cfg.background_rgb();
        let Some(win) = self.win.as_mut() else { return };
        if let Ok(mut buf) = win.surface.buffer_mut() {
            buf.fill(bg);
            let _ = buf.present();
        }
    }

    fn evict(&mut self, wanted: &HashSet<RenderKey>) {
        if self.cache_bytes <= CACHE_BUDGET {
            return;
        }
        let mut entries: Vec<(RenderKey, u64)> =
            self.cache.iter().filter(|(k, _)| !wanted.contains(k)).map(|(k, e)| (*k, e.used)).collect();
        entries.sort_by_key(|&(_, used)| used);
        for (k, _) in entries {
            if self.cache_bytes <= CACHE_BUDGET * 3 / 4 {
                break;
            }
            if let Some(e) = self.cache.remove(&k) {
                self.cache_bytes -= e.bitmap.bytes();
            }
        }
    }

    fn update_title(&self) {
        let Some(win) = &self.win else { return };
        let status = match &self.status.error {
            Some((n, msg)) => {
                let msg: String = msg.chars().take(120).collect();
                format!("  ⚠ {n} error{}: {msg}", if *n == 1 { "" } else { "s" })
            }
            None if self.doc.is_some() => {
                let warn = match self.status.warnings {
                    0 => String::new(),
                    n => format!("  ·  {n} warning{}", if n == 1 { "" } else { "s" }),
                };
                format!("  ·  {} pages  ·  {:.0} ms{warn}", self.pages.len(), self.status.compile_ms)
            }
            None => String::new(),
        };
        win.window.set_title(&format!("{}{}", self.title, status));
    }

    // ---------------------------------------------------------------- window

    fn open_window(&mut self, el: &ActiveEventLoop) {
        if let Some(w) = &self.win {
            w.window.set_visible(true);
            w.window.focus_window();
            return;
        }
        let mut attrs = Window::default_attributes()
            .with_title(&self.title)
            .with_window_level(if self.cfg.always_on_top { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal });
        // Place it like a split view: right side of the screen.
        if let Some(m) = el.primary_monitor().or_else(|| el.available_monitors().next()) {
            let sf = m.scale_factor();
            let size = m.size().to_logical::<f64>(sf);
            let origin = m.position().to_logical::<f64>(sf);
            let width = (size.width * 0.45).max(480.0);
            let height = (size.height - 90.0).max(480.0);
            attrs = attrs
                .with_inner_size(LogicalSize::new(width, height))
                .with_position(LogicalPosition::new(origin.x + size.width - width - 8.0, origin.y + 40.0));
        } else {
            attrs = attrs.with_inner_size(LogicalSize::new(720.0, 960.0));
        }
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log::error!("cannot create window: {e}");
                return;
            }
        };
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                log::error!("softbuffer context: {e}");
                return;
            }
        };
        let surface = match softbuffer::Surface::new(&context, window.clone()) {
            Ok(s) => s,
            Err(e) => {
                log::error!("softbuffer surface: {e}");
                return;
            }
        };
        self.dpr = window.scale_factor();
        let size = window.inner_size();
        self.win = Some(WindowState { window, surface, size: (0, 0) });
        self.resize(size.width, size.height);
        self.update_title();
    }

    fn close_window(&mut self) {
        self.win = None;
        self.cache.clear();
        self.cache_bytes = 0;
        self.shown.clear();
        self.in_flight.clear();
        self.pool.set_wanted(HashSet::new());
        self.send(CoreMsg::WindowClosed);
    }

    fn resize(&mut self, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        if let Some(win) = self.win.as_mut() {
            win.size = (w, h);
            let _ = win.surface.resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap());
        }
        if self.fit_width {
            self.zoom = self.fit_zoom();
        }
        self.clamp_scroll();
        self.redraw();
    }

    fn scroll_by(&mut self, dx: f64, dy: f64) {
        self.scroll.0 += dx;
        self.scroll.1 += dy;
        self.clamp_scroll();
        self.redraw();
    }

    fn on_key(&mut self, el: &ActiveEventLoop, key: &Key) {
        let cmd = self.mods.super_key() || self.mods.control_key();
        let (_, vh) = self.view_size();
        let line = 48.0 * self.dpr;
        let center = (self.view_size().0 / 2.0, vh / 2.0);
        match key {
            Key::Character(c) if cmd => match c.as_str() {
                "=" | "+" | ";" => self.set_zoom(self.zoom * 1.2, center),
                "-" => self.set_zoom(self.zoom / 1.2, center),
                "0" => self.set_fit_width(),
                "w" => {
                    self.close_window();
                    let _ = el;
                }
                _ => {}
            },
            Key::Named(NamedKey::ArrowDown) => self.scroll_by(0.0, line),
            Key::Named(NamedKey::ArrowUp) => self.scroll_by(0.0, -line),
            Key::Named(NamedKey::ArrowLeft) => self.scroll_by(-line, 0.0),
            Key::Named(NamedKey::ArrowRight) => self.scroll_by(line, 0.0),
            Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => {
                let d = if self.mods.shift_key() { -vh * 0.9 } else { vh * 0.9 };
                self.scroll_by(0.0, d)
            }
            Key::Named(NamedKey::PageUp) => self.scroll_by(0.0, -vh * 0.9),
            Key::Named(NamedKey::Home) => self.scroll_by(0.0, f64::NEG_INFINITY.max(-1e12)),
            Key::Named(NamedKey::End) => self.scroll_by(0.0, 1e12),
            _ => {}
        }
    }

    fn on_document(&mut self, update: DocUpdate) {
        self.generation = update.generation;
        if let Some(t) = update.edit_received {
            self.latency_probe.get_or_insert(t);
        }
        let first = self.doc.is_none();
        self.doc = Some(update.doc);
        self.pages = update.pages;
        self.status = update.status;
        if self.shown.len() > self.pages.len() {
            self.shown.truncate(self.pages.len());
        }
        if self.fit_width || first {
            if first {
                self.fit_width = true;
            }
            self.zoom = self.fit_zoom();
        }
        self.clamp_scroll();
        if let Some(m) = self.pending_reveal.take() {
            self.reveal(m, true);
        }
        self.update_title();
        // Changed pages usually render in a few ms: present once they are
        // ready instead of drawing the stale bitmaps first (saves a frame).
        if self.schedule_renders() == 0 || first {
            self.redraw();
        } else {
            self.waiting_since = Some(Instant::now());
        }
    }

    /// Indices of pages intersecting the view.
    fn visible_pages(&self) -> Vec<usize> {
        let (rects, _) = self.layout();
        let (_, vh) = self.view_size();
        let sy = self.scroll.1;
        (0..rects.len()).filter(|&i| rects[i][1] + rects[i][3] > sy && rects[i][1] < sy + vh).collect()
    }

    /// Submits render jobs for visible pages (+1 page on each side).
    /// Returns how many visible pages are still being rendered.
    fn schedule_renders(&mut self) -> usize {
        let Some(doc) = self.doc.clone() else { return 0 };
        let visible = self.visible_pages();
        let mut wanted_idx = visible.clone();
        if let (Some(&first), Some(&last)) = (visible.first(), visible.last()) {
            if first > 0 {
                wanted_idx.push(first - 1);
            }
            if last + 1 < self.pages.len() {
                wanted_idx.push(last + 1);
            }
        }
        self.tick += 1;
        // Publish the wanted set before submitting, so workers never drop a
        // job that is still needed.
        let wanted: HashSet<RenderKey> = wanted_idx.iter().map(|&i| self.key_for(i)).collect();
        self.pool.set_wanted(wanted.clone());
        for &i in &wanted_idx {
            let key = self.key_for(i);
            if let Some(e) = self.cache.get_mut(&key) {
                e.used = self.tick;
            } else if !self.in_flight.contains(&key) {
                self.in_flight.insert(key);
                self.pool.submit(key, doc.clone(), i);
            }
        }
        self.evict(&wanted);
        visible.iter().filter(|&&i| self.in_flight.contains(&self.key_for(i))).count()
    }

    fn reset_document(&mut self) {
        self.doc = None;
        self.pages.clear();
        self.shown.clear();
        self.scroll = (0.0, 0.0);
        self.cursor_markers.clear();
        self.status = Status::default();
    }
}

impl ApplicationHandler<UiEvent> for Viewer {
    fn resumed(&mut self, _el: &ActiveEventLoop) {}

    fn user_event(&mut self, el: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::Roots(_) | UiEvent::Jump { .. } => {}
            UiEvent::Show { title, cfg, .. } => {
                self.title = title;
                self.cfg = cfg;
                self.open_window(el);
            }
            UiEvent::Title { title, .. } => {
                if title != self.title {
                    self.title = title;
                    self.reset_document();
                    self.update_title();
                    self.redraw();
                }
            }
            UiEvent::Document(update) => self.on_document(update),
            UiEvent::Status(status) => {
                self.status = status;
                self.update_title();
                self.redraw();
            }
            UiEvent::Cursor(markers) | UiEvent::CursorNoScroll(markers) if self.doc.is_none() => {
                self.cursor_markers = markers;
            }
            UiEvent::Cursor(markers) => {
                // Prefer the occurrence closest to what is currently shown.
                let (_, vh) = self.view_size();
                let mid = self.scroll.1 + vh / 2.0;
                let best = markers
                    .iter()
                    .copied()
                    .min_by(|a, b| {
                        let da = self.content_pos(a.page, a.x, a.y).map_or(f64::MAX, |p| (p.1 - mid).abs());
                        let db = self.content_pos(b.page, b.x, b.y).map_or(f64::MAX, |p| (p.1 - mid).abs());
                        da.total_cmp(&db)
                    });
                if let Some(m) = best {
                    self.reveal(m, false);
                }
                if self.cursor_markers != markers {
                    self.cursor_markers = markers;
                }
                self.redraw();
            }
            UiEvent::CursorNoScroll(markers) => {
                self.cursor_markers = markers;
                self.redraw();
            }
            UiEvent::Reveal(m) => {
                if self.doc.is_none() {
                    self.pending_reveal = Some(m);
                } else {
                    self.reveal(m, true);
                    self.flash = Some((m, Instant::now()));
                    self.redraw();
                }
            }
            UiEvent::ActivateEditor => activate_editor(),
            UiEvent::Config(cfg) => {
                let invert_changed = cfg.invert_colors != self.cfg.invert_colors;
                if let Some(w) = &self.win {
                    w.window.set_window_level(if cfg.always_on_top { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal });
                }
                self.cfg = cfg;
                if invert_changed {
                    self.shown.iter_mut().for_each(|s| *s = None);
                }
                self.redraw();
            }
            UiEvent::Rendered(key, bitmap) => {
                self.in_flight.remove(&key);
                if self.win.is_none() {
                    return;
                }
                self.cache_bytes += bitmap.bytes();
                if let Some(old) = self.cache.insert(key, CacheEntry { bitmap, used: self.tick }) {
                    self.cache_bytes -= old.bitmap.bytes();
                }
                let pending = self.visible_pages().iter().any(|&i| self.in_flight.contains(&self.key_for(i)));
                if !pending {
                    // Draw right away rather than waiting for the next
                    // display refresh callback.
                    self.draw();
                }
            }
            UiEvent::RenderSkipped(key) => {
                // Re-request it if it became visible again in the meantime.
                if self.in_flight.remove(&key) {
                    self.redraw();
                }
            }
            UiEvent::CoreReady(core) => self.core = Some(core),
            UiEvent::Exit => el.exit(),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.close_window(),
            WindowEvent::Resized(size) => self.resize(size.width, size.height),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.dpr = scale_factor;
                if self.fit_width {
                    self.zoom = self.fit_zoom();
                }
                self.redraw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.on_key(el, &event.logical_key)
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as f64 * 48.0 * self.dpr, y as f64 * 48.0 * self.dpr),
                    MouseScrollDelta::PixelDelta(PhysicalPosition { x, y }) => (x, y),
                };
                if self.mods.super_key() || self.mods.control_key() {
                    let factor = (dy / (120.0 * self.dpr)).exp();
                    self.set_zoom(self.zoom * factor, self.mouse);
                } else {
                    self.scroll_by(-dx, -dy);
                }
            }
            WindowEvent::PinchGesture { delta, phase, .. } => {
                if matches!(phase, TouchPhase::Moved | TouchPhase::Started) {
                    self.set_zoom(self.zoom * (1.0 + delta), self.mouse);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse = (position.x, position.y);
                if let Some((p, s)) = self.press {
                    let (dx, dy) = (position.x - p.0, position.y - p.1);
                    if self.dragging || dx.abs() + dy.abs() > 5.0 * self.dpr {
                        self.dragging = true;
                        self.scroll = (s.0 - dx, s.1 - dy);
                        self.clamp_scroll();
                        self.redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => {
                    self.press = Some((self.mouse, self.scroll));
                    self.dragging = false;
                }
                ElementState::Released => {
                    let was_drag = self.dragging;
                    self.press = None;
                    self.dragging = false;
                    if !was_drag {
                        if let Some((page, x, y)) = self.hit(self.mouse) {
                            self.send(CoreMsg::Click { page, x, y });
                        }
                    }
                }
            },
            WindowEvent::Focused(true) => self.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if let Some(t) = self.waiting_since {
            // Slow page: show the layout change with stale bitmaps meanwhile.
            let deadline = t + Duration::from_millis(50);
            if Instant::now() >= deadline {
                self.waiting_since = None;
                self.redraw();
            } else {
                el.set_control_flow(ControlFlow::WaitUntil(deadline));
                return;
            }
        }
        match self.flash {
            Some((_, t)) if t.elapsed() <= FLASH => {
                self.redraw();
                el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16)));
            }
            Some(_) => {
                self.flash = None;
                self.redraw();
                el.set_control_flow(ControlFlow::Wait);
            }
            None => el.set_control_flow(ControlFlow::Wait),
        }
        let _ = &self.proxy;
    }
}

// ---------------------------------------------------------------- pixel ops

fn darken(c: u32) -> u32 {
    let f = |v: u32| (v * 3 / 4) & 0xff;
    (f(c >> 16) << 16) | (f((c >> 8) & 0xff) << 8) | f(c & 0xff)
}

/// Clips a rectangle to the buffer and returns integer bounds.
fn clip(r: [f64; 4], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let x0 = r[0].round().max(0.0) as i64;
    let y0 = r[1].round().max(0.0) as i64;
    let x1 = ((r[0] + r[2]).round() as i64).min(w as i64);
    let y1 = ((r[1] + r[3]).round() as i64).min(h as i64);
    (x1 > x0 && y1 > y0).then(|| (x0 as usize, y0 as usize, x1 as usize, y1 as usize))
}

fn fill_rect(buf: &mut [u32], w: usize, h: usize, r: [f64; 4], color: u32, alpha: f64) {
    let Some((x0, y0, x1, y1)) = clip(r, w, h) else { return };
    if alpha >= 1.0 {
        for y in y0..y1 {
            buf[y * w + x0..y * w + x1].fill(color);
        }
        return;
    }
    let a = (alpha.clamp(0.0, 1.0) * 256.0) as u32;
    let (cr, cg, cb) = ((color >> 16) & 0xff, (color >> 8) & 0xff, color & 0xff);
    for y in y0..y1 {
        for px in &mut buf[y * w + x0..y * w + x1] {
            let (r, g, b) = ((*px >> 16) & 0xff, (*px >> 8) & 0xff, *px & 0xff);
            let mix = |d: u32, s: u32| (d * (256 - a) + s * a) >> 8;
            *px = (mix(r, cr) << 16) | (mix(g, cg) << 8) | mix(b, cb);
        }
    }
}

/// Copies a bitmap into the rectangle `r` (screen px), scaling with nearest
/// neighbour if the sizes differ (only while a re-render is pending).
fn blit(buf: &mut [u32], w: usize, h: usize, r: [f64; 4], b: &Bitmap) {
    let Some((x0, y0, x1, y1)) = clip(r, w, h) else { return };
    let (rx, ry) = (r[0].round() as i64, r[1].round() as i64);
    let (bw, bh) = (b.width as usize, b.height as usize);
    let exact = (r[2].round() as usize, r[3].round() as usize) == (bw, bh);
    if exact {
        for y in y0..y1 {
            let sy = (y as i64 - ry) as usize;
            if sy >= bh {
                break;
            }
            let sx0 = (x0 as i64 - rx) as usize;
            let n = (x1 - x0).min(bw.saturating_sub(sx0));
            buf[y * w + x0..y * w + x0 + n].copy_from_slice(&b.pixels[sy * bw + sx0..sy * bw + sx0 + n]);
        }
    } else {
        let fx = bw as f64 / r[2];
        let fy = bh as f64 / r[3];
        let xs: Vec<usize> =
            (x0..x1).map(|x| (((x as f64 - r[0]) * fx) as usize).min(bw - 1)).collect();
        for y in y0..y1 {
            let sy = (((y as f64 - r[1]) * fy) as usize).min(bh - 1);
            let src = &b.pixels[sy * bw..(sy + 1) * bw];
            let dst = &mut buf[y * w + x0..y * w + x1];
            for (d, &sx) in dst.iter_mut().zip(&xs) {
                *d = src[sx];
            }
        }
    }
}

/// Debugging aid: writes the framebuffer to a PNG file.
fn dump_png(buf: &[u32], w: usize, h: usize, path: &std::path::Path) {
    let Some(mut pixmap) = tiny_skia::Pixmap::new(w as u32, h as u32) else { return };
    for (dst, &px) in pixmap.data_mut().chunks_exact_mut(4).zip(buf) {
        dst.copy_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, px as u8, 255]);
    }
    let _ = pixmap.save_png(path);
}

/// Brings the editor (our parent process) to the front.
fn activate_editor() {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
        let pid = std::os::unix::process::parent_id() as i32;
        if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
            #[allow(deprecated)]
            app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_exact_and_clipped() {
        let b = Bitmap { width: 2, height: 2, pixels: vec![1, 2, 3, 4] };
        let mut buf = vec![0u32; 9];
        blit(&mut buf, 3, 3, [1.0, 1.0, 2.0, 2.0], &b);
        assert_eq!(buf, vec![0, 0, 0, 0, 1, 2, 0, 3, 4]);
        let mut buf = vec![0u32; 4];
        blit(&mut buf, 2, 2, [-1.0, -1.0, 2.0, 2.0], &b);
        assert_eq!(buf, vec![4, 0, 0, 0]);
    }

    #[test]
    fn blit_scaled() {
        let b = Bitmap { width: 1, height: 1, pixels: vec![7] };
        let mut buf = vec![0u32; 4];
        blit(&mut buf, 2, 2, [0.0, 0.0, 2.0, 2.0], &b);
        assert_eq!(buf, vec![7; 4]);
    }
}
