//! Hosts the socket that preview servers connect to, and routes messages
//! between servers and preview panes.
//!
//! Each server says in its `hello` which kind of document it renders and for
//! which file extensions. A pane shows one kind and is bound to the server of
//! that kind whose workspace root contains the previewed file.

use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use gpui::{App, AppContext as _, Context, Entity, Global, WeakEntity};
use serde_json::{Value, json};

use crate::ExternalPreviewView;
use crate::protocol::{self, read_frame, write_frame};

pub(crate) type ConnId = u64;
pub(crate) type Writer = mpsc::Sender<Value>;

enum HubEvent {
    Connected(ConnId, mpsc::Sender<Value>),
    Frame(ConnId, Value, Vec<u8>),
    Disconnected(ConnId),
}

/// How a server lays out its pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    /// The server decides the page sizes (Typst, PDF); zooming scales them.
    Fixed,
    /// The server lays out the document to the pane width it is told about
    /// (`viewport` messages), like a web page.
    Reflow,
}

/// What a server renders, from its `hello`.
#[derive(Clone, Debug)]
pub(crate) struct ServerInfo {
    pub kind: String,
    pub name: String,
    /// Lowercase, without the dot.
    pub extensions: Vec<String>,
    pub layout: Layout,
}

impl ServerInfo {
    /// Protocol v2 servers (`typst-livepreview` before v3) do not say.
    fn typst() -> Self {
        Self {
            kind: "typst".into(),
            name: "Typst".into(),
            extensions: vec!["typ".into()],
            layout: Layout::Fixed,
        }
    }

    fn from_hello(header: &Value) -> Self {
        let Some(kind) = header.get("kind").and_then(Value::as_str) else {
            return Self::typst();
        };
        let extensions = header
            .get("extensions")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|e| e.trim_start_matches('.').to_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        let layout = match header.get("layout").and_then(Value::as_str) {
            Some("reflow") => Layout::Reflow,
            _ => Layout::Fixed,
        };
        Self {
            kind: kind.to_string(),
            name: header
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(kind)
                .to_string(),
            extensions,
            layout,
        }
    }

    pub fn handles(&self, path: &Path) -> bool {
        handles(&self.extensions, path)
    }
}

/// Whether `path` has one of `extensions` (lowercase, without the dot).
pub(crate) fn handles(extensions: &[String], path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| extensions.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

struct Conn {
    roots: Vec<PathBuf>,
    writer: mpsc::Sender<Value>,
    /// Known once the server said `hello`.
    info: Option<ServerInfo>,
}

#[derive(Default)]
pub(crate) struct Hub {
    conns: BTreeMap<ConnId, Conn>,
    views: Vec<WeakEntity<ExternalPreviewView>>,
}

struct GlobalHub(Entity<Hub>);
impl Global for GlobalHub {}

pub(crate) fn hub(cx: &App) -> Option<Entity<Hub>> {
    cx.try_global::<GlobalHub>().map(|g| g.0.clone())
}

pub(crate) fn init(cx: &mut App) {
    let hub = cx.new(|_| Hub::default());
    cx.set_global(GlobalHub(hub.clone()));

    let (tx, rx) = smol::channel::unbounded::<HubEvent>();
    if let Err(error) = std::thread::Builder::new()
        .name("external-preview-listener".into())
        .spawn(move || listen(tx))
    {
        log::error!("external preview: cannot start listener: {error}");
        return;
    }
    cx.spawn(async move |cx| {
        while let Ok(event) = rx.recv().await {
            cx.update(|cx| hub.update(cx, |hub, cx| hub.handle(event, cx)));
        }
    })
    .detach();
}

/// Removes sockets left behind by Zed processes that no longer run.
fn remove_stale_sockets(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let pid = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<i32>().ok());
        if let Some(pid) = pid {
            let alive = unsafe { libc::kill(pid, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
            if !alive {
                std::fs::remove_file(&path).ok();
            }
        }
    }
}

fn listen(tx: smol::channel::Sender<HubEvent>) {
    let dir = protocol::socket_dir();
    if let Err(error) = std::fs::create_dir_all(&dir) {
        log::error!("external preview: cannot create {dir:?}: {error}");
        return;
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok();
    remove_stale_sockets(&dir);
    let path = protocol::socket_path();
    std::fs::remove_file(&path).ok();
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            log::error!("external preview: cannot listen on {path:?}: {error}");
            return;
        }
    };
    log::info!("external preview: listening on {path:?}");

    let mut next_id: ConnId = 1;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let id = next_id;
        next_id += 1;

        let (writer_tx, writer_rx) = mpsc::channel::<Value>();
        let Ok(write_stream) = stream.try_clone() else { continue };
        std::thread::Builder::new()
            .name(format!("external-preview-writer-{id}"))
            .spawn(move || {
                let mut w = BufWriter::new(write_stream);
                for header in writer_rx {
                    if write_frame(&mut w, &header, &[]).is_err() {
                        return;
                    }
                }
            })
            .ok();

        let tx2 = tx.clone();
        std::thread::Builder::new()
            .name(format!("external-preview-reader-{id}"))
            .spawn(move || {
                let mut r = BufReader::with_capacity(1 << 20, stream);
                while let Ok((header, payload)) = read_frame(&mut r) {
                    if tx2.send_blocking(HubEvent::Frame(id, header, payload)).is_err() {
                        return;
                    }
                }
                tx2.send_blocking(HubEvent::Disconnected(id)).ok();
            })
            .ok();

        if tx.send_blocking(HubEvent::Connected(id, writer_tx)).is_err() {
            return;
        }
    }
}

