//! The Typst [`World`]: unsaved editor buffers (overlays) on top of the file
//! system, with incremental re-parsing of edited sources.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use parking_lot::Mutex;
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::package::PackageSpec;
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_ide::IdeWorld;
use typst_kit::datetime::Time;
use typst_kit::fonts::FontStore;
use typst_kit::packages::SystemPackages;

/// Cached state of a file on disk.
#[derive(Default)]
struct Slot {
    /// Revision in which the file was last checked for changes.
    checked: u64,
    /// Modification time + length when the data was loaded.
    stamp: Option<(SystemTime, u64)>,
    bytes: Option<FileResult<Bytes>>,
    source: Option<FileResult<Source>>,
    /// The previous source, reused for incremental re-parsing.
    stale: Option<Source>,
}

pub struct PreviewWorld {
    root: PathBuf,
    main: FileId,
    library: LazyHash<Library>,
    fonts: Arc<FontStore>,
    packages: Arc<SystemPackages>,
    package_roots: Mutex<HashMap<PackageSpec, PathBuf>>,
    /// Unsaved buffers from the editor, keyed by absolute path.
    overlays: HashMap<PathBuf, Source>,
    slots: Mutex<HashMap<FileId, Slot>>,
    /// Project files accessed during the current compilation.
    accessed: Mutex<HashSet<PathBuf>>,
    revision: u64,
    time: Time,
}

impl PreviewWorld {
    pub fn new(fonts: Arc<FontStore>, packages: Arc<SystemPackages>) -> Self {
        let root = PathBuf::from("/");
        let main = Self::id_for(&root, Path::new("/main.typ"))
            .unwrap_or_else(|| RootedPath::new(VirtualRoot::Project, VirtualPath::new("main.typ").unwrap()).intern());
        Self {
            root,
            main,
            library: LazyHash::new(Library::default()),
            fonts,
            packages,
            package_roots: Mutex::new(HashMap::new()),
            overlays: HashMap::new(),
            slots: Mutex::new(HashMap::new()),
            accessed: Mutex::new(HashSet::new()),
            revision: 1,
            time: Time::system(),
        }
    }

    fn id_for(root: &Path, path: &Path) -> Option<FileId> {
        let vpath = VirtualPath::virtualize(root, path).ok()?;
        Some(RootedPath::new(VirtualRoot::Project, vpath).intern())
    }

    /// Sets the project root and main file. Returns `false` if the main file
    /// is not inside the root.
    pub fn set_target(&mut self, root: &Path, main: &Path) -> bool {
        let Some(main_id) = Self::id_for(root, main) else { return false };
        if root != self.root {
            self.root = root.to_path_buf();
            // Re-create overlays with ids in the new root.
            let old = std::mem::take(&mut self.overlays);
            for (path, src) in old {
                let source = self.make_source(&path, src.text().to_string());
                self.overlays.insert(path, source);
            }
            self.slots.lock().clear();
        }
        self.main = main_id;
        true
    }

    fn make_source(&self, path: &Path, text: String) -> Source {
        match Self::id_for(&self.root, path) {
            Some(id) => Source::new(id, text),
            None => Source::detached(text),
        }
    }

    /// Sets the full text of an editor buffer.
    pub fn set_overlay(&mut self, path: &Path, text: String) {
        match self.overlays.get_mut(path) {
            Some(src) => {
                src.replace(&text);
            }
            None => {
                let src = self.make_source(path, text);
                self.overlays.insert(path.to_path_buf(), src);
            }
        }
    }

    pub fn overlay_mut(&mut self, path: &Path) -> Option<&mut Source> {
        self.overlays.get_mut(path)
    }

    pub fn remove_overlay(&mut self, path: &Path) {
        if let Some(src) = self.overlays.remove(path) {
            // Keep the parsed tree around so re-loading from disk is incremental.
            let mut slots = self.slots.lock();
            if let Some(slot) = slots.get_mut(&src.id()) {
                slot.stale = Some(src);
                slot.source = None;
                slot.bytes = None;
                slot.stamp = None;
            }
        }
    }

    /// Returns the source for a path, preferring the editor buffer.
    pub fn source_for_path(&self, path: &Path) -> Option<Source> {
        if let Some(src) = self.overlays.get(path) {
            return Some(src.clone());
        }
        let id = Self::id_for(&self.root, path)?;
        self.source(id).ok()
    }

    /// Starts a new compilation: files on disk will be re-checked once.
    pub fn begin_revision(&mut self) {
        self.revision += 1;
        self.accessed.get_mut().clear();
        self.time.reset();
    }

