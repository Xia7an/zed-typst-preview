//! The core actor: owns the Typst world, applies editor changes, compiles and
//! resolves jumps between source and preview.
//!
//! Every message is handled on one thread. Pending messages are drained before
//! compiling, so a burst of keystrokes results in a single compilation.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use typst::WorldExt;
use typst::diag::Severity;
use typst::introspection::PagedPosition;
use typst::layout::{Abs, Frame, FrameItem, Point, Transform};
use typst::syntax::{LinkedNode, Side, Source, Span, SyntaxKind};
use typst::World;
use typst_ide::Jump;
use typst_kit::fonts::FontStore;
use typst_layout::PagedDocument;

use crate::config::Config;
use crate::diff;
use crate::position;
use crate::viewer::{DocUpdate, Marker, PageInfo, Status, UiEvent, UiSender};
use crate::world::{self, PreviewWorld};

/// A text change from the editor (`range` = start line, start col, end line,
/// end col in UTF-16; `None` replaces the whole document).
pub struct TextChange {
    pub range: Option<(u32, u32, u32, u32)>,
    pub text: String,
}

pub enum CoreMsg {
    Open { path: PathBuf, text: String },
    Change { path: PathBuf, changes: Vec<TextChange> },
    Close { path: PathBuf },
    /// Files changed on disk (or a periodic poll in standalone mode).
    FilesChanged,
    Cursor { path: PathBuf, line: u32, character: u32 },
    /// Explicitly open the preview (optionally for a specific file).
    OpenPreview { path: Option<PathBuf> },
    WindowClosed,
    /// A preview pane inside the editor was opened for `path`.
    Attach { path: PathBuf },
    /// A click at `x`,`y` (pt) on page `page` (0-based).
    Click { page: usize, x: f64, y: f64 },
    Config(Config),
    WorkspaceRoots(Vec<PathBuf>),
    Shutdown,
}

/// Receives jump requests from the preview.
pub trait Editor: Send {
    fn show_document(&self, path: &Path, line: u32, character: u32);
}

/// State shared with the LSP thread (for offering code actions).
#[derive(Default)]
pub struct Shared {
    pub window_open: AtomicBool,
}

enum Fonts {
    Loading(JoinHandle<FontStore>),
    Ready(Arc<FontStore>),
}

pub struct Core {
    cfg: Config,
    roots: Vec<PathBuf>,
    fonts: Option<Fonts>,
    world: Option<PreviewWorld>,
    /// Open buffers that arrived before the world was ready.
    open_paths: HashSet<PathBuf>,
    target: Option<PathBuf>,
    deps: HashSet<PathBuf>,
    doc: Option<Arc<PagedDocument>>,
    doc_generation: u64,
    last_hashes: Vec<u128>,
    window_open: bool,
    dirty: bool,
    /// Last known cursor (path + byte offset).
    cursor: Option<(PathBuf, usize)>,
    cursor_changed: bool,
    /// When the oldest edit not yet compiled arrived (for latency logging).
    edit_received: Option<Instant>,
    first_open_done: bool,
    editor: Box<dyn Editor>,
    ui: UiSender,
    shared: Arc<Shared>,
}

pub fn spawn(
    cfg: Config,
    roots: Vec<PathBuf>,
    editor: Box<dyn Editor>,
    ui: UiSender,
    shared: Arc<Shared>,
) -> (Sender<CoreMsg>, JoinHandle<()>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let handle = std::thread::Builder::new()
        .name("core".into())
        .stack_size(64 * 1024 * 1024) // deep documents recurse a lot
        .spawn(move || {
            let mut core = Core::new(cfg, roots, editor, ui, shared);
            core.run(rx);
        })
        .expect("spawn core thread");
    (tx, handle)
}

fn is_typst(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "typ")
}

impl Core {
    fn new(
        cfg: Config,
        roots: Vec<PathBuf>,
        editor: Box<dyn Editor>,
        ui: UiSender,
        shared: Arc<Shared>,
    ) -> Self {
        let fonts = Self::start_font_loading(&cfg);
        Self {
            cfg,
            roots,
            fonts: Some(fonts),
            world: None,
            open_paths: HashSet::new(),
            target: None,
            deps: HashSet::new(),
            doc: None,
            doc_generation: 0,
            last_hashes: Vec::new(),
            window_open: false,
            dirty: false,
            cursor: None,
            cursor_changed: false,
            edit_received: None,
            first_open_done: false,
            editor,
            ui,
            shared,
        }
    }

