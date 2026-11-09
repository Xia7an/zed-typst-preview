use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use editor::scroll::Autoscroll;
use editor::{Editor, EditorEvent, SelectionEffects, ToPoint as _};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, ImageId, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, PinchEvent, Pixels, Point, Render,
    RenderImage, ScrollEvent, StyledImage, Subscription, WeakEntity, Window, canvas, div, img, point, px,
};
use serde_json::{Value, json};
use text::{Bias, PointUtf16, Unclipped};
use ui::prelude::*;
use workspace::item::{Item, ItemEvent};
use workspace::{MultiWorkspace, OpenOptions, OpenVisible, Pane, Workspace};

use crate::hub::{self, ConnId, Writer};
use crate::protocol::parse_hash;
use crate::{FitWidth, OpenPreview, OpenPreviewToTheSide, ZoomIn, ZoomOut};

/// Space around and between pages, in logical pixels.
const MARGIN: f32 = 16.0;
const GAP: f32 = 12.0;
/// Largest bitmap side we ask for (GPU texture limits, memory).
const MAX_BITMAP_SIDE: f32 = 8192.0;
/// Pages are rendered, sent and uploaded in horizontal strips of this many
/// device pixels, so an edit only replaces the strips it touched.
const STRIP_PX: u32 = 128;
/// Strip bitmaps kept besides the ones currently needed.
const SPARE_STRIPS: usize = 96;
const FLASH: Duration = Duration::from_millis(900);

#[derive(Clone, Debug)]
struct PageInfo {
    width: f32,
    height: f32,
    hash: u128,
}

#[derive(Clone, Copy, Debug)]
struct Marker {
    page: usize,
    x: f32,
    y: f32,
    height: f32,
}

impl Marker {
    fn parse(v: &Value) -> Option<Self> {
        Some(Self {
            page: v.get("page")?.as_u64()? as usize,
            x: v.get("x")?.as_f64()? as f32,
            y: v.get("y")?.as_f64()? as f32,
            height: v.get("height").and_then(Value::as_f64).unwrap_or(0.0) as f32,
        })
    }
}

/// Page hash + pixels per point × 1000.
type PageKey = (u128, u32);
/// Page hash + pixels per point × 1000 + strip index.
type Key = (u128, u32, u32);

/// Bitmap size of a page, rounded like `typst-render` does.
fn page_px(page: &PageInfo, scale_milli: u32) -> (u32, u32) {
    let s = (scale_milli as f64 / 1000.0) as f32;
    let px = |pt: f32| (s * pt).round().max(1.0) as u32;
    (px(page.width), px(page.height))
}

