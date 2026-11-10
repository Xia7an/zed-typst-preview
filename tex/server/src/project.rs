//! Finding the main file of a document and compiling it.
//!
//! Compilation runs latexmk (or a configured command) in the directory of the
//! main file, with all output in a private directory. Unsaved editor buffers
//! are written to an overlay directory that is put first on `TEXINPUTS` (and
//! `BIBINPUTS`), so TeX reads them instead of the files on disk: this works
//! for the main file and for `\input{sec/a}`-style relative names (not for
//! names starting with `./` or `../`, which kpathsea does not search).

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::BufReader;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crate::config::Config;
use crate::pdf::PdfDoc;
use crate::synctex::SyncTex;

// ---------------------------------------------------------------------------
// Source text helpers

/// Drops a `%` comment (an unescaped `%`) from a line.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'%' => return &line[..i],
            _ => i += 1,
        }
    }
    line
}

/// `% !TEX <key> = value` in the first lines (TeXShop / VS Code convention).
pub fn magic_comment(text: &str, key: &str) -> Option<String> {
    for line in text.lines().take(50) {
        let Some(rest) = line.trim_start().strip_prefix('%') else { continue };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('!') else { continue };
        let rest = rest.trim_start();
        if rest.len() < 3 || !rest[..3].eq_ignore_ascii_case("tex") {
            continue;
        }
        let Some((k, v)) = rest[3..].split_once('=') else { continue };
        let k = k.trim();
        if k.eq_ignore_ascii_case(key) || (key.eq_ignore_ascii_case("program") && k.eq_ignore_ascii_case("ts-program")) {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// The class and its options: `\documentclass[opts]{class}`.
pub fn document_class(text: &str) -> Option<(String, String)> {
    for line in text.lines() {
        let line = strip_comment(line);
        let Some(i) = line.find("\\documentclass") else { continue };
        let rest = &line[i + "\\documentclass".len()..];
        let (opts, rest) = match rest.trim_start().strip_prefix('[') {
            Some(r) => r.split_once(']').unwrap_or((r, "")),
            None => ("", rest),
        };
        let class = rest.trim_start().strip_prefix('{').and_then(|r| r.split_once('}')).map(|(c, _)| c.trim().to_string());
        return Some((class.unwrap_or_default(), opts.to_string()));
    }
    None
}

fn uses_package(text: &str, names: &[&str]) -> bool {
    for line in text.lines().map(strip_comment) {
        for cmd in ["\\usepackage", "\\RequirePackage"] {
            let Some(i) = line.find(cmd) else { continue };
            let rest = line[i + cmd.len()..].trim_start();
            let rest = match rest.strip_prefix('[') {
                Some(r) => r.split_once(']').map(|(_, r)| r).unwrap_or(""),
                None => rest,
            };
            let body = rest.trim_start().strip_prefix('{').and_then(|r| r.split_once('}')).map(|(b, _)| b);
            if body.is_some_and(|b| b.split(',').any(|p| names.contains(&p.trim()))) {
                return true;
            }
        }
    }
    false
}

/// The TeX engine for a document, for `engine: auto`, in this order: a
/// `% !TEX program` comment, a latexmkrc in the document's directory that
/// sets `$pdf_mode`, the class and packages, the user's latexmkrc, pdfLaTeX.
pub fn detect_engine(text: &str, main_dir: &Path) -> String {
    let (local, global) = latexmkrc_sets_mode(main_dir);
    choose_engine(text, local, global)
}

fn choose_engine(text: &str, local_rc: bool, global_rc: bool) -> String {
    if let Some(program) = magic_comment(text, "program") {
        let p = program.to_lowercase();
        for e in ["lualatex", "xelatex", "uplatex", "platex", "pdflatex"] {
            if p.contains(e) {
                return e.into();
            }
        }
        if p.contains("latexmk") {
            return "latexmkrc".into();
        }
    }
    if local_rc {
        return "latexmkrc".into();
    }
    if let Some(engine) = engine_from_source(text) {
        return engine.into();
    }
    if global_rc { "latexmkrc".into() } else { "pdflatex".into() }
}

/// What the class (and its options) or the packages require.
fn engine_from_source(text: &str) -> Option<&'static str> {
    const ENGINES: [&str; 5] = ["lualatex", "xelatex", "uplatex", "platex", "pdflatex"];
    if let Some((class, opts)) = document_class(text) {
        let opts = opts.to_lowercase();
        if let Some(e) = ENGINES.into_iter().find(|e| opts.split(',').any(|o| o.trim() == *e)) {
            return Some(e);
        }
        let class = class.to_lowercase();
        if class.starts_with("ltj") || class == "jlreq" {
            return Some("lualatex");
        }
        if class.starts_with("js") || class.starts_with("uj") || class.starts_with("ut") {
            return Some("uplatex");
        }
        if ["jarticle", "jbook", "jreport", "tarticle", "tbook", "treport"].contains(&class.as_str()) {
            return Some("platex");
        }
    }
    if uses_package(text, &["fontspec", "unicode-math", "luatexja", "luatexja-preset", "luacode"]) {
        return Some("lualatex");
    }
    if uses_package(text, &["xeCJK", "polyglossia"]) {
        return Some("xelatex");
    }
    None
}

/// Whether a latexmkrc that latexmk will read chooses how to make the PDF:
/// (in the document's directory, the user's).
fn latexmkrc_sets_mode(main_dir: &Path) -> (bool, bool) {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(".config")));
    let sets_mode = |p: PathBuf| {
        std::fs::read_to_string(p).is_ok_and(|t| t.lines().map(|l| l.split('#').next().unwrap_or("")).any(|l| l.contains("$pdf_mode")))
    };
    let local = [main_dir.join("latexmkrc"), main_dir.join(".latexmkrc")].into_iter().any(sets_mode);
    let global = [home.map(|h| h.join(".latexmkrc")), xdg.map(|x| x.join("latexmk").join("latexmkrc"))]
        .into_iter()
        .flatten()
        .any(sets_mode);
    (local, global)
}