    fn start_font_loading(cfg: &Config) -> Fonts {
        let extra: Vec<PathBuf> = cfg.font_paths.iter().map(PathBuf::from).collect();
        let system = !cfg.ignore_system_fonts;
        Fonts::Loading(std::thread::spawn(move || {
            let t = Instant::now();
            let fonts = world::load_fonts(&extra, system);
            log::info!("loaded {} fonts in {:?}", fonts.book().families().count(), t.elapsed());
            fonts
        }))
    }

    fn world(&mut self) -> &mut PreviewWorld {
        if self.world.is_none() {
            let fonts = match self.fonts.take().expect("fonts") {
                Fonts::Loading(h) => Arc::new(h.join().unwrap_or_default()),
                Fonts::Ready(f) => f,
            };
            self.fonts = Some(Fonts::Ready(fonts.clone()));
            self.world = Some(PreviewWorld::new(fonts, Arc::new(world::packages())));
        }
        self.world.as_mut().unwrap()
    }

    fn run(&mut self, rx: Receiver<CoreMsg>) {
        while let Ok(msg) = rx.recv() {
            if !self.handle(msg) {
                return;
            }
            while let Ok(msg) = rx.try_recv() {
                if !self.handle(msg) {
                    return;
                }
            }
            if self.dirty && self.window_open && self.target.is_some() {
                self.compile();
            }
            if self.cursor_changed {
                self.cursor_changed = false;
                self.send_cursor();
            }
        }
    }

    fn handle(&mut self, msg: CoreMsg) -> bool {
        match msg {
            CoreMsg::Open { path, text } => {
                self.world().set_overlay(&path, text);
                self.open_paths.insert(path.clone());
                self.dirty = true;
                if is_typst(&path) && self.target.is_none() {
                    self.retarget(&path);
                    if self.cfg.auto_open && !self.first_open_done {
                        self.first_open_done = true;
                        self.show_window();
                    }
                }
            }
            CoreMsg::Change { path, changes } => {
                self.edit_received.get_or_insert_with(Instant::now);
                let world = self.world();
                let Some(src) = world.overlay_mut(&path) else { return true };
                let mut last = None;
                for change in changes {
                    match change.range {
                        Some((sl, sc, el, ec)) => {
                            let start = position::to_offset(src, sl, sc);
                            let end = position::to_offset(src, el, ec).max(start);
                            src.edit(start..end, &change.text);
                            last = Some(start + change.text.len());
                        }
                        None => {
                            src.replace(&change.text);
                        }
                    }
                }
                if let Some(offset) = last {
                    self.cursor = Some((path.clone(), offset));
                    self.cursor_changed = true;
                }
                self.dirty = true;
                self.on_activity(&path);
            }
            CoreMsg::Close { path } => {
                self.world().remove_overlay(&path);
                self.open_paths.remove(&path);
                self.dirty = true;
            }
            CoreMsg::FilesChanged => self.dirty = true,
            CoreMsg::Cursor { path, line, character } => {
                self.on_activity(&path);
                let Some(src) = self.world().source_for_path(&path) else { return true };
                let offset = position::to_offset(&src, line, character);
                let new = Some((path, offset));
                if new != self.cursor {
                    self.cursor = new;
                    self.cursor_changed = true;
                }
            }
            CoreMsg::OpenPreview { path } => {
                if let Some(path) = path.filter(|p| is_typst(p)) {
                    if self.cfg.main.is_none() && self.target.as_ref() != Some(&path) && !self.deps.contains(&path) {
                        self.retarget(&path);
                    }
                }
                if self.target.is_none() {
                    if let Some(p) = self.open_paths.iter().find(|p| is_typst(p)).cloned() {
                        self.retarget(&p);
                    }
                }
                self.show_window();
            }
            CoreMsg::Attach { path } => {
                if self.cfg.main.is_none() && is_typst(&path) && self.target.as_ref() != Some(&path) && !self.deps.contains(&path) {
                    self.retarget(&path);
                }
                if self.target.is_none() && is_typst(&path) {
                    self.retarget(&path);
                }
                self.window_open = true;
                self.shared.window_open.store(true, Ordering::Relaxed);
                self.dirty = true;
                self.cursor_changed = true;
                // Re-send the current state to the new pane.
                self.last_hashes.clear();
                self.send_title();
            }
            CoreMsg::WindowClosed => {
                self.window_open = false;
                self.shared.window_open.store(false, Ordering::Relaxed);
            }
            CoreMsg::Click { page, x, y } => self.click(page, x, y),
            CoreMsg::Config(cfg) => {
                let fonts_changed = cfg.font_paths != self.cfg.font_paths
                    || cfg.ignore_system_fonts != self.cfg.ignore_system_fonts;
                self.cfg = cfg;
                if fonts_changed {
                    self.fonts = Some(Self::start_font_loading(&self.cfg));
                    // Rebuild the world, keeping the editor buffers.
                    if let Some(old) = self.world.take() {
                        let paths: Vec<_> = self.open_paths.iter().cloned().collect();
                        let texts: Vec<_> = paths
                            .iter()
                            .filter_map(|p| Some((p.clone(), old.source_for_path(p)?.text().to_string())))
                            .collect();
                        for (p, t) in texts {
                            self.world().set_overlay(&p, t);
                        }
                    }
                }
                let _ = self.ui.send_event(UiEvent::Config(self.cfg.clone()));
                if let Some(t) = self.target.clone() {
                    self.retarget(&t);
                }
                self.dirty = true;
            }
            CoreMsg::WorkspaceRoots(roots) => self.roots = roots,
            CoreMsg::Shutdown => return false,
        }
        true
    }