/// Strips touched by the changed `bands` (pt) of a page `height_px` tall.
/// Mirrors `diff::dirty_strips` in the server.
fn dirty_strips(bands: &[(f64, f64)], scale_milli: u32, height_px: u32) -> Vec<u32> {
    if height_px == 0 {
        return Vec::new();
    }
    let scale = scale_milli as f64 / 1000.0;
    let last = (height_px - 1) / STRIP_PX;
    let mut out = Vec::new();
    for &(y0, y1) in bands {
        let r0 = ((y0 * scale).floor() - 1.0).max(0.0) as u32;
        let r1 = ((y1 * scale).ceil() + 1.0).max(0.0) as u32;
        out.extend((r0 / STRIP_PX).min(last)..=(r1 / STRIP_PX).min(last));
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// A rectangle in content coordinates (logical px).
#[derive(Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

pub enum PreviewEvent {
    TitleChanged,
}

pub struct TypstPreviewView {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    window: gpui::AnyWindowHandle,
    origin_pane: Option<WeakEntity<Pane>>,
    /// The file the preview was opened for (sent to the server on attach).
    source: PathBuf,
    title: SharedString,
    conn: Option<(ConnId, Writer)>,

    pages: Vec<PageInfo>,
    /// Strip bitmaps (+ when last needed). Strips that did not change in an
    /// edit are shared between the old and the new page hash.
    images: HashMap<Key, (Arc<RenderImage>, u64)>,
    /// Per page, the strips drawn last frame: kept on screen until their
    /// replacements arrive, so edits and zooming never flicker.
    shown: Vec<Vec<(Key, Arc<RenderImage>)>>,
    in_flight: HashSet<Key>,
    last_wanted: Vec<PageKey>,
    /// Every image handed to GPUI; dropped from the GPU atlas once neither
    /// `images` nor `shown` refers to it.
    owned: HashMap<ImageId, Arc<RenderImage>>,
    tick: u64,

    /// Logical pixels per point.
    zoom: f32,
    fit_width: bool,
    scroll: Point<f32>,
    bounds: Option<Bounds<Pixels>>,
    scale_factor: f32,

    markers: Vec<Marker>,
    flash: Option<(Marker, Instant)>,
    pending_reveal: Option<Marker>,
    error: Option<String>,

    press: Option<(Point<Pixels>, Point<f32>)>,
    dragging: bool,

    tracked_editor: Option<WeakEntity<Editor>>,
    _editor_subscription: Option<Subscription>,
    _workspace_subscription: Subscription,
}

fn is_typst(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("typ"))
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn editor_path(editor: &Entity<Editor>, cx: &App) -> Option<PathBuf> {
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?;
    Some(file.as_local()?.abs_path(cx))
}

impl TypstPreviewView {
    pub fn register(workspace: &mut Workspace, _window: &mut Window, _cx: &mut Context<Workspace>) {
        workspace.register_action(|workspace, _: &OpenPreview, window, cx| {
            if let Some((path, pane)) = Self::active_typst_file(workspace, cx) {
                Self::open(workspace, path, pane, false, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &OpenPreviewToTheSide, window, cx| {
            if let Some((path, pane)) = Self::active_typst_file(workspace, cx) {
                Self::open(workspace, path, pane, true, window, cx);
            }
        });
    }

    fn active_typst_file(
        workspace: &Workspace,
        cx: &mut Context<Workspace>,
    ) -> Option<(PathBuf, Entity<Pane>)> {
        let editor = workspace.active_item(cx)?.act_as::<Editor>(cx)?;
        let path = editor_path(&editor, cx)?;
        is_typst(&path).then(|| (path, workspace.active_pane().clone()))
    }

    fn is_typst_editor(editor: &Entity<Editor>, cx: &App) -> bool {
        editor_path(editor, cx).is_some_and(|p| is_typst(&p))
    }

    /// Opens (or reveals) the preview of `path`, next to `origin` if
    /// `to_the_side`.
    pub fn open(
        workspace: &mut Workspace,
        path: PathBuf,
        origin: Entity<Pane>,
        to_the_side: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        // One preview per workspace: re-target an existing one.
        for pane in workspace.panes().to_vec() {
            let existing = pane.read(cx).items_of_type::<Self>().next();
            if let Some(view) = existing {
                view.update(cx, |view, cx| view.retarget(path.clone(), cx));
                if let Some(ix) = pane.read(cx).index_for_item(&view) {
                    pane.update(cx, |pane, cx| pane.activate_item(ix, true, false, window, cx));
                }
                return;
            }
        }
        let target = if to_the_side {
            workspace.adjacent_pane_of(&origin, window, cx)
        } else {
            origin.clone()
        };
        let view = Self::new(path, workspace, Some(origin.downgrade()), window, cx);
        target.update(cx, |pane, cx| {
            pane.add_item(Box::new(view), true, !to_the_side, None, window, cx)
        });
        if to_the_side {
            // Keep typing in the editor.
            window.focus(&origin.focus_handle(cx), cx);
        }
    }

    /// Opens a preview requested by the language server (a toolbar button,
    /// a code action or `auto_open`), in the window whose project contains
    /// `path`: next to the active pane if `split`, else in it.
    pub(crate) fn open_for_path(path: PathBuf, split: bool, cx: &mut App) {
        let target = canonical(&path);
        for window in cx.windows() {
            let Some(multi) = window.downcast::<MultiWorkspace>() else {
                continue;
            };
            let opened = multi
                .update(cx, |multi, window, cx| {
                    let workspaces: Vec<_> = multi.workspaces().cloned().collect();
                    for workspace in workspaces {
                        let contains = workspace
                            .read(cx)
                            .project()
                            .read(cx)
                            .visible_worktrees(cx)
                            .any(|wt| target.starts_with(canonical(&wt.read(cx).abs_path())));
                        if contains {
                            workspace.update(cx, |workspace, cx| {
                                let origin = workspace.active_pane().clone();
                                Self::open(workspace, path.clone(), origin, split, window, cx);
                            });
                            return true;
                        }
                    }
                    false
                })
                .unwrap_or(false);
            if opened {
                return;
            }
        }
    }

    fn new(
        source: PathBuf,
        workspace: &Workspace,
        origin_pane: Option<WeakEntity<Pane>>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_handle = workspace.weak_handle();
        let workspace_entity = cx.entity();
        // The workspace is being updated right now: read it here, not through
        // its entity.
        let active_editor = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx));
        let view = cx.new(|cx| {
            let workspace_subscription = cx.subscribe_in(
                &workspace_entity,
                window,
                |this: &mut Self, workspace, event: &workspace::Event, window, cx| {
                    if let workspace::Event::ActiveItemChanged = event {
                        this.track_active_editor(workspace, window, cx);
                    }
                },
            );
            cx.on_release(|this: &mut Self, cx| {
                let conn = this.conn.as_ref().map(|(id, _)| *id);
                for (_, image) in this.owned.drain() {
                    cx.drop_image(image, None);
                }
                if let Some(hub) = hub::hub(cx) {
                    hub.update(cx, |hub, cx| hub.view_released(conn, cx));
                }
            })
            .detach();
            let title = Self::title_for(&source);
            let mut this = Self {
                focus_handle: cx.focus_handle(),
                workspace: workspace_handle,
                window: window.window_handle(),
                origin_pane,
                source,
                title,
                conn: None,
                pages: Vec::new(),
                images: HashMap::new(),
                shown: Vec::new(),
                in_flight: HashSet::new(),
                last_wanted: Vec::new(),
                owned: HashMap::new(),
                tick: 0,
                zoom: 1.0,
                fit_width: true,
                scroll: point(0.0, 0.0),
                bounds: None,
                scale_factor: window.scale_factor(),
                markers: Vec::new(),
                flash: None,
                pending_reveal: None,
                error: None,
                press: None,
                dragging: false,
                tracked_editor: None,
                _editor_subscription: None,
                _workspace_subscription: workspace_subscription,
            };
            if let Some(editor) = active_editor {
                this.track_editor(editor, window, cx);
            }
            this
        });
        if let Some(hub) = hub::hub(cx) {
            hub.update(cx, |hub, cx| hub.add_view(view.downgrade(), cx));
        }
        view
    }

    fn title_for(path: &Path) -> SharedString {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("Preview {name}").into()
    }

    // ------------------------------------------------------------ server link

    pub(crate) fn conn(&self) -> Option<ConnId> {
        self.conn.as_ref().map(|(id, _)| *id)
    }

    pub(crate) fn source_path(&self) -> PathBuf {
        self.source.clone()
    }

    fn send(&self, header: Value) {
        if let Some((_, writer)) = &self.conn {
            writer.send(header).ok();
        }
    }

    pub(crate) fn set_conn(&mut self, conn: Option<(ConnId, Writer)>, cx: &mut Context<Self>) {
        self.conn = conn;
        self.in_flight.clear();
        self.last_wanted.clear();
        if self.conn.is_some() {
            self.send(json!({ "type": "attach", "path": self.source }));
            if let Some(editor) = self.tracked_editor.as_ref().and_then(|e| e.upgrade()) {
                self.send_cursor(&editor, cx);
            }
        }
        cx.notify();
    }

    fn retarget(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.source = path;
        self.send(json!({ "type": "attach", "path": self.source }));
        cx.notify();
    }

    fn track_active_editor(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = workspace.read(cx).active_item(cx) else {
            return;
        };
        // The preview itself became active: it is being updated right now,
        // so it must not be read (and it is not an editor anyway).
        if item.item_id() == cx.entity_id() {
            return;
        }
        let Some(editor) = item.act_as::<Editor>(cx) else {
            return;
        };
        self.track_editor(editor, window, cx);
    }

    /// Follows the cursor of `editor` if it shows a Typst file.
    fn track_editor(&mut self, editor: Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        if !Self::is_typst_editor(&editor, cx) {
            return;
        }
        if self.tracked_editor.as_ref().and_then(|e| e.upgrade()).as_ref() == Some(&editor) {
            return;
        }
        self._editor_subscription = Some(cx.subscribe_in(
            &editor,
            window,
            |this: &mut Self, editor, event: &EditorEvent, _window, cx| {
                if let EditorEvent::SelectionsChanged { local: true } = event {
                    this.send_cursor(editor, cx);
                }
            },
        ));
        self.tracked_editor = Some(editor.downgrade());
        self.send_cursor(&editor, cx);
    }

    /// Reports the editor cursor so the preview can follow it.
    fn send_cursor(&self, editor: &Entity<Editor>, cx: &App) {
        let Some(path) = editor_path(editor, cx) else {
            return;
        };
        let editor = editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let head = editor.selections.newest_anchor().head();
        let position = snapshot.point_to_point_utf16(head.to_point(&snapshot));
        self.send(json!({
            "type": "cursor",
            "path": path,
            "line": position.row,
            "character": position.column,
        }));
    }

    pub(crate) fn handle_message(&mut self, header: Value, payload: Vec<u8>, cx: &mut Context<Self>) {
        let kind = header.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "target" => {
                if let Some(path) = header.get("path").and_then(Value::as_str) {
                    let title = Self::title_for(Path::new(path));
                    if title != self.title {
                        self.title = title;
                        self.reset_document();
                        cx.emit(PreviewEvent::TitleChanged);
                    }
                }
            }
            "document" => {
                let mut pages = Vec::new();
                for p in header.get("pages").and_then(Value::as_array).into_iter().flatten() {
                    let (Some(width), Some(height), Some(hash)) = (
                        p.get("width").and_then(Value::as_f64),
                        p.get("height").and_then(Value::as_f64),
                        parse_hash(p.get("hash")),
                    ) else {
                        continue;
                    };
                    let page = PageInfo { width: width as f32, height: height as f32, hash };
                    // An edit: reuse the strips that look the same as before.
                    if let (Some(base), Some(dirty)) = (parse_hash(p.get("base")), p.get("dirty").and_then(Value::as_array)) {
                        let bands: Vec<(f64, f64)> = dirty
                            .iter()
                            .filter_map(|b| Some((b.get(0)?.as_f64()?, b.get(1)?.as_f64()?)))
                            .collect();
                        self.reuse_clean_strips(base, &page, &bands);
                    }
                    // Strips the server is already rendering for us.
                    for push in p.get("pushed").and_then(Value::as_array).into_iter().flatten() {
                        let Some(scale) = push.get("scale_milli").and_then(Value::as_u64) else { continue };
                        if push.get("strip_px").and_then(Value::as_u64) != Some(STRIP_PX as u64) {
                            continue;
                        }
                        for k in push.get("strips").and_then(Value::as_array).into_iter().flatten() {
                            if let Some(k) = k.as_u64() {
                                self.in_flight.insert((hash, scale as u32, k as u32));
                            }
                        }
                    }
                    pages.push(page);
                }
                log::debug!("typst preview: document with {} pages", pages.len());
                self.pages = pages;
                self.error = None;
                self.shown.resize(self.pages.len(), Vec::new());
                // Forget bitmaps of page versions that no longer exist.
                let live: HashSet<u128> = self.pages.iter().map(|p| p.hash).collect();
                self.images.retain(|k, _| live.contains(&k.0));
                if let Some(m) = self.pending_reveal.take() {
                    self.reveal(m, true);
                }
                // Ask for the new strips now rather than on the next frame.
                self.update_zoom();
                self.request_renders();
            }
            "status" => {
                self.error = header
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "bitmap" => {
                let (Some(hash), Some(scale), Some(strip), Some(w), Some(h)) = (
                    parse_hash(header.get("hash")),
                    header.get("scale_milli").and_then(Value::as_u64),
                    header.get("strip").and_then(Value::as_u64),
                    header.get("width").and_then(Value::as_u64),
                    header.get("height").and_then(Value::as_u64),
                ) else {
                    return;
                };
                let key = (hash, scale as u32, strip as u32);
                self.in_flight.remove(&key);
                if !self.pages.iter().any(|p| p.hash == hash) {
                    return; // An outdated version of a page.
                }
                // BGRA, as GPUI expects.
                let Some(buffer) = image::RgbaImage::from_raw(w as u32, h as u32, payload) else {
                    return;
                };
                let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
                self.owned.insert(image.id, image.clone());
                self.images.insert(key, (image, self.tick));
            }
            "skipped" => {
                if let (Some(hash), Some(scale)) = (
                    parse_hash(header.get("hash")),
                    header.get("scale_milli").and_then(Value::as_u64),
                ) {
                    let scale = scale as u32;
                    match (
                        header.get("first").and_then(Value::as_u64),
                        header.get("end").and_then(Value::as_u64),
                    ) {
                        (Some(first), Some(end)) => {
                            for k in first as u32..end as u32 {
                                self.in_flight.remove(&(hash, scale, k));
                            }
                        }
                        _ => self.in_flight.retain(|k| (k.0, k.1) != (hash, scale)),
                    }
                }
            }
            "cursor" => {
                let markers: Vec<Marker> = header
                    .get("markers")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Marker::parse).collect())
                    .unwrap_or_default();
                let scroll = header.get("scroll").and_then(Value::as_bool).unwrap_or(true);
                if scroll && !self.pages.is_empty() {
                    let (_, vh) = self.view_size();
                    let mid = self.scroll.y + vh / 2.0;
                    let best = markers.iter().copied().min_by(|a, b| {
                        let da = self.content_pos(a.page, a.x, a.y).map_or(f32::MAX, |p| (p.y - mid).abs());
                        let db = self.content_pos(b.page, b.x, b.y).map_or(f32::MAX, |p| (p.y - mid).abs());
                        da.total_cmp(&db)
                    });
                    if let Some(m) = best {
                        self.reveal(m, false);
                    }
                }
                self.markers = markers;
            }
            "reveal" => {
                if let Some(m) = header.get("marker").and_then(Marker::parse) {
                    if self.pages.is_empty() {
                        self.pending_reveal = Some(m);
                    } else {
                        self.reveal(m, true);
                        self.flash = Some((m, Instant::now()));
                    }
                }
            }
            "jump" => {
                let (Some(path), Some(line), Some(character)) = (
                    header.get("path").and_then(Value::as_str).map(PathBuf::from),
                    header.get("line").and_then(Value::as_u64),
                    header.get("character").and_then(Value::as_u64),
                ) else {
                    return;
                };
                let workspace = self.workspace.clone();
                let origin = self.origin_pane.clone();
                let window = self.window;
                cx.defer(move |cx| {
                    window
                        .update(cx, |_, window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    jump(workspace, origin, path, line as u32, character as u32, window, cx)
                                })
                                .ok();
                        })
                        .ok();
                });
                return;
            }
            "invalidate" => {
                self.images.clear();
                self.shown.iter_mut().for_each(Vec::clear);
                self.in_flight.clear();
                self.last_wanted.clear();
            }
            _ => return,
        }
        cx.notify();
    }

    /// After an edit of a page from `base` to `page.hash`: strips outside the
    /// changed `bands` have the same pixels, so share their bitmaps.
    fn reuse_clean_strips(&mut self, base: u128, page: &PageInfo, bands: &[(f64, f64)]) {
        let mut dirty: HashMap<u32, Vec<u32>> = HashMap::new();
        let old: Vec<Key> = self.images.keys().filter(|k| k.0 == base).copied().collect();
        for key in old {
            let (_, scale, strip) = key;
            let dirty = dirty
                .entry(scale)
                .or_insert_with(|| dirty_strips(bands, scale, page_px(page, scale).1));
            if !dirty.contains(&strip) {
                let image = self.images[&key].0.clone();
                self.images.entry((page.hash, scale, strip)).or_insert((image, self.tick));
            }
        }
    }

    fn reset_document(&mut self) {
        self.pages.clear();
        self.images.clear();
        self.shown.clear();
        self.in_flight.clear();
        self.last_wanted.clear();
        self.markers.clear();
        self.scroll = point(0.0, 0.0);
        self.fit_width = true;
        self.error = None;
    }

    // ------------------------------------------------------------- layout

    fn view_size(&self) -> (f32, f32) {
        self.bounds
            .map(|b| (b.size.width.as_f32(), b.size.height.as_f32()))
            .unwrap_or((800.0, 600.0))
    }

    fn max_page_width(&self) -> f32 {
        self.pages.iter().map(|p| p.width).fold(1.0, f32::max)
    }

    fn fit_zoom(&self) -> f32 {
        let (w, _) = self.view_size();
        ((w - 2.0 * MARGIN) / self.max_page_width()).clamp(0.05, 20.0)
    }

    fn update_zoom(&mut self) {
        if self.fit_width && self.bounds.is_some() && !self.pages.is_empty() {
            self.zoom = self.fit_zoom();
        }
        self.clamp_scroll();
    }

    fn layout(&self) -> (Vec<Rect>, (f32, f32)) {
        let (vw, _) = self.view_size();
        let s = self.zoom;
        let content_w = vw.max(self.max_page_width() * s + 2.0 * MARGIN);
        let mut y = MARGIN;
        let rects: Vec<Rect> = self
            .pages
            .iter()
            .map(|p| {
                let (w, h) = (p.width * s, p.height * s);
                let r = Rect { x: (content_w - w) / 2.0, y, w, h };
                y += h + GAP;
                r
            })
            .collect();
        let content_h = if rects.is_empty() { 0.0 } else { y - GAP + MARGIN };
        (rects, (content_w, content_h))
    }

    fn clamp_scroll(&mut self) {
        let (_, (cw, ch)) = self.layout();
        let (vw, vh) = self.view_size();
        self.scroll.x = self.scroll.x.clamp(0.0, (cw - vw).max(0.0));
        self.scroll.y = self.scroll.y.clamp(0.0, (ch - vh).max(0.0));
    }

    /// Position (relative to the pane) → page and point on it (pt).
    fn hit(&self, pos: Point<f32>) -> Option<(usize, f32, f32)> {
        let (rects, _) = self.layout();
        let (cx, cy) = (pos.x + self.scroll.x, pos.y + self.scroll.y);
        rects.iter().enumerate().find_map(|(i, r)| {
            (cx >= r.x && cx < r.x + r.w && cy >= r.y && cy < r.y + r.h)
                .then(|| (i, (cx - r.x) / self.zoom, (cy - r.y) / self.zoom))
        })
    }

    fn content_pos(&self, page: usize, x: f32, y: f32) -> Option<Point<f32>> {
        let (rects, _) = self.layout();
        let r = rects.get(page)?;
        Some(point(r.x + x * self.zoom, r.y + y * self.zoom))
    }

    fn local(&self, pos: Point<Pixels>) -> Point<f32> {
        let origin = self.bounds.map(|b| b.origin).unwrap_or_default();
        point((pos.x - origin.x).as_f32(), (pos.y - origin.y).as_f32())
    }

    fn set_zoom(&mut self, zoom: f32, anchor: Option<Point<f32>>, cx: &mut Context<Self>) {
        let (vw, vh) = self.view_size();
        let anchor = anchor.unwrap_or(point(vw / 2.0, vh / 2.0));
        let before = self.hit(anchor);
        let ratio = zoom.clamp(0.05, 20.0) / self.zoom;
        let old = self.scroll;
        self.zoom = zoom.clamp(0.05, 20.0);
        self.fit_width = false;
        match before.and_then(|(p, x, y)| self.content_pos(p, x, y)) {
            Some(c) => self.scroll = point(c.x - anchor.x, c.y - anchor.y),
            None => {
                self.scroll = point(
                    (old.x + anchor.x) * ratio - anchor.x,
                    (old.y + anchor.y) * ratio - anchor.y,
                )
            }
        }
        self.clamp_scroll();
        cx.notify();
    }

    fn reveal(&mut self, m: Marker, center: bool) {
        let Some(c) = self.content_pos(m.page, m.x, m.y) else {
            return;
        };
        let (vw, vh) = self.view_size();
        let h = m.height.max(10.0) * self.zoom;
        let visible_y = c.y - h >= self.scroll.y + vh * 0.05 && c.y + h * 0.3 <= self.scroll.y + vh * 0.95;
        let visible_x = c.x >= self.scroll.x && c.x <= self.scroll.x + vw;
        if center || !visible_y {
            self.scroll.y = c.y - vh / 2.0;
        }
        if center || !visible_x {
            self.scroll.x = c.x - vw / 2.0;
        }
        self.clamp_scroll();
    }

    fn key_for(&self, index: usize) -> PageKey {
        let page = &self.pages[index];
        let mut scale = self.zoom * self.scale_factor;
        let side = page.width.max(page.height) * scale;
        if side > MAX_BITMAP_SIDE {
            scale *= MAX_BITMAP_SIDE / side;
        }
        (page.hash, (scale * 1000.0).round() as u32)
    }

    /// Strips of page `index` (at its current resolution) that cover the
    /// part `top..bottom` (pt) of the page.
    fn strip_range(&self, index: usize, top: f32, bottom: f32) -> std::ops::RangeInclusive<u32> {
        let (_, scale) = self.key_for(index);
        let (_, height) = page_px(&self.pages[index], scale);
        let last = (height.max(1) - 1) / STRIP_PX;
        let s = scale as f32 / 1000.0;
        let strip = |pt: f32| (((pt * s).max(0.0) as u32) / STRIP_PX).min(last);
        strip(top)..=strip(bottom)
    }

    /// Asks the server for the strips on screen, plus one screen above and
    /// below, that are neither cached nor on their way.
    fn request_renders(&mut self) {
        // Wait for the first layout: the zoom depends on the pane size.
        if self.conn.is_none() || self.bounds.is_none() || self.pages.is_empty() {
            return;
        }
        self.tick += 1;
        let (rects, _) = self.layout();
        let (_, vh) = self.view_size();
        let (lo, hi) = (self.scroll.y - vh, self.scroll.y + 2.0 * vh);
        let mut wanted: Vec<PageKey> = Vec::new();
        let mut needed: HashSet<Key> = HashSet::new();
        let mut requests = Vec::new();
        for (index, r) in rects.iter().enumerate() {
            if r.y + r.h <= lo || r.y >= hi {
                continue;
            }
            let (hash, scale) = self.key_for(index);
            wanted.push((hash, scale));
            let mut run: Option<(u32, u32)> = None;
            let mut flush = |run: &mut Option<(u32, u32)>| {
                if let Some((first, end)) = run.take() {
                    requests.push(json!({
                        "index": index,
                        "hash": format!("{hash:x}"),
                        "scale_milli": scale,
                        "strip_px": STRIP_PX,
                        "first": first,
                        "end": end,
                    }));
                }
            };
            for k in self.strip_range(index, (lo - r.y) / self.zoom, (hi - r.y) / self.zoom) {
                let key = (hash, scale, k);
                needed.insert(key);
                if let Some(entry) = self.images.get_mut(&key) {
                    entry.1 = self.tick;
                    flush(&mut run);
                } else if !self.in_flight.insert(key) {
                    flush(&mut run);
                } else {
                    run = Some(run.map_or((k, k + 1), |(first, _)| (first, k + 1)));
                }
            }
            flush(&mut run);
        }
        if !requests.is_empty() || wanted != self.last_wanted {
            let wanted_json: Vec<Value> = wanted
                .iter()
                .map(|k| json!({ "hash": format!("{:x}", k.0), "scale_milli": k.1 }))
                .collect();
            log::debug!("typst preview: requesting {} strip runs", requests.len());
            self.send(json!({ "type": "render", "wanted": wanted_json, "requests": requests }));
            self.last_wanted = wanted;
        }
        // Evict least recently used bitmaps beyond what is needed.
        if self.images.len() > needed.len() + SPARE_STRIPS {
            let mut old: Vec<(Key, u64)> = self
                .images
                .iter()
                .filter(|(k, _)| !needed.contains(k))
                .map(|(k, (_, used))| (*k, *used))
                .collect();
            old.sort_by_key(|&(_, used)| used);
            let excess = self.images.len() - needed.len() - SPARE_STRIPS;
            for (key, _) in old.into_iter().take(excess) {
                self.images.remove(&key);
            }
        }
    }

    /// The strips to draw for the part `top..bottom` (pt) of page `index`:
    /// current ones where available, else what was shown there before (an
    /// older version of the page, or another zoom level underneath).
    fn strips_to_draw(&mut self, index: usize, top: f32, bottom: f32) -> Vec<(Key, Arc<RenderImage>)> {
        let (hash, scale) = self.key_for(index);
        let mut current = Vec::new();
        let mut missing = false;
        for k in self.strip_range(index, top, bottom) {
            let fresh = self.images.get(&(hash, scale, k)).map(|(image, _)| ((hash, scale, k), image.clone()));
            let stale = || self.shown[index].iter().find(|(key, _)| key.1 == scale && key.2 == k).cloned();
            match fresh.or_else(stale) {
                Some(strip) => current.push(strip),
                None => missing = true,
            }
        }
        let mut strips: Vec<(Key, Arc<RenderImage>)> = Vec::new();
        if missing {
            strips.extend(self.shown[index].iter().filter(|(key, _)| key.1 != scale).cloned());
        }
        strips.extend(current);
        self.shown[index] = strips.clone();
        strips
    }

    /// Drops images from the GPU atlas that are neither cached nor shown.
    fn collect_garbage(&mut self, window: &mut Window) {
        let live: HashSet<ImageId> = self
            .images
            .values()
            .map(|(image, _)| image.id)
            .chain(self.shown.iter().flatten().map(|(_, image)| image.id))
            .collect();
        self.owned.retain(|id, image| {
            let keep = live.contains(id);
            if !keep {
                window.drop_image(image.clone()).ok();
            }
            keep
        });
    }

    // ------------------------------------------------------------- input

    fn on_scroll(&mut self, event: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(20.0));
        let (dx, dy) = (delta.x.as_f32(), delta.y.as_f32());
        if event.modifiers.platform || event.modifiers.control {
            let anchor = self.local(event.position);
            self.set_zoom(self.zoom * (dy / 200.0).exp(), Some(anchor), cx);
        } else {
            self.scroll.x -= dx;
            self.scroll.y -= dy;
            self.clamp_scroll();
            cx.notify();
        }
    }

    fn on_pinch(&mut self, event: &PinchEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let anchor = self.local(event.position);
        self.set_zoom(self.zoom * (1.0 + event.delta), Some(anchor), cx);
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        self.press = Some((event.position, self.scroll));
        self.dragging = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((start, scroll)) = self.press else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.press = None;
            return;
        }
        let (dx, dy) = ((event.position.x - start.x).as_f32(), (event.position.y - start.y).as_f32());
        if self.dragging || dx.abs() + dy.abs() > 4.0 {
            self.dragging = true;
            self.scroll = point(scroll.x - dx, scroll.y - dy);
            self.clamp_scroll();
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        let was_drag = self.dragging;
        self.press = None;
        self.dragging = false;
        if !was_drag
            && let Some((page, x, y)) = self.hit(self.local(event.position))
        {
            self.send(json!({ "type": "click", "page": page, "x": x, "y": y }));
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let (_, vh) = self.view_size();
        let line = 48.0;
        let (dx, dy) = match event.keystroke.key.as_str() {
            "down" => (0.0, line),
            "up" => (0.0, -line),
            "left" => (-line, 0.0),
            "right" => (line, 0.0),
            "pagedown" => (0.0, vh * 0.9),
            "space" if event.keystroke.modifiers.shift => (0.0, -vh * 0.9),
            "space" => (0.0, vh * 0.9),
            "pageup" => (0.0, -vh * 0.9),
            "home" => (0.0, -f32::MAX / 4.0),
            "end" => (0.0, f32::MAX / 4.0),
            _ => return,
        };
        self.scroll.x += dx;
        self.scroll.y += dy;
        self.clamp_scroll();
        cx.notify();
        cx.stop_propagation();
    }
}