/// latexmk arguments that select `engine`.
fn engine_args(engine: &str) -> Vec<String> {
    let dvi = |latex: &str, bibtex: &str, makeindex: &str| {
        vec![
            "-pdfdvi".into(),
            format!("-latex={latex} %O %S"),
            "-e".into(),
            format!("$dvipdf=q/dvipdfmx %O -o %D %S/; $bibtex=q/{bibtex} %O %S/; $makeindex=q/{makeindex} %O -o %D %S/;"),
        ]
    };
    match engine {
        "latexmkrc" => vec![],
        "lualatex" => vec!["-lualatex".into()],
        "xelatex" => vec!["-xelatex".into()],
        "uplatex" => dvi("uplatex", "upbibtex", "upmendex"),
        "platex" => dvi("platex", "pbibtex", "mendex"),
        _ => vec!["-pdf".into()],
    }
}

/// Removes `.` and `..` components without touching the file system.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// `path` with symlinks resolved when it exists (like the editor's paths).
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path))
}

/// Whether `text` (a candidate main file in `dir`) includes `file`.
fn includes(text: &str, dir: &Path, file: &Path) -> bool {
    let Ok(rel) = file.strip_prefix(dir) else { return false };
    let rel = rel.to_string_lossy().replace('\\', "/");
    let no_ext = rel.strip_suffix(".tex").unwrap_or(&rel).to_string();
    let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut weak = false;
    for line in text.lines().map(strip_comment) {
        for name in [&rel, &no_ext] {
            for prefix in ["{", "{./"] {
                if line.contains(&format!("{prefix}{name}}}")) {
                    return true;
                }
            }
        }
        // `\import{sec/}{a}`, `\subimport`, `\include{chapters/a}` with
        // another base directory…
        if !stem.is_empty() && (line.contains(&format!("{{{stem}}}")) || line.contains(&format!("/{stem}}}"))) {
            weak = true;
        }
    }
    weak
}

