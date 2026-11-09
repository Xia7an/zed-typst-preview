//! Hosts the socket that `typst-livepreview` servers connect to, and routes
//! messages between servers and preview panes.

use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use gpui::{App, AppContext as _, Context, Entity, Global, WeakEntity};
use serde_json::{Value, json};

use crate::TypstPreviewView;
use crate::protocol::{self, read_frame, write_frame};

pub(crate) type ConnId = u64;
pub(crate) type Writer = mpsc::Sender<Value>;

enum HubEvent {
    Connected(ConnId, mpsc::Sender<Value>),
    Frame(ConnId, Value, Vec<u8>),
    Disconnected(ConnId),
}

struct Conn {
    roots: Vec<PathBuf>,
    writer: mpsc::Sender<Value>,
}

#[derive(Default)]
pub(crate) struct Hub {
    conns: BTreeMap<ConnId, Conn>,
    views: Vec<WeakEntity<TypstPreviewView>>,
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
        .name("typst-preview-listener".into())
        .spawn(move || listen(tx))
    {
        log::error!("typst preview: cannot start listener: {error}");
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
        log::error!("typst preview: cannot create {dir:?}: {error}");
        return;
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok();
    remove_stale_sockets(&dir);
    let path = protocol::socket_path();
    std::fs::remove_file(&path).ok();
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            log::error!("typst preview: cannot listen on {path:?}: {error}");
            return;
        }
    };
    log::info!("typst preview: listening on {path:?}");

    let mut next_id: ConnId = 1;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let id = next_id;
        next_id += 1;

        let (writer_tx, writer_rx) = mpsc::channel::<Value>();
        let Ok(write_stream) = stream.try_clone() else { continue };
        std::thread::Builder::new()
            .name(format!("typst-preview-writer-{id}"))
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
            .name(format!("typst-preview-reader-{id}"))
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
                log::info!("typst preview: language server connected ({id})");
                self.conns.insert(
                    id,
                    Conn {
                        roots: Vec::new(),
                        writer,
                    },
                );
                self.bind_unbound_views(cx);
            }
            HubEvent::Disconnected(id) => {
                log::info!("typst preview: language server disconnected ({id})");
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
                        if version != Some(protocol::PROTOCOL_VERSION) {
                            log::warn!(
                                "typst preview: server speaks protocol {version:?}, expected {}",
                                protocol::PROTOCOL_VERSION
                            );
                        }
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
                            if explicit || !has_view {
                                cx.defer(move |cx| TypstPreviewView::open_for_path(path, split, cx));
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

    fn live_views(&mut self) -> Vec<Entity<TypstPreviewView>> {
        self.views.retain(|v| v.upgrade().is_some());
        self.views.iter().filter_map(|v| v.upgrade()).collect()
    }

    /// Picks the server responsible for `path`: the one whose workspace root
    /// contains it (longest match), otherwise the most recent one.
    fn pick(&self, path: &Path) -> Option<ConnId> {
        self.conns
            .iter()
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
            .or_else(|| self.conns.keys().next_back().copied())
    }

    fn bind_unbound_views(&mut self, cx: &mut Context<Self>) {
        for view in self.live_views() {
            let (conn, path) = {
                let v = view.read(cx);
                (v.conn(), v.source_path())
            };
            if conn.is_none()
                && let Some(id) = self.pick(&path)
                && let Some(c) = self.conns.get(&id)
            {
                // Panes get their own sender: they must not call back into the
                // hub while it is being updated.
                let writer = c.writer.clone();
                view.update(cx, |view, cx| view.set_conn(Some((id, writer)), cx));
            }
        }
    }

    pub(crate) fn add_view(&mut self, view: WeakEntity<TypstPreviewView>, cx: &mut Context<Self>) {
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