impl Hub {
    fn handle(&mut self, event: HubEvent, cx: &mut Context<Self>) {
        match event {
            HubEvent::Connected(id, writer) => {
                log::info!("external preview: language server connected ({id})");
                // Panes are bound once the server said what it renders.
                self.conns.insert(
                    id,
                    Conn {
                        roots: Vec::new(),
                        writer,
                        info: None,
                    },
                );
            }
            HubEvent::Disconnected(id) => {
                log::info!("external preview: language server disconnected ({id})");
                self.conns.remove(&id);
                for view in self.live_views() {
                    view.update(cx, |view, cx| {
                        if view.conn() == Some(id) {
                            view.set_conn(None, cx);
                        }
                    });
                }
                self.bind_unbound_views(cx);
            }
            HubEvent::Frame(id, header, payload) => {
                let kind = header.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "hello" => {
                        let version = header.get("version").and_then(Value::as_u64);
                        if version.is_none_or(|v| v > protocol::PROTOCOL_VERSION) {
                            log::warn!(
                                "external preview: server speaks protocol {version:?}, expected {}",
                                protocol::PROTOCOL_VERSION
                            );
                        }
                        let info = ServerInfo::from_hello(&header);
                        log::info!(
                            "external preview: server {id} is {} ({}, {:?}, {:?}, protocol {version:?})",
                            info.name,
                            info.kind,
                            info.extensions,
                            info.layout
                        );
                        if let Some(conn) = self.conns.get_mut(&id) {
                            conn.info = Some(info);
                        }
                        self.bind_unbound_views(cx);
                    }
                    "roots" => {
                        if let Some(conn) = self.conns.get_mut(&id) {
                            conn.roots = header
                                .get("roots")
                                .and_then(Value::as_array)
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|v| v.as_str().map(PathBuf::from))
                                        .collect()
                                })
                                .unwrap_or_default();
                        }
                        self.bind_unbound_views(cx);
                    }
                    "open_preview" => {
                        if let Some(path) = header.get("path").and_then(Value::as_str) {
                            let path = PathBuf::from(path);
                            // `split`: next to the editor, else in its pane.
                            // `explicit`: the user asked (toolbar button), so
                            // reveal the preview even if it is open.
                            let split = header.get("split").and_then(Value::as_bool).unwrap_or(true);
                            let explicit = header.get("explicit").and_then(Value::as_bool).unwrap_or(false);
                            let has_view = self.live_views().iter().any(|v| v.read(cx).conn() == Some(id));
                            let info = self.conns.get(&id).and_then(|c| c.info.clone());
                            if let Some(info) = info
                                && (explicit || !has_view)
                            {
                                cx.defer(move |cx| ExternalPreviewView::open_for_path(path, info, split, cx));
                            }
                        }
                    }
                    _ => {
                        // Everything else is for the panes of this server.
                        let mut payload = Some(payload);
                        let views: Vec<_> = self
                            .live_views()
                            .into_iter()
                            .filter(|v| v.read(cx).conn() == Some(id))
                            .collect();
                        let count = views.len();
                        for (i, view) in views.into_iter().enumerate() {
                            // Avoid copying bitmaps for the common single-pane case.
                            let data = if i + 1 == count {
                                payload.take().unwrap_or_default()
                            } else {
                                payload.clone().unwrap_or_default()
                            };
                            let header = header.clone();
                            view.update(cx, |view, cx| view.handle_message(header, data, cx));
                        }
                    }
                }
            }
        }
    }

    fn live_views(&mut self) -> Vec<Entity<ExternalPreviewView>> {
        self.views.retain(|v| v.upgrade().is_some());
        self.views.iter().filter_map(|v| v.upgrade()).collect()
    }

    /// Picks the server among `candidates` responsible for `path`: the one
    /// whose workspace root contains it (longest match), otherwise the most
    /// recent one.
    fn pick_among<'a>(
        candidates: impl Iterator<Item = (&'a ConnId, &'a Conn)> + Clone,
        path: &Path,
    ) -> Option<ConnId> {
        candidates
            .clone()
            .filter_map(|(id, c)| {
                c.roots
                    .iter()
                    .filter(|r| path.starts_with(r))
                    .map(|r| r.as_os_str().len())
                    .max()
                    .map(|len| (len, *id))
            })
            .max()
            .map(|(_, id)| id)
            .or_else(|| candidates.map(|(id, _)| *id).max())
    }

    /// The server of `kind` responsible for `path`.
    fn pick(&self, kind: &str, path: &Path) -> Option<ConnId> {
        let candidates = self
            .conns
            .iter()
            .filter(|(_, c)| c.info.as_ref().is_some_and(|i| i.kind == kind));
        Self::pick_among(candidates, path)
    }

    /// What would preview `path`: the server that handles its extension (if
    /// several do, the one whose workspace root contains it).
    pub(crate) fn info_for_path(&self, path: &Path) -> Option<ServerInfo> {
        let candidates = self
            .conns
            .iter()
            .filter(|(_, c)| c.info.as_ref().is_some_and(|i| i.handles(path)));
        let id = Self::pick_among(candidates, path)?;
        self.conns.get(&id)?.info.clone()
    }

    fn bind_unbound_views(&mut self, cx: &mut Context<Self>) {
        for view in self.live_views() {
            let (conn, kind, path) = {
                let v = view.read(cx);
                (v.conn(), v.kind().to_string(), v.source_path())
            };
            if conn.is_none()
                && let Some(id) = self.pick(&kind, &path)
                && let Some(c) = self.conns.get(&id)
                && let Some(info) = c.info.clone()
            {
                // Panes get their own sender: they must not call back into the
                // hub while it is being updated.
                let writer = c.writer.clone();
                view.update(cx, |view, cx| view.set_conn(Some((id, writer, info)), cx));
            }
        }
    }

    pub(crate) fn add_view(&mut self, view: WeakEntity<ExternalPreviewView>, cx: &mut Context<Self>) {
        self.views.push(view);
        self.bind_unbound_views(cx);
    }

    /// Called when a pane goes away: tells its server if no pane is left.
    pub(crate) fn view_released(&mut self, conn: Option<ConnId>, cx: &mut Context<Self>) {
        let Some(id) = conn else { return };
        let remaining = self
            .live_views()
            .iter()
            .filter(|v| v.read(cx).conn() == Some(id))
            .count();
        if remaining == 0 {
            self.send(id, json!({ "type": "detach" }));
        }
    }

    pub(crate) fn send(&self, conn: ConnId, header: Value) {
        if let Some(c) = self.conns.get(&conn) {
            c.writer.send(header).ok();
        }
    }
}