/// Finds a main file for `file`, which has no `\documentclass`: a `.tex`
/// file with one, in its directory or a parent (up to `root`), that includes
/// it.
pub fn search_main(file: &Path, root: Option<&Path>, text_of: &dyn Fn(&Path) -> Option<String>) -> Option<PathBuf> {
    let mut dir = file.parent()?;
    for _ in 0..8 {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "tex") && p != file)
            .collect();
        entries.sort();
        for candidate in entries {
            let Some(text) = text_of(&candidate) else { continue };
            if document_class(&text).is_some() && includes(&text, dir, file) {
                return Some(canonical(&candidate));
            }
        }
        if root.is_some_and(|r| dir == r) {
            break;
        }
        dir = dir.parent()?;
    }
    None
}

// ---------------------------------------------------------------------------
// Compilation

pub struct Job {
    pub id: u64,
    pub main: PathBuf,
    /// Contents of the files open in the editor (absolute paths).
    pub buffers: Vec<(PathBuf, Arc<str>)>,
    pub cfg: Config,
    /// Load the PDF even if this run did not rewrite it (latexmk found it
    /// up to date, but the caller has no document yet).
    pub reload: bool,
}

pub struct Outcome {
    pub id: u64,
    pub main: PathBuf,
    /// A new PDF, if this run wrote one.
    pub doc: Option<Arc<PdfDoc>>,
    pub pdf_path: Option<PathBuf>,
    pub synctex: Option<SyncTex>,
    /// Files the document read (outside the TeX distribution).
    pub deps: HashSet<PathBuf>,
    /// Error messages, the first one first.
    pub errors: Vec<String>,
    pub warnings: usize,
    pub ms: u64,
}

/// Private directories for a main file: `(overlay, out)`.
pub fn work_dirs(main: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    main.hash(&mut h);
    let uid = unsafe { libc::getuid() };
    let base = std::env::temp_dir().join(format!("tex-livepreview-{uid}"));
    std::fs::create_dir_all(&base)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).ok();
    }
    let stem = main.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = canonical(&base).join(format!("{stem}-{:016x}", h.finish()));
    let (overlay, out) = (dir.join("overlay"), dir.join("out"));
    std::fs::create_dir_all(&overlay)?;
    std::fs::create_dir_all(&out)?;
    Ok((overlay, out))
}

/// Makes `overlay` hold exactly the buffers under `main_dir`, at the same
/// relative paths. Returns whether files were added or removed: latexmk
/// only notices content changes of the files TeX read last time.
fn sync_overlay(overlay: &Path, main_dir: &Path, buffers: &[(PathBuf, Arc<str>)]) -> std::io::Result<bool> {
    let wanted: HashMap<PathBuf, &str> = buffers
        .iter()
        .filter_map(|(path, text)| Some((overlay.join(path.strip_prefix(main_dir).ok()?), &**text)))
        .collect();
    // Remove stale files.
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut existing = Vec::new();
    walk(overlay, &mut existing);
    let mut added_or_removed = false;
    for path in &existing {
        if !wanted.contains_key(path) {
            std::fs::remove_file(path)?;
            added_or_removed = true;
        }
    }
    for (path, text) in wanted {
        match std::fs::read(&path) {
            Ok(old) if old == text.as_bytes() => continue,
            Ok(_) => {}
            Err(_) => added_or_removed = true,
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, text)?;
    }
    Ok(added_or_removed)
}

/// `<overlay>:<previous value>`, keeping the default path (an empty
/// element) when the variable was unset.
fn search_path(overlay: &Path, var: &str) -> OsString {
    let mut v = OsString::from(overlay);
    v.push(":");
    if let Some(old) = std::env::var_os(var) {
        v.push(old);
    }
    v
}

/// Whether `path` belongs to the TeX distribution (not the user's project).
fn is_distribution(path: &str) -> bool {
    path.contains("/texmf") || path.contains("/nix/store/") || path.contains("/texlive/") || path.contains("/TeX/")
}