/// Moves the cursor of the editor showing `path` (opening it if needed).
fn jump(
    workspace: &mut Workspace,
    origin: Option<WeakEntity<Pane>>,
    path: PathBuf,
    line: u32,
    character: u32,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    fn select(editor: &mut Editor, line: u32, character: u32, window: &mut Window, cx: &mut Context<Editor>) {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let point = snapshot.clip_point_utf16(Unclipped(PointUtf16::new(line, character)), Bias::Left);
        let offset = snapshot.point_utf16_to_offset(point);
        editor.change_selections(SelectionEffects::scroll(Autoscroll::fit()), window, cx, |s| {
            s.select_ranges([offset..offset])
        });
    }

    let target = canonical(&path);
    for pane in workspace.panes().to_vec() {
        let editor = pane
            .read(cx)
            .items_of_type::<Editor>()
            .find(|e| editor_path(e, cx).is_some_and(|p| canonical(&p) == target));
        if let Some(editor) = editor {
            if let Some(ix) = pane.read(cx).index_for_item(&editor) {
                pane.update(cx, |pane, cx| pane.activate_item(ix, true, true, window, cx));
            }
            editor.update(cx, |editor, cx| select(editor, line, character, window, cx));
            return;
        }
    }

    let project_path = {
        let project = workspace.project().read(cx);
        project.find_project_path(&path, cx).or_else(|| {
            project.visible_worktrees(cx).find_map(|wt| {
                let root = wt.read(cx).abs_path();
                let rel = target.strip_prefix(canonical(&root)).ok()?;
                project.find_project_path(root.join(rel), cx)
            })
        })
    };
    let task = match project_path {
        Some(project_path) => workspace.open_path(project_path, origin, true, window, cx),
        None => workspace.open_abs_path(
            path,
            OpenOptions {
                visible: Some(OpenVisible::None),
                focus: Some(true),
                ..Default::default()
            },
            window,
            cx,
        ),
    };
    cx.spawn_in(window, async move |_, cx| {
        if let Ok(item) = task.await
            && let Some(editor) = item.downcast::<Editor>()
        {
            editor
                .update_in(cx, |editor, window, cx| select(editor, line, character, window, cx))
                .ok();
        }
    })
    .detach();
}