    /// Project files read by the last compilation.
    pub fn dependencies(&self) -> HashSet<PathBuf> {
        self.accessed.lock().clone()
    }

    /// Resolves a file id to a real path.
    pub fn path_of(&self, id: FileId) -> Option<PathBuf> {
        let root = self.root_of(id).ok()?;
        id.vpath().realize(&root).ok()
    }

    fn root_of(&self, id: FileId) -> FileResult<PathBuf> {
        match id.root() {
            VirtualRoot::Project => Ok(self.root.clone()),
            VirtualRoot::Package(spec) => {
                if let Some(root) = self.package_roots.lock().get(spec) {
                    return Ok(root.clone());
                }
                let root = self.packages.obtain(spec)?.path().to_path_buf();
                self.package_roots.lock().insert(spec.clone(), root.clone());
                Ok(root)
            }
        }
    }

    /// Looks up (and refreshes) the disk slot for a file.
    fn with_slot<T>(&self, id: FileId, f: impl FnOnce(&mut Slot, &Path) -> FileResult<T>) -> FileResult<T> {
        let is_project = matches!(id.root(), VirtualRoot::Project);
        let path = id.vpath().realize(&self.root_of(id)?)?;
        if is_project {
            self.accessed.lock().insert(path.clone());
        }
        let mut slots = self.slots.lock();
        let slot = slots.entry(id).or_default();
        if slot.checked != self.revision {
            slot.checked = self.revision;
            // Package files never change; project files are re-checked once
            // per compilation through a cheap `stat`.
            if is_project || slot.stamp.is_none() {
                let stamp = fs::metadata(&path).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
                if stamp != slot.stamp || stamp.is_none() {
                    slot.stamp = stamp;
                    slot.bytes = None;
                    if let Some(Ok(src)) = slot.source.take() {
                        slot.stale = Some(src);
                    }
                }
            }
        }
        f(slot, &path)
    }
}

fn read(path: &Path) -> FileResult<Bytes> {
    if path.is_dir() {
        return Err(FileError::IsDirectory);
    }
    fs::read(path).map(Bytes::new).map_err(|err| FileError::from_io(err, path))
}

impl World for PreviewWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        self.fonts.book()
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if matches!(id.root(), VirtualRoot::Project) {
            if let Ok(path) = id.vpath().realize(&self.root) {
                if let Some(src) = self.overlays.get(&path) {
                    self.accessed.lock().insert(path);
                    return Ok(src.clone());
                }
            }
        }
        self.with_slot(id, |slot, path| {
            if let Some(src) = &slot.source {
                return src.clone();
            }
            let bytes = match &slot.bytes {
                Some(b) => b.clone()?,
                None => {
                    let b = read(path);
                    slot.bytes = Some(b.clone());
                    b?
                }
            };
            let text = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
            let result = std::str::from_utf8(text).map_err(FileError::from).map(|text| match slot.stale.take() {
                Some(mut src) if src.id() == id => {
                    src.replace(text);
                    src
                }
                _ => Source::new(id, text.to_string()),
            });
            slot.source = Some(result.clone());
            result
        })
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if matches!(id.root(), VirtualRoot::Project) {
            if let Ok(path) = id.vpath().realize(&self.root) {
                if let Some(src) = self.overlays.get(&path) {
                    self.accessed.lock().insert(path);
                    return Ok(Bytes::from_string(src.clone()));
                }
            }
        }
        self.with_slot(id, |slot, path| {
            if let Some(b) = &slot.bytes {
                return b.clone();
            }
            let b = read(path);
            slot.bytes = Some(b.clone());
            b
        })
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.font(index)
    }

    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        self.time.today(offset)
    }
}

impl IdeWorld for PreviewWorld {
    fn upcast(&self) -> &dyn World {
        self
    }
}

/// Loads fonts: embedded Typst fonts, system fonts and extra directories.
pub fn load_fonts(extra: &[PathBuf], system: bool) -> FontStore {
    let mut store = FontStore::new();
    for dir in extra {
        store.extend(typst_kit::fonts::scan(dir));
    }
    if system {
        store.extend(typst_kit::fonts::system());
    }
    store.extend(typst_kit::fonts::embedded());
    store
}

pub fn packages() -> SystemPackages {
    let downloader = typst_kit::downloader::SystemDownloader::new(concat!("typst-livepreview/", env!("CARGO_PKG_VERSION")));
    SystemPackages::new(downloader)
}