/// Maps a path written by TeX (in `.fls`, `.synctex`, the log) to the
/// source file it stands for.
pub struct PathMap {
    pub main_dir: PathBuf,
    pub overlay: PathBuf,
    pub out: PathBuf,
}

impl PathMap {
    pub fn source(&self, raw: &str) -> PathBuf {
        let path = Path::new(raw);
        let path = if path.is_absolute() { normalize(path) } else { normalize(&self.main_dir.join(path)) };
        match path.strip_prefix(&self.overlay) {
            Ok(rel) => self.main_dir.join(rel),
            Err(_) => canonical(&path),
        }
    }

    /// Like `source`, but `None` for the TeX distribution and generated files.
    pub fn project_source(&self, raw: &str) -> Option<PathBuf> {
        if is_distribution(raw) {
            return None;
        }
        let path = self.source(raw);
        (!path.starts_with(&self.out) && !path.starts_with(&self.overlay)).then_some(path)
    }

    /// A path to show to users: relative to the main directory if inside.
    pub fn display(&self, raw: &str) -> String {
        let path = self.source(raw);
        path.strip_prefix(&self.main_dir).map(|p| p.display().to_string()).unwrap_or_else(|_| path.display().to_string())
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub fn compile(job: Job) -> Outcome {
    let start = Instant::now();
    let mut outcome = Outcome {
        id: job.id,
        main: job.main.clone(),
        doc: None,
        pdf_path: None,
        synctex: None,
        deps: HashSet::new(),
        errors: Vec::new(),
        warnings: 0,
        ms: 0,
    };
    let result = run(&job, &mut outcome);
    if let Err(e) = result {
        outcome.errors.insert(0, e);
    }
    outcome.ms = start.elapsed().as_millis() as u64;
    outcome
}

fn run(job: &Job, outcome: &mut Outcome) -> Result<(), String> {
    let main = &job.main;
    let main_dir = main.parent().ok_or("the main file has no directory")?.to_path_buf();
    let file_name = main.file_name().ok_or("no file name")?.to_string_lossy().into_owned();
    let stem = main.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let (overlay, out) = work_dirs(main).map_err(|e| format!("cannot create the build directory: {e}"))?;
    let overlay_changed =
        sync_overlay(&overlay, &main_dir, &job.buffers).map_err(|e| format!("cannot write unsaved files: {e}"))?;
    let map = PathMap { main_dir: main_dir.clone(), overlay: overlay.clone(), out: out.clone() };

    let main_text = job
        .buffers
        .iter()
        .find(|(p, _)| p == main)
        .map(|(_, t)| t.to_string())
        .or_else(|| std::fs::read_to_string(main).ok())
        .unwrap_or_default();

    let (program, args): (String, Vec<String>) = match &job.cfg.command {
        Some(cmd) if !cmd.is_empty() => {
            let subst = |a: &String| a.replace("%MAIN%", &file_name).replace("%OUTDIR%", &out.to_string_lossy());
            (subst(&cmd[0]), cmd[1..].iter().map(subst).collect())
        }
        _ => {
            let engine = match job.cfg.engine.as_str() {
                "" | "auto" => detect_engine(&main_text, &main_dir),
                other => other.to_string(),
            };
            let mut args = engine_args(&engine);
            args.extend(
                ["-synctex=1", "-interaction=nonstopmode", "-file-line-error", "-halt-on-error", "-recorder"]
                    .map(String::from),
            );
            if overlay_changed {
                // TeX reads other files now: run even if latexmk thinks
                // that nothing changed.
                args.push("-g".into());
            }
            args.push(format!("-outdir={}", out.display()));
            args.extend(job.cfg.latexmk_args.iter().cloned());
            args.push(file_name.clone());
            log::debug!("engine {engine}");
            ("latexmk".to_string(), args)
        }
    };

    let pdf_path = out.join(format!("{stem}.pdf"));
    let before = mtime(&pdf_path);
    log::debug!("running {program} {args:?} in {}", main_dir.display());
    let output = Command::new(&program)
        .args(&args)
        .current_dir(&main_dir)
        .env("TEXINPUTS", search_path(&overlay, "TEXINPUTS"))
        .env("BIBINPUTS", search_path(&overlay, "BIBINPUTS"))
        // Long log lines, so that messages are not wrapped.
        .env("max_print_line", "10000")
        .env("error_line", "254")
        .env("half_error_line", "238")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;

    // Which files the document read: switching to one of them keeps the
    // preview on this main file.
    let fls = std::fs::read_to_string(out.join(format!("{stem}.fls"))).unwrap_or_default();
    outcome.deps = fls
        .lines()
        .filter_map(|l| l.strip_prefix("INPUT "))
        .filter_map(|p| map.project_source(p))
        .collect();
    outcome.deps.insert(main.clone());

    let log_text = std::fs::read(out.join(format!("{stem}.log"))).map(|b| String::from_utf8_lossy(&b).into_owned());
    if let Ok(log_text) = &log_text {
        let (errors, warnings) = parse_log(log_text, &map);
        outcome.errors = errors;
        outcome.warnings = warnings;
    }
    if !output.status.success() && outcome.errors.is_empty() {
        // latexmk's own complaint (bibtex errors, missing programs, …).
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stderr
            .lines()
            .chain(stdout.lines())
            .rfind(|l| l.contains("rror") || l.contains("not found") || l.contains("failed"))
            .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
            .unwrap_or("compilation failed");
        outcome.errors.push(format!("{program}: {}", line.trim()));
    }

    let after = mtime(&pdf_path);
    if after.is_some() && (after != before || job.reload) {
        let bytes = std::fs::read(&pdf_path).map_err(|e| format!("cannot read {}: {e}", pdf_path.display()))?;
        let doc = PdfDoc::load(bytes).map_err(|e| e.to_string())?;
        outcome.doc = Some(Arc::new(doc));
        outcome.synctex = read_synctex(&out.join(format!("{stem}.synctex.gz")), &map);
    }
    if after.is_some() {
        outcome.pdf_path = Some(pdf_path);
    }
    Ok(())
}

fn read_synctex(path: &Path, map: &PathMap) -> Option<SyncTex> {
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(flate2::read::MultiGzDecoder::new(file));
    let st = SyncTex::parse(reader, |p| map.project_source(p));
    (!st.is_empty()).then_some(st)
}

/// `file:line: message` (with `-file-line-error`).
fn file_line_error(line: &str) -> Option<(&str, &str, &str)> {
    let mut search = 0;
    while let Some(i) = line[search..].find(':') {
        let i = search + i;
        let rest = &line[i + 1..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && rest[digits..].starts_with(": ") {
            let file = &line[..i];
            if file.contains('.') && !file.contains(' ') || file.starts_with('/') || file.starts_with("./") {
                return Some((file, &rest[..digits], &rest[digits + 2..]));
            }
        }
        search = i + 1;
    }
    None
}

/// Errors (first first, with their location) and the number of warnings.
pub fn parse_log(log: &str, map: &PathMap) -> (Vec<String>, usize) {
    let lines: Vec<&str> = log.lines().collect();
    let mut errors = Vec::new();
    let mut warnings = 0;
    for (i, line) in lines.iter().enumerate() {
        let message = if let Some((file, n, msg)) = file_line_error(line) {
            format!("{}:{n}: {msg}", map.display(file))
        } else if let Some(msg) = line.strip_prefix("! ") {
            if msg.starts_with("Emergency stop") || msg.starts_with("==> Fatal error") {
                continue;
            }
            msg.to_string()
        } else {
            if line.contains("Warning:") {
                warnings += 1;
            }
            continue;
        };
        // TeX shows where it stopped: `l.12 \textbx` (the rest of the line
        // follows on the next one).
        let context = lines[i + 1..]
            .iter()
            .take(8)
            .find_map(|l| l.strip_prefix("l.").and_then(|r| r.split_once(' ')).map(|(_, c)| c.trim()))
            .filter(|c| !c.is_empty());
        errors.push(match context {
            Some(c) => format!("{message} ({c})"),
            None => message,
        });
    }
    errors.dedup();
    (errors, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_comments() {
        let t = "% !TEX root = ../main.tex\n%!TeX program=lualatex\n\\section{a}";
        assert_eq!(magic_comment(t, "root").as_deref(), Some("../main.tex"));
        assert_eq!(magic_comment(t, "program").as_deref(), Some("lualatex"));
        assert_eq!(magic_comment("% !TEX TS-program = xelatex", "program").as_deref(), Some("xelatex"));
        assert_eq!(magic_comment("\\documentclass{article}", "root"), None);
    }

    #[test]
    fn classes_and_engines() {
        let e = |t: &str| choose_engine(t, false, false);
        assert_eq!(document_class("% \\documentclass{x}\n\\documentclass[a4paper,uplatex]{jsarticle}"), Some(("jsarticle".into(), "a4paper,uplatex".into())));
        assert_eq!(e("\\documentclass{article}"), "pdflatex");
        assert_eq!(e("\\documentclass{jsarticle}"), "uplatex");
        assert_eq!(e("\\documentclass{ltjsarticle}"), "lualatex");
        assert_eq!(e("\\documentclass[lualatex]{bxjsarticle}"), "lualatex");
        assert_eq!(e("\\documentclass{article}\n\\usepackage{fontspec}"), "lualatex");
        assert_eq!(e("\\documentclass{article}\n\\usepackage[a]{amsmath,xeCJK}"), "xelatex");
        assert_eq!(e("\\documentclass{article}\n% \\usepackage{fontspec}"), "pdflatex");
        assert_eq!(e("% !TEX program = XeLaTeX\n\\documentclass{jsarticle}"), "xelatex");
        // A latexmkrc next to the document wins over the class, the user's
        // own one does not.
        assert_eq!(choose_engine("\\documentclass{ltjsarticle}", true, false), "latexmkrc");
        assert_eq!(choose_engine("\\documentclass{ltjsarticle}", false, true), "lualatex");
        assert_eq!(choose_engine("\\documentclass{article}", false, true), "latexmkrc");
    }

    #[test]
    fn finds_includes() {
        let dir = Path::new("/p");
        let f = Path::new("/p/sec/intro.tex");
        assert!(includes("\\input{sec/intro}", dir, f));
        assert!(includes("\\include{./sec/intro.tex}", dir, f));
        assert!(includes("\\subimport{sec/}{intro}", dir, f));
        assert!(!includes("% \\input{sec/intro}", dir, f));
        assert!(!includes("\\input{sec/other}", dir, f));
    }

    #[test]
    fn log_errors() {
        let map = PathMap { main_dir: "/p".into(), overlay: "/o".into(), out: "/out".into() };
        let log = "(./main.tex\n/o/sec/a.tex:3: Undefined control sequence.\nl.3 \\textbx\n             {hi}\n! Emergency stop.\nLaTeX Warning: Reference `x' undefined.\n";
        let (errors, warnings) = parse_log(log, &map);
        assert_eq!(errors, vec!["sec/a.tex:3: Undefined control sequence. (\\textbx)".to_string()]);
        assert_eq!(warnings, 1);
        let (errors, _) = parse_log("! LaTeX Error: File `x.sty' not found.\n", &map);
        assert_eq!(errors, vec!["LaTeX Error: File `x.sty' not found.".to_string()]);
    }

    #[test]
    fn maps_overlay_paths() {
        let map = PathMap { main_dir: "/p".into(), overlay: "/o".into(), out: "/out".into() };
        assert_eq!(map.source("/o/sec/a.tex"), PathBuf::from("/p/sec/a.tex"));
        assert_eq!(map.project_source("/out/main.aux"), None);
        assert_eq!(map.project_source("/nix/store/x-texlive/tex/latex/base/article.cls"), None);
        assert_eq!(map.display("/o/./main.tex"), "main.tex");
    }
}