    /// The editor is active in `path`: follow it unless pinned, or it's part
    /// of the current document.
    fn on_activity(&mut self, path: &Path) {
        if !is_typst(path) || self.cfg.main.is_some() {
            return;
        }
        match &self.target {
            Some(t) if t == path || self.deps.contains(path) => {}
            _ => self.retarget(path),
        }
    }

    fn workspace_root_for(&self, path: &Path) -> Option<PathBuf> {
        self.roots.iter().filter(|r| path.starts_with(r)).max_by_key(|r| r.as_os_str().len()).cloned()
    }

    /// Picks the main file (config or `path`) and the project root.
    fn retarget(&mut self, path: &Path) {
        let base = self.workspace_root_for(path).or_else(|| self.roots.first().cloned());
        let resolve = |p: &str| {
            let p = PathBuf::from(p);
            let p = if p.is_absolute() { p } else { base.clone().unwrap_or_default().join(p) };
            std::fs::canonicalize(&p).unwrap_or(p)
        };
        let main = self.cfg.main.as_deref().map(&resolve).unwrap_or_else(|| path.to_path_buf());
        let root = self
            .cfg
            .root
            .as_deref()
            .map(&resolve)
            .or_else(|| self.workspace_root_for(&main))
            .or_else(|| main.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("/"));
        let root = if main.starts_with(&root) { root } else { main.parent().unwrap_or(Path::new("/")).to_path_buf() };

        if !self.world().set_target(&root, &main) {
            log::warn!("cannot preview {main:?}: outside of root {root:?}");
            return;
        }
        if self.target.as_ref() != Some(&main) {
            log::info!("preview target: {main:?} (root {root:?})");
            self.target = Some(main);
            self.deps.clear();
            self.doc = None;
            self.last_hashes.clear();
            self.dirty = true;
            self.send_title();
        }
    }

    fn title(&self) -> String {
        let name = self
            .target
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Typst".into());
        format!("{name} — Typst Live Preview")
    }

    fn send_title(&mut self) {
        if let Some(path) = self.target.clone() {
            let _ = self.ui.send_event(UiEvent::Title { title: self.title(), path });
        }
    }

    fn show_window(&mut self) {
        self.window_open = true;
        self.shared.window_open.store(true, Ordering::Relaxed);
        self.dirty = true;
        self.cursor_changed = true;
        let _ = self.ui.send_event(UiEvent::Show { title: self.title(), cfg: self.cfg.clone(), path: self.target.clone() });
    }

