//! The server's state: open buffers, the previewed main file, the latest
//! PDF and SyncTeX data. Runs on its own thread; the language server, the
//! connection to Zed and the compiler talk to it through `CoreMsg`s.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde_json::{Value, json};

use crate::config::Config;
use crate::pdf::PdfDoc;
use crate::project::{self, Job, Outcome, canonical};
use crate::render::{self, RenderPool};
use crate::synctex::{Marker, SyncTex};
use crate::zed::ZedOut;

pub type ExportReply = Sender<Result<PathBuf, String>>;

pub enum CoreMsg {
    Open { path: PathBuf, text: String },
    Change { path: PathBuf, text: String },
    Close { path: PathBuf },
    FilesChanged(Vec<PathBuf>),
    Config(Config),
    Roots(Vec<PathBuf>),
    /// The user asked for the preview (toolbar button, code action).
    OpenPreview { path: Option<PathBuf>, split: bool, explicit: bool },
    ExportPdf { path: Option<PathBuf>, reply: ExportReply },
    /// A message from Zed's preview pane.
    Zed(Value),
    ZedClosed,
    Compiled(Box<Outcome>),
    Shutdown,
}

/// State the language server thread reads.
#[derive(Default)]
pub struct Shared {
    /// A preview pane is open.
    pub attached: AtomicBool,
}

/// Files TeX writes next to the sources: changes to them must not trigger
/// a compilation.
fn is_generated(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    const EXT: &[&str] = &[
        ".aux", ".log", ".fls", ".fdb_latexmk", ".synctex.gz", ".synctex", ".out", ".toc", ".lof", ".lot", ".bbl",
        ".blg", ".idx", ".ind", ".ilg", ".nav", ".snm", ".vrb", ".run.xml", ".bcf", ".xdv", ".dvi", ".swp",
        ".tmp",
    ];
    EXT.iter().any(|e| name.ends_with(e)) || path.components().any(|c| c.as_os_str() == ".git")
}