impl Render for TypstPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.collect_garbage(window);
        self.scale_factor = window.scale_factor();
        self.update_zoom();

        let (rects, _) = self.layout();
        let (_, vh) = self.view_size();
        let (sx, sy) = (self.scroll.x, self.scroll.y);
        let visible: Vec<usize> = (0..rects.len())
            .filter(|&i| rects[i].y + rects[i].h > sy && rects[i].y < sy + vh)
            .collect();
        log::trace!(
            "typst preview: render, bounds {:?}, {} pages, visible {visible:?}",
            self.bounds,
            self.pages.len()
        );
        self.request_renders();
        // Pages off screen keep nothing alive (the cache still has them).
        for (i, shown) in self.shown.iter_mut().enumerate() {
            if !visible.contains(&i) {
                shown.clear();
            }
        }

        let colors = cx.theme().colors();
        let sf = self.scale_factor;
        // Snap to device pixels so bitmaps map 1:1 to the screen.
        let snap = |v: f32| (v * sf).round() / sf;
        let mut children: Vec<AnyElement> = Vec::new();
        for &i in &visible {
            let r = rects[i];
            let (x, y) = (snap(r.x - sx), snap(r.y - sy));
            let (w, h) = (snap(r.w), snap(r.h));
            children.push(
                div()
                    .absolute()
                    .left(px(x - 1.0))
                    .top(px(y - 1.0))
                    .w(px(w + 2.0))
                    .h(px(h + 2.0))
                    .bg(colors.border)
                    .into_any_element(),
            );
            children.push(div().absolute().left(px(x)).top(px(y)).w(px(w)).h(px(h)).bg(gpui::white()).into_any_element());
            let zoom = self.zoom;
            let (top, bottom) = ((sy - r.y) / zoom, (sy + vh - r.y) / zoom);
            for ((_, scale, k), image) in self.strips_to_draw(i, top, bottom) {
                // Place each strip by its own resolution (older strips may
                // come from another zoom level).
                let s = scale as f32 / 1000.0;
                let rows = image.size(0).height.0 as f32;
                let y0 = snap(r.y - sy + (k * STRIP_PX) as f32 / s * zoom);
                let y1 = snap(r.y - sy + ((k * STRIP_PX) as f32 + rows) / s * zoom).min(y + h);
                if y1 <= y0 {
                    continue;
                }
                children.push(
                    img(image)
                        .absolute()
                        .left(px(x))
                        .top(px(y0))
                        .w(px(w))
                        .h(px(y1 - y0))
                        .object_fit(ObjectFit::Fill)
                        .into_any_element(),
                );
            }
        }

        let zoom = self.zoom;
        for m in &self.markers {
            let Some(r) = rects.get(m.page) else { continue };
            let size = if m.height > 0.0 { m.height } else { 11.0 } * zoom;
            let w = (0.8 * zoom).max(2.0);
            children.push(
                div()
                    .absolute()
                    .left(px(r.x + m.x * zoom - sx - w / 2.0))
                    .top(px(r.y + m.y * zoom - sy - size * 0.85))
                    .w(px(w))
                    .h(px(size * 1.1))
                    .bg(gpui::rgba(0x2f80edd9))
                    .into_any_element(),
            );
        }
        if let Some((m, t)) = self.flash {
            let age = t.elapsed();
            if age < FLASH {
                if let Some(r) = rects.get(m.page) {
                    let size = if m.height > 0.0 { m.height } else { 14.0 } * zoom;
                    let alpha = 0.35 * (1.0 - age.as_secs_f32() / FLASH.as_secs_f32());
                    children.push(
                        div()
                            .absolute()
                            .left(px(r.x - sx))
                            .top(px(r.y + m.y * zoom - sy - size))
                            .w(px(r.w))
                            .h(px(size * 1.6))
                            .bg(gpui::hsla(0.13, 1.0, 0.6, alpha))
                            .into_any_element(),
                    );
                }
                window.request_animation_frame();
            } else {
                self.flash = None;
            }
        }

        let placeholder = if self.pages.is_empty() {
            Some(if self.conn.is_none() {
                "Waiting for the typst-livepreview language server…"
            } else {
                "Compiling…"
            })
        } else {
            None
        };

        let this = cx.entity().downgrade();
        div()
            .id("TypstPreview")
            .key_context("TypstPreview")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(colors.editor_background)
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.set_zoom(this.zoom * 1.2, None, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.set_zoom(this.zoom / 1.2, None, cx)))
            .on_action(cx.listener(|this, _: &FitWidth, _, cx| {
                this.fit_width = true;
                cx.notify();
            }))
            .on_scroll(cx.listener(Self::on_scroll))
            .on_pinch(cx.listener(Self::on_pinch))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        this.update(cx, |this, cx| {
                            if this.bounds != Some(bounds) {
                                this.bounds = Some(bounds);
                                cx.notify();
                            }
                        })
                        .ok();
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(children)
            .when_some(placeholder, |this, text| {
                this.child(
                    div()
                        .absolute()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Label::new(text).color(Color::Muted)),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .bg(colors.editor_background)
                        .border_t_1()
                        .border_color(colors.border)
                        .child(Icon::new(IconName::Warning).color(Color::Error).size(IconSize::Small))
                        .child(Label::new(error).color(Color::Error).size(LabelSize::Small)),
                )
            })
    }
}

impl Focusable for TypstPreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PreviewEvent> for TypstPreviewView {}

impl Item for TypstPreviewView {
    type Event = PreviewEvent;

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        FileIcons::get_icon(&self.source, cx)
            .map(Icon::from_path)
            .or_else(|| Some(Icon::new(IconName::Eye)))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            PreviewEvent::TitleChanged => f(ItemEvent::UpdateTab),
        }
    }
}