    fn compile(&mut self) {
        self.dirty = false;
        let edit_received = self.edit_received;
        let world = self.world();
        world.begin_revision();
        let start = Instant::now();
        let warned = typst::compile::<PagedDocument>(&*world);
        let elapsed = start.elapsed();
        let deps = world.dependencies();
        typst::comemo::evict(10);

        match warned.output {
            Ok(doc) => {
                let prev = self.doc.clone();
                let pages: Vec<PageInfo> = doc
                    .pages()
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let size = p.frame.size();
                        let hash = typst::utils::hash128(&(&p.frame, &p.fill));
                        // Changed page: find out which parts of it changed.
                        let diff = match (self.last_hashes.get(i), prev.as_ref().and_then(|d| d.pages().get(i))) {
                            (Some(&old_hash), Some(old)) if old_hash != hash => {
                                diff::dirty_bands(old, p).map(|bands| (old_hash, bands))
                            }
                            _ => None,
                        };
                        PageInfo { width: size.x.to_pt(), height: size.y.to_pt(), hash, diff }
                    })
                    .collect();
                let status = Status { error: None, warnings: warned.warnings.len(), compile_ms: elapsed.as_secs_f64() * 1000.0 };
                self.deps = deps;
                // Nothing visible changed (e.g. an edit in a comment).
                if self.last_hashes.len() == pages.len()
                    && self.last_hashes.iter().zip(&pages).all(|(h, p): (&u128, &PageInfo)| *h == p.hash)
                    && self.doc.is_some()
                {
                    let _ = self.ui.send_event(UiEvent::Status(status));
                    self.edit_received = None;
                    let _ = edit_received;
                    return;
                }
                self.last_hashes = pages.iter().map(|p: &PageInfo| p.hash).collect();
                let doc = Arc::new(doc);
                self.doc = Some(doc.clone());
                self.doc_generation += 1;
                log::debug!("compiled in {elapsed:?}");
                let _ = self.ui.send_event(UiEvent::Document(DocUpdate {
                    doc,
                    pages,
                    generation: self.doc_generation,
                    status,
                    edit_received: self.edit_received.take(),
                }));
                self.cursor_changed = true;
            }
            Err(errors) => {
                if !deps.is_empty() {
                    self.deps.extend(deps);
                }
                let world = self.world.as_ref().unwrap();
                let first = errors.iter().find(|e| e.severity == Severity::Error).or(errors.first());
                let message = first.map(|e| {
                    let loc = e
                        .span
                        .id()
                        .and_then(|id| {
                            let src = world.source(id).ok()?;
                            let range = world.range(e.span)?;
                            let (line, col) = position::to_position(&src, range.start);
                            let name = id.vpath().file_name().unwrap_or("").to_string();
                            Some(format!("{name}:{}:{}: ", line + 1, col + 1))
                        })
                        .unwrap_or_default();
                    format!("{loc}{}", e.message)
                });
                log::debug!("compile error: {message:?}");
                let _ = self.ui.send_event(UiEvent::Status(Status {
                    error: Some((errors.len(), message.unwrap_or_default())),
                    warnings: warned.warnings.len(),
                    compile_ms: elapsed.as_secs_f64() * 1000.0,
                }));
            }
        }
    }

    /// Sends the cursor indicator positions to the preview.
    fn send_cursor(&mut self) {
        if !self.cfg.follow_cursor || !self.window_open {
            return;
        }
        let Some((path, offset)) = self.cursor.clone() else { return };
        let Some(doc) = self.doc.clone() else { return };
        let Some(src) = self.world().source_for_path(&path) else { return };
        let markers = cursor_markers(&doc, &src, offset);
        if !markers.is_empty() {
            let _ = self.ui.send_event(UiEvent::Cursor(markers));
        }
    }

    fn click(&mut self, page: usize, x: f64, y: f64) {
        let Some(doc) = self.doc.clone() else { return };
        let Some(p) = doc.pages().get(page) else { return };
        let _ = p;
        let jump = resolve_click(self.world(), &doc, page, x, y);
        match jump {
            Some(Jump::File(id, offset)) => {
                let world = self.world.as_ref().unwrap();
                let (Some(path), Ok(src)) = (world.path_of(id), world.source(id)) else { return };
                let (line, character) = position::to_position(&src, offset);
                log::info!("jump to {}:{}:{}", path.display(), line + 1, character + 1);
                self.editor.show_document(&path, line, character);
                if self.cfg.focus_editor_on_jump {
                    let _ = self.ui.send_event(UiEvent::ActivateEditor);
                }
                // Show the indicator right away; the editor confirms later.
                let markers = cursor_markers(&doc, &src, offset);
                if !markers.is_empty() && self.cfg.follow_cursor {
                    let _ = self.ui.send_event(UiEvent::CursorNoScroll(markers));
                }
                if let Some(path) = world.path_of(id) {
                    self.cursor = Some((path, offset));
                }
            }
            Some(Jump::Url(url)) => open_url(url.as_str()),
            Some(Jump::Position(p)) => {
                let _ = self.ui.send_event(UiEvent::Reveal(Marker {
                    page: p.page.get() - 1,
                    x: p.point.x.to_pt(),
                    y: p.point.y.to_pt(),
                    height: 0.0,
                }));
            }
            None => {}
        }
    }
}