fn is_tex(path: &Path) -> bool {
    path.extension().is_some_and(|e| ["tex", "ltx", "latex"].iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

struct Core {
    cfg: Config,
    roots: Vec<PathBuf>,
    buffers: HashMap<PathBuf, Arc<str>>,
    zed: Option<ZedOut>,
    pool: Option<RenderPool>,
    shared: Arc<Shared>,
    attached: bool,
    auto_opened: bool,

    /// The previewed main file.
    main: Option<PathBuf>,
    /// Files the main file read in its last compilation.
    deps: HashSet<PathBuf>,
    doc: Option<Arc<PdfDoc>>,
    synctex: Option<SyncTex>,
    generation: u64,

    compiler: Sender<Job>,
    /// The id of the running compilation.
    running: Option<u64>,
    next_id: u64,
    /// Compile the preview when this time has come.
    due: Option<Instant>,
    /// Main file, first compilation id that may answer, reply.
    exports: Vec<(PathBuf, u64, ExportReply)>,

    last_cursor: Option<(PathBuf, u32, u32)>,
    /// Where a click in the preview moved the editor: the cursor event that
    /// follows must not scroll the preview.
    jumped: Option<(PathBuf, u32)>,
}

/// Starts the core. Messages for it go to `tx` (whose receiver is `rx`).
pub fn spawn(
    cfg: Config,
    zed: Option<ZedOut>,
    shared: Arc<Shared>,
    tx: Sender<CoreMsg>,
    rx: Receiver<CoreMsg>,
) -> std::thread::JoinHandle<()> {
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<Job>();
    {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("compile".into())
            .spawn(move || {
                for job in job_rx {
                    let outcome = project::compile(job);
                    if tx.send(CoreMsg::Compiled(Box::new(outcome))).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn compile thread");
    }
    std::thread::Builder::new()
        .name("core".into())
        .spawn(move || {
            let pool = zed.clone().map(RenderPool::new);
            let mut core = Core {
                cfg,
                roots: Vec::new(),
                buffers: HashMap::new(),
                zed,
                pool,
                shared,
                attached: false,
                auto_opened: false,
                main: None,
                deps: HashSet::new(),
                doc: None,
                synctex: None,
                generation: 0,
                compiler: job_tx,
                running: None,
                next_id: 1,
                due: None,
                exports: Vec::new(),
                last_cursor: None,
                jumped: None,
            };
            core.run(rx);
        })
        .expect("spawn core thread")
}

impl Core {
    fn run(&mut self, rx: Receiver<CoreMsg>) {
        loop {
            let msg = match self.due.filter(|_| self.running.is_none()) {
                Some(due) => match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return,
                },
            };
            match msg {
                Some(CoreMsg::Shutdown) => return,
                Some(m) => self.handle(m),
                None => {}
            }
            self.start_compile();
        }
    }

    fn send(&self, v: Value) {
        if let Some(z) = &self.zed {
            z.send(v);
        }
    }

    fn handle(&mut self, msg: CoreMsg) {
        match msg {
            CoreMsg::Open { path, text } => {
                let tex = is_tex(&path);
                self.buffers.insert(path.clone(), text.into());
                if tex && self.cfg.auto_open && !self.auto_opened && !self.attached {
                    self.auto_opened = true;
                    self.open_preview(Some(path), true, false);
                }
            }
            CoreMsg::Change { path, text } => {
                self.buffers.insert(path.clone(), text.into());
                self.source_changed(&path);
            }
            CoreMsg::Close { path } => {
                if self.buffers.remove(&path).is_some() {
                    self.source_changed(&path);
                }
            }
            CoreMsg::FilesChanged(paths) => {
                // Exporting writes `<main>.pdf`.
                let exported = self.main.as_ref().map(|m| m.with_extension("pdf"));
                let relevant: Vec<PathBuf> =
                    paths.into_iter().filter(|p| !is_generated(p) && Some(p) != exported.as_ref()).collect();
                for p in relevant {
                    // Open buffers win over the disk.
                    if !self.buffers.contains_key(&p) {
                        self.source_changed(&p);
                    }
                }
            }
            CoreMsg::Config(cfg) => {
                let recompile = cfg.main != self.cfg.main
                    || cfg.engine != self.cfg.engine
                    || cfg.command != self.cfg.command
                    || cfg.latexmk_args != self.cfg.latexmk_args;
                if cfg.invert_colors != self.cfg.invert_colors {
                    self.send(json!({ "type": "invalidate" }));
                }
                self.cfg = cfg;
                if recompile && let Some((path, _, _)) = self.last_cursor.clone() {
                    self.set_target(&path);
                    self.schedule(Duration::ZERO);
                }
            }
            CoreMsg::Roots(roots) => {
                for r in roots {
                    if !self.roots.contains(&r) {
                        self.roots.push(r);
                    }
                }
                self.send(json!({ "type": "roots", "roots": self.roots }));
            }
            CoreMsg::OpenPreview { path, split, explicit } => self.open_preview(path, split, explicit),
            CoreMsg::ExportPdf { path, reply } => {
                let Some(file) = path.or_else(|| self.main.clone()) else {
                    let _ = reply.send(Err("no TeX file to export".into()));
                    return;
                };
                let main = self.resolve_main(&file);
                if self.main.as_ref() == Some(&main) && self.attached {
                    // Compile the latest edits first.
                    self.schedule(Duration::ZERO);
                }
                // A compilation that is running may miss the latest edits.
                self.exports.push((main, self.next_id, reply));
            }
            CoreMsg::Zed(v) => self.handle_zed(v),
            CoreMsg::ZedClosed => {
                log::info!("Zed closed the preview connection");
                self.zed = None;
                self.attached = false;
            }
            CoreMsg::Compiled(outcome) => self.compiled(*outcome),
            CoreMsg::Shutdown => {}
        }
        self.shared.attached.store(self.attached, Ordering::Relaxed);
    }

    fn open_preview(&mut self, path: Option<PathBuf>, split: bool, explicit: bool) {
        let path = path.or_else(|| self.last_cursor.as_ref().map(|c| c.0.clone())).or_else(|| self.main.clone());
        if let Some(path) = path {
            self.send(json!({ "type": "open_preview", "path": path, "split": split, "explicit": explicit }));
        }
    }

    // -----------------------------------------------------------------------
    // Main file

    fn text_of(&self, path: &Path) -> Option<String> {
        self.buffers.get(path).map(|t| t.to_string()).or_else(|| std::fs::read_to_string(path).ok())
    }

    fn root_for(&self, path: &Path) -> Option<&Path> {
        self.roots.iter().filter(|r| path.starts_with(r)).max_by_key(|r| r.as_os_str().len()).map(PathBuf::as_path)
    }

    /// The file to compile to preview `file`.
    fn resolve_main(&self, file: &Path) -> PathBuf {
        if let Some(main) = &self.cfg.main {
            let main = Path::new(main);
            if main.is_absolute() {
                return canonical(main);
            }
            let base = self.root_for(file).map(Path::to_path_buf).or_else(|| file.parent().map(Path::to_path_buf));
            return canonical(&base.unwrap_or_default().join(main));
        }
        let text = self.text_of(file).unwrap_or_default();
        if let Some(root) = project::magic_comment(&text, "root") {
            return canonical(&file.parent().unwrap_or(Path::new("/")).join(root));
        }
        // Editing a file that belongs to the previewed document.
        if let Some(main) = &self.main
            && self.deps.contains(file)
        {
            return main.clone();
        }
        if project::document_class(&text).is_some() {
            return file.to_path_buf();
        }
        let text_of = |p: &Path| self.text_of(p);
        project::search_main(file, self.root_for(file), &text_of).unwrap_or_else(|| file.to_path_buf())
    }

    /// Previews the document `file` belongs to.
    fn set_target(&mut self, file: &Path) {
        let main = self.resolve_main(file);
        if self.main.as_ref() == Some(&main) {
            return;
        }
        log::info!("previewing {}", main.display());
        self.main = Some(main.clone());
        self.deps.clear();
        self.doc = None;
        self.synctex = None;
        let title = main.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.send(json!({ "type": "target", "path": main, "title": title }));
        self.schedule(Duration::ZERO);
    }

    fn source_changed(&mut self, path: &Path) {
        let Some(main) = &self.main else { return };
        let related = self.deps.contains(path) || main.parent().is_some_and(|d| path.starts_with(d));
        if related && self.attached {
            self.schedule(Duration::from_millis(self.cfg.debounce_ms));
        }
    }

    fn schedule(&mut self, delay: Duration) {
        let at = Instant::now() + delay;
        self.due = Some(match self.due {
            // A pending compilation that is due sooner stays (edits arrive
            // while a compilation runs).
            Some(d) if delay.is_zero() => d.min(at),
            _ => at,
        });
    }

    // -----------------------------------------------------------------------
    // Compilation

    fn start_compile(&mut self) {
        if self.running.is_some() {
            return;
        }
        let preview = match (&self.main, self.due) {
            (Some(main), Some(due)) if self.attached && due <= Instant::now() => Some(main.clone()),
            _ => None,
        };
        let main = match preview {
            Some(m) => {
                self.due = None;
                m
            }
            // Exports of documents that are not previewed.
            None => match self.exports.iter().map(|(m, _, _)| m).find(|m| !self.attached || Some(*m) != self.main.as_ref()) {
                Some(m) => m.clone(),
                None => return,
            },
        };
        let id = self.next_id;
        self.next_id += 1;
        self.running = Some(id);
        let reload = self.main.as_ref() != Some(&main) || self.doc.is_none();
        let buffers = self.buffers.iter().map(|(p, t)| (p.clone(), t.clone())).collect();
        let _ = self.compiler.send(Job { id, main, buffers, cfg: self.cfg.clone(), reload });
    }

    fn compiled(&mut self, outcome: Outcome) {
        if self.running == Some(outcome.id) {
            self.running = None;
        }
        log::info!(
            "compiled {} in {} ms: {} error(s), {} warning(s){}",
            outcome.main.display(),
            outcome.ms,
            outcome.errors.len(),
            outcome.warnings,
            if outcome.doc.is_some() { ", new PDF" } else { "" }
        );
        self.finish_exports(&outcome);
        if self.main.as_ref() != Some(&outcome.main) {
            return;
        }
        if !outcome.deps.is_empty() {
            self.deps = outcome.deps;
        }
        if let Some(doc) = outcome.doc {
            self.doc = Some(doc);
            self.synctex = outcome.synctex;
            self.generation += 1;
            self.send_document(outcome.ms, outcome.warnings);
        }
        self.send(json!({
            "type": "status",
            "error": outcome.errors.first(),
            "errors": outcome.errors.len(),
            "compile_ms": outcome.ms,
            "warnings": outcome.warnings,
        }));
        if let Some((path, line, character)) = self.last_cursor.clone() {
            self.forward(&path, line, character, false);
        }
    }

    fn finish_exports(&mut self, outcome: &Outcome) {
        let (done, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.exports)
            .into_iter()
            .partition(|(m, first_id, _)| *m == outcome.main && outcome.id >= *first_id);
        self.exports = rest;
        for (main, _, reply) in done {
            let result = match (&outcome.pdf_path, outcome.errors.first()) {
                (_, Some(e)) if outcome.doc.is_none() => Err(e.clone()),
                (Some(pdf), _) => {
                    let target = main.with_extension("pdf");
                    std::fs::copy(pdf, &target).map(|_| target).map_err(|e| format!("cannot write the PDF: {e}"))
                }
                (None, _) => Err("the compilation produced no PDF".to_string()),
            };
            let _ = reply.send(result);
        }
    }

    fn send_document(&self, compile_ms: u64, warnings: usize) {
        let Some(doc) = &self.doc else { return };
        let pages: Vec<Value> = doc
            .pages
            .iter()
            .map(|p| json!({ "width": p.width, "height": p.height, "hash": format!("{:x}", p.hash) }))
            .collect();
        self.send(json!({
            "type": "document",
            "generation": self.generation,
            "pages": pages,
            "compile_ms": compile_ms,
            "warnings": warnings,
        }));
    }

    // -----------------------------------------------------------------------
    // Messages from the pane

    fn handle_zed(&mut self, h: Value) {
        let path = |key: &str| h.get(key).and_then(Value::as_str).map(|p| canonical(Path::new(p)));
        let num = |key: &str| h.get(key).and_then(Value::as_f64);
        match h.get("type").and_then(Value::as_str).unwrap_or("") {
            "attach" => {
                let Some(path) = path("path") else { return };
                self.attached = true;
                let before = self.main.clone();
                self.set_target(&path);
                if before == self.main {
                    // A new pane for the same document: give it what we have.
                    if let Some(main) = &self.main {
                        let title = main.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        self.send(json!({ "type": "target", "path": main, "title": title }));
                    }
                    self.send_document(0, 0);
                    // Edits made while no pane was open.
                    self.schedule(Duration::ZERO);
                }
            }
            "detach" => self.attached = false,
            "cursor" => {
                let (Some(path), Some(line), Some(character)) = (path("path"), num("line"), num("character")) else {
                    return;
                };
                let (line, character) = (line as u32, character as u32);
                self.last_cursor = Some((path.clone(), line, character));
                if self.attached && is_tex(&path) && self.main.as_ref() != Some(&path) && !self.deps.contains(&path) {
                    // Another document (only once it compiled: until then its
                    // dependencies are unknown).
                    if self.doc.is_some() || self.main.is_none() {
                        self.set_target(&path);
                    }
                }
                let scroll = self.jumped.take() != Some((path.clone(), line));
                self.forward(&path, line, character, scroll);
            }
            "click" => {
                let (Some(page), Some(x), Some(y)) = (num("page"), num("x"), num("y")) else { return };
                self.backward(page as usize, x, y);
            }
            "render" => self.render(&h),
            // Pages have a fixed size (`layout: fixed`).
            "viewport" => {}
            other => log::debug!("unknown message from Zed: {other}"),
        }
    }

    fn render(&mut self, h: &Value) {
        let Some(pool) = &self.pool else { return };
        let key = |v: &Value| -> Option<(u128, u32)> {
            let hash = u128::from_str_radix(v.get("hash")?.as_str()?, 16).ok()?;
            Some((hash, v.get("scale_milli")?.as_u64()? as u32))
        };
        let wanted: HashSet<(u128, u32)> =
            h.get("wanted").and_then(Value::as_array).map(|a| a.iter().filter_map(key).collect()).unwrap_or_default();
        pool.set_wanted(wanted);
        let invert = self.cfg.invert_colors;
        for req in h.get("requests").and_then(Value::as_array).into_iter().flatten() {
            let (Some(index), Some((hash, scale))) = (req.get("index").and_then(Value::as_u64), key(req)) else {
                continue;
            };
            let field = |name: &str| req.get(name).and_then(Value::as_u64).map(|v| v as u32);
            let strips = match (field("strip_px"), field("first"), field("end")) {
                (Some(px), Some(first), Some(end)) if px > 0 && first < end => Some((px, first, end)),
                _ => None,
            };
            let current = self.doc.as_ref().filter(|d| d.pages.get(index as usize).is_some_and(|p| p.hash == hash));
            match current {
                Some(doc) => pool.submit(render::Job {
                    doc: doc.clone(),
                    index: index as usize,
                    key: (hash, scale, invert),
                    strips,
                }),
                None => {
                    // Stale request (the document changed meanwhile).
                    let mut v = json!({ "type": "skipped", "hash": format!("{hash:x}"), "scale_milli": scale });
                    if let Some((_, first, end)) = strips {
                        v["first"] = first.into();
                        v["end"] = end.into();
                    }
                    self.send(v);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // SyncTeX

    fn line_text(&self, path: &Path, line: u32) -> String {
        self.text_of(path).and_then(|t| t.lines().nth(line as usize).map(str::to_string)).unwrap_or_default()
    }

    /// Shows where the cursor (`line`, UTF-16 `character`, 0-based) is.
    fn forward(&self, path: &Path, line: u32, character: u32, scroll: bool) {
        if !self.cfg.follow_cursor || !self.attached {
            return;
        }
        let Some(st) = &self.synctex else { return };
        let len = utf16_len(&self.line_text(path, line));
        let frac = if len == 0 { 0.0 } else { character as f64 / len as f64 };
        let Some(m) = st.forward(path, line + 1, frac) else { return };
        let Marker { page, x, y, height } = m;
        self.send(json!({
            "type": "cursor",
            "scroll": scroll,
            "markers": [{ "page": page, "x": x, "y": y, "height": height }],
        }));
    }

    /// Moves the editor to what was clicked.
    fn backward(&mut self, page: usize, x: f64, y: f64) {
        let Some(st) = &self.synctex else { return };
        let Some((path, line, frac)) = st.backward(page, x, y) else { return };
        let line = line.saturating_sub(1);
        let text = self.line_text(&path, line);
        // Skip the indentation: `frac` 0 is the first thing on the line.
        let indent = utf16_len(&text[..text.len() - text.trim_start().len()]);
        let len = utf16_len(&text);
        let character = indent + ((len - indent) as f64 * frac).round() as usize;
        self.jumped = Some((path.clone(), line));
        self.send(json!({ "type": "jump", "path": path, "line": line, "character": character.min(len) }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_files() {
        assert!(is_generated(Path::new("/p/main.aux")));
        assert!(is_generated(Path::new("/p/main.synctex.gz")));
        assert!(is_generated(Path::new("/p/.git/index")));
        assert!(!is_generated(Path::new("/p/fig.png")));
        assert!(!is_generated(Path::new("/p/refs.bib")));
    }
}