/// Resolves a click on page `page` (0-based) at `x`,`y` (pt).
pub fn resolve_click(world: &PreviewWorld, doc: &PagedDocument, page: usize, x: f64, y: f64) -> Option<Jump> {
    let frame = &doc.pages().get(page)?.frame;
    let click = Point::new(Abs::pt(x), Abs::pt(y));
    let pos = |point| PagedPosition { page: NonZeroUsize::new(page + 1).unwrap(), point };
    typst_ide::jump_from_click(world, doc, &pos(click)).or_else(|| {
        // Clicked between glyphs or beside a line: use the nearest text.
        let near = nearest_glyph(frame, click, Abs::pt(24.0))?;
        typst_ide::jump_from_click(world, doc, &pos(near))
    })
}

fn open_url(url: &str) {
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:")) {
        return;
    }
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(url).spawn();
}

/// Finds where the text at `offset` is shown in the document, with the
/// precise caret x-position inside the glyph run.
pub fn cursor_markers(doc: &PagedDocument, source: &Source, offset: usize) -> Vec<Marker> {
    fn is_text(node: &LinkedNode) -> bool {
        matches!(node.kind(), SyntaxKind::Text | SyntaxKind::MathText)
    }
    let root = LinkedNode::new(source.root());
    let Some(node) = root
        .leaf_at(offset, Side::Before)
        .filter(is_text)
        .or_else(|| root.leaf_at(offset, Side::After).filter(is_text))
    else {
        return vec![];
    };
    let span = node.span();
    let rel = offset.saturating_sub(node.offset());

    let mut best: Vec<Marker> = Vec::new();
    let mut best_key = None;
    for (i, page) in doc.pages().iter().enumerate() {
        find_caret(&page.frame, Transform::identity(), span, rel, &mut |key, point, size| {
            let marker = Marker { page: i, x: point.x.to_pt(), y: point.y.to_pt(), height: size.to_pt() };
            match best_key {
                Some(k) if key < k => {}
                Some(k) if key == k => {
                    // Same glyph shown repeatedly (e.g. in a header) or a
                    // second glyph run: keep one marker per page.
                    if !best.iter().any(|m| m.page == i) {
                        best.push(marker);
                    }
                }
                _ => {
                    best_key = Some(key);
                    best = vec![marker];
                }
            }
        });
    }
    best
}

/// Visits glyphs belonging to `span`, reporting `(key, baseline point, font
/// size)` for the best caret position in each glyph run. A larger key is a
/// closer match to the cursor offset `rel` inside the node.
fn find_caret(frame: &Frame, ts: Transform, span: Span, rel: usize, f: &mut dyn FnMut((usize, bool), Point, Abs)) {
    for (pos, item) in frame.items() {
        match item {
            FrameItem::Group(group) => {
                let child = ts.pre_concat(Transform::translate(pos.x, pos.y)).pre_concat(group.transform);
                find_caret(&group.frame, child, span, rel, f);
            }
            FrameItem::Text(text) => {
                let mut x = pos.x;
                let mut found: Option<((usize, bool), Abs)> = None;
                for glyph in &text.glyphs {
                    let adv = glyph.x_advance.at(text.size);
                    if glyph.span.0 == span {
                        let start = glyph.span.1 as usize;
                        let len = glyph.range().len();
                        if start <= rel {
                            let after = rel >= start + len;
                            let key = (start, after);
                            if found.is_none_or(|(k, _)| key > k) {
                                found = Some((key, if after { x + adv } else { x }));
                            }
                        }
                    }
                    x += adv;
                }
                if let Some((key, x)) = found {
                    let p = Point::new(x, pos.y).transform(ts);
                    f(key, p, text.size);
                }
            }
            _ => {}
        }
    }
}

/// Finds the center of the glyph closest to `click`. Text on the same line
/// is preferred; text more than `max` above or below is ignored.
fn nearest_glyph(frame: &Frame, click: Point, max: Abs) -> Option<Point> {
    fn walk(frame: &Frame, ts: Transform, click: Point, max: f64, best: &mut Option<(f64, Point)>) {
        for (pos, item) in frame.items() {
            match item {
                FrameItem::Group(group) => {
                    let child = ts.pre_concat(Transform::translate(pos.x, pos.y)).pre_concat(group.transform);
                    walk(&group.frame, child, click, max, best);
                }
                FrameItem::Text(text) => {
                    let mut x = pos.x;
                    for glyph in &text.glyphs {
                        let adv = glyph.x_advance.at(text.size);
                        if glyph.span.0.id().is_some() {
                            let a = Point::new(x, pos.y - text.size).transform(ts);
                            let b = Point::new(x + adv, pos.y).transform(ts);
                            let (x0, x1) = (a.x.min(b.x).to_pt(), a.x.max(b.x).to_pt());
                            let (y0, y1) = (a.y.min(b.y).to_pt(), a.y.max(b.y).to_pt());
                            let (cx, cy) = (click.x.to_pt(), click.y.to_pt());
                            let dx = (x0 - cx).max(cx - x1).max(0.0);
                            let dy = (y0 - cy).max(cy - y1).max(0.0);
                            let score = dx + 8.0 * dy;
                            if dy <= max && best.is_none_or(|(s, _)| score < s) {
                                let center = Point::new(Abs::pt((x0 + x1) / 2.0), Abs::pt((y0 + y1) / 2.0));
                                *best = Some((score, center));
                            }
                        }
                        x += adv;
                    }
                }
                _ => {}
            }
        }
    }
    let mut best = None;
    walk(frame, Transform::identity(), click, max.to_pt(), &mut best);
    best.map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "#set page(width: 300pt, height: auto, margin: 20pt)\n= Title\n\nHello brave new world.\n\nSecond paragraph with $x^2$ math.\n";

    fn setup() -> (tempdir::Dir, PreviewWorld, PathBuf) {
        let dir = tempdir::Dir::new();
        let path = dir.0.join("main.typ");
        std::fs::write(&path, DOC).unwrap();
        let fonts = Arc::new(world::load_fonts(&[], false));
        let mut w = PreviewWorld::new(fonts, Arc::new(world::packages()));
        w.set_overlay(&path, DOC.to_string());
        assert!(w.set_target(&dir.0, &path));
        (dir, w, path)
    }

    fn compile(w: &mut PreviewWorld) -> PagedDocument {
        w.begin_revision();
        typst::compile::<PagedDocument>(&*w).output.expect("compiles")
    }

    #[test]
    fn cursor_and_click_roundtrip() {
        let (_dir, mut w, path) = setup();
        let doc = compile(&mut w);
        let src = w.source_for_path(&path).unwrap();
        let word = DOC.find("brave").unwrap();

        // Source → preview: caret before "b" of "brave".
        let markers = cursor_markers(&doc, &src, word);
        assert_eq!(markers.len(), 1);
        let m = markers[0];
        assert!(m.height > 5.0);

        // Preview → source: clicking slightly right of the caret, inside "b".
        let jump = resolve_click(&w, &doc, m.page, m.x + 1.0, m.y - m.height / 3.0).unwrap();
        let Jump::File(id, offset) = jump else { panic!("expected a file jump, got {jump:?}") };
        assert_eq!(w.path_of(id).unwrap(), path);
        assert!((word..=word + 1).contains(&offset), "offset {offset}, word at {word}");
        assert_eq!(position::to_position(&src, offset).0, 3);
    }

    #[test]
    fn click_beside_text_uses_nearest_glyph() {
        let (_dir, mut w, path) = setup();
        let doc = compile(&mut w);
        let src = w.source_for_path(&path).unwrap();
        let end = DOC.find("world.").unwrap() + "world.".len();
        let m = cursor_markers(&doc, &src, end)[0];
        // Far right of the line end, in empty space.
        let jump = resolve_click(&w, &doc, m.page, 280.0, m.y - m.height / 3.0).unwrap();
        let Jump::File(_, offset) = jump else { panic!() };
        assert_eq!(position::to_position(&src, offset).0, 3, "jumps to the same line");
    }

    #[test]
    fn incremental_edit_updates_document() {
        let (_dir, mut w, path) = setup();
        let before = compile(&mut w);
        let at = DOC.find("world").unwrap();
        w.overlay_mut(&path).unwrap().edit(at..at + 5, "Typst");
        let after = compile(&mut w);
        let text = |d: &PagedDocument| typst::utils::hash128(&d.pages()[0].frame);
        assert_ne!(text(&before), text(&after));
        let src = w.source_for_path(&path).unwrap();
        assert!(src.text().contains("brave new Typst."));
    }

    /// Minimal temporary directory (avoids a dev-dependency).
    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let p = std::env::temp_dir().join(format!(
                    "tlp-test-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Self(std::fs::canonicalize(&p).unwrap())
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
