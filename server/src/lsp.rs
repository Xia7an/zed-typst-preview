//! The language server side: receives buffer contents and cursor positions
//! from the editor and asks it to reveal source locations.
//!
//! This server does not publish diagnostics or completions: it is meant to
//! run next to tinymist, which handles those.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use crossbeam_channel::Sender;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use lsp_types::{
    CodeActionKind, CodeActionOrCommand, CodeActionProviderCapability, Command, ExecuteCommandOptions,
    InitializeParams, Position, Range, ServerCapabilities, ServerInfo, ShowDocumentParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::config::Config;
use crate::core::{self, CoreMsg, Editor, Shared, TextChange};
use crate::viewer::{UiEvent, UiSender};

pub const CMD_OPEN: &str = "typst-livepreview.open";

pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let path = url::Url::parse(uri.as_str()).ok()?.to_file_path().ok()?;
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

fn path_to_uri(path: &Path) -> Option<Uri> {
    Uri::from_str(url::Url::from_file_path(path).ok()?.as_str()).ok()
}

/// Sends `window/showDocument` requests (or, inside Zed's preview pane, a
/// direct jump); falls back to an editor CLI.
struct LspEditor {
    embedded: Option<UiSender>,
    sender: Sender<Message>,
    next_id: AtomicI32,
    pending: Arc<Mutex<HashMap<RequestId, (PathBuf, u32, u32)>>>,
}

impl Editor for LspEditor {
    fn show_document(&self, path: &Path, line: u32, character: u32) {
        if let Some(ui) = &self.embedded {
            ui.send_event(UiEvent::Jump { path: path.to_path_buf(), line, character });
            return;
        }
        let Some(uri) = path_to_uri(path) else { return };
        let pos = Position { line, character };
        let params = ShowDocumentParams {
            uri,
            external: Some(false),
            take_focus: Some(true),
            selection: Some(Range { start: pos, end: pos }),
        };
        let id = RequestId::from(format!("tlp-{}", self.next_id.fetch_add(1, Ordering::Relaxed)));
        self.pending.lock().insert(id.clone(), (path.to_path_buf(), line, character));
        let req = Request::new(id, lsp_types::request::ShowDocument::METHOD.into(), params);
        let _ = self.sender.send(Message::Request(req));
    }
}

/// Opens `path:line:col` with an editor command line tool.
fn open_with_cli(cfg: &Config, path: &Path, line: u32, character: u32) {
    let target = format!("{}:{}:{}", path.display(), line + 1, character + 1);
    let candidates: Vec<String> = match &cfg.editor_command {
        Some(c) => vec![c.clone()],
        None => vec!["zed".into(), "zeditor".into(), "zed-editor".into()],
    };
    for cmd in candidates {
        if std::process::Command::new(&cmd).arg(&target).spawn().is_ok() {
            return;
        }
    }
    log::warn!("could not open {target}: no editor command found");
}

/// Runs the language server until the editor shuts it down. With
/// `embedded`, the preview is shown in Zed's built-in pane.
pub fn run(proxy: UiSender, embedded: bool) -> anyhow::Result<()> {
    let (connection, io_threads) = Connection::stdio();

    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::INCREMENTAL)),
        // Zed asks every server for code actions whenever the cursor moves:
        // this is how the preview follows the cursor.
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![CMD_OPEN.into()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let (init_id, init_params) = connection.initialize_start()?;
    let init: InitializeParams = serde_json::from_value(init_params)?;
    connection.initialize_finish(
        init_id,
        json!({
            "capabilities": capabilities,
            "serverInfo": ServerInfo { name: "typst-livepreview".into(), version: Some(env!("CARGO_PKG_VERSION").into()) },
        }),
    )?;

    let mut cfg = init.initialization_options.as_ref().and_then(Config::from_json).unwrap_or_default();
    #[allow(deprecated)]
    let mut roots: Vec<PathBuf> = init
        .workspace_folders
        .iter()
        .flatten()
        .filter_map(|f| uri_to_path(&f.uri))
        .chain(init.root_uri.as_ref().and_then(uri_to_path))
        .collect();
    roots.dedup();
    log::info!("initialized; workspace roots: {roots:?}");

    let pending = Arc::new(Mutex::new(HashMap::new()));
    let _ = proxy.send_event(UiEvent::Roots(roots.clone()));
    let editor = LspEditor {
        embedded: embedded.then(|| proxy.clone()),
        sender: connection.sender.clone(),
        next_id: AtomicI32::new(1),
        pending: pending.clone(),
    };
    let shared = Arc::new(Shared::default());
    let (core_tx, core_thread) = core::spawn(cfg.clone(), roots, Box::new(editor), proxy.clone(), shared.clone());
    let _ = proxy.send_event(UiEvent::Config(cfg.clone()));
    let _ = proxy.send_event(UiEvent::CoreReady(core_tx.clone()));

    register_file_watcher(&connection);

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    break;
                }
                let resp = handle_request(req, &core_tx, &shared);
                connection.sender.send(Message::Response(resp))?;
            }
            Message::Notification(not) => {
                if not.method == lsp_types::notification::Exit::METHOD {
                    break;
                }
                if let Some(new_cfg) = handle_notification(not, &core_tx) {
                    cfg = new_cfg.clone();
                    let _ = core_tx.send(CoreMsg::Config(new_cfg));
                }
            }
            Message::Response(resp) => {
                let Some((path, line, col)) = pending.lock().remove(&resp.id) else { continue };
                let ok = resp.error.is_none()
                    && resp.result.as_ref().and_then(|r| r.get("success")).and_then(Value::as_bool).unwrap_or(false);
                if !ok {
                    log::info!("window/showDocument failed ({:?}); using the editor CLI", resp.error);
                    open_with_cli(&cfg, &path, line, col);
                }
            }
        }
    }

    log::info!("shutting down");
    let _ = core_tx.send(CoreMsg::Shutdown);
    let _ = proxy.send_event(UiEvent::Exit);
    drop(connection);
    let _ = core_thread.join();
    let _ = io_threads.join();
    Ok(())
}

/// Asks the client to report file changes on disk (images, bibliographies,
/// files not open in the editor).
fn register_file_watcher(connection: &Connection) {
    let params = json!({
        "registrations": [{
            "id": "typst-livepreview-watch",
            "method": lsp_types::notification::DidChangeWatchedFiles::METHOD,
            "registerOptions": { "watchers": [{ "globPattern": "**/*" }] }
        }]
    });
    let req = Request::new(
        RequestId::from("tlp-register".to_string()),
        lsp_types::request::RegisterCapability::METHOD.into(),
        params,
    );
    let _ = connection.sender.send(Message::Request(req));
}

fn handle_request(req: Request, core: &Sender<CoreMsg>, shared: &Shared) -> Response {
    match req.method.as_str() {
        lsp_types::request::CodeActionRequest::METHOD => {
            let Ok(params) = serde_json::from_value::<lsp_types::CodeActionParams>(req.params) else {
                return Response::new_ok(req.id, Value::Null);
            };
            let Some(path) = uri_to_path(&params.text_document.uri) else {
                return Response::new_ok(req.id, Value::Null);
            };
            let start = params.range.start;
            let _ = core.send(CoreMsg::Cursor { path, line: start.line, character: start.character });

            // Offer to (re)open the preview only while it is closed, to keep
            // the code action indicator quiet otherwise.
            let mut actions: Vec<CodeActionOrCommand> = Vec::new();
            let only_other_kinds = params
                .context
                .only
                .as_ref()
                .is_some_and(|only| !only.iter().any(|k| k.as_str().is_empty() || *k == CodeActionKind::EMPTY));
            if !shared.window_open.load(Ordering::Relaxed) && !only_other_kinds {
                actions.push(CodeActionOrCommand::CodeAction(lsp_types::CodeAction {
                    title: "Open Typst Live Preview".into(),
                    kind: Some(CodeActionKind::EMPTY),
                    command: Some(Command {
                        title: "Open Typst Live Preview".into(),
                        command: CMD_OPEN.into(),
                        arguments: Some(vec![json!(params.text_document.uri.as_str())]),
                    }),
                    ..Default::default()
                }));
            }
            Response::new_ok(req.id, actions)
        }
        lsp_types::request::ExecuteCommand::METHOD => {
            let params: lsp_types::ExecuteCommandParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return Response::new_err(req.id, -32602, e.to_string()),
            };
            if params.command == CMD_OPEN {
                let path = params
                    .arguments
                    .first()
                    .and_then(Value::as_str)
                    .and_then(|s| Uri::from_str(s).ok())
                    .and_then(|u| uri_to_path(&u));
                let _ = core.send(CoreMsg::OpenPreview { path });
                Response::new_ok(req.id, Value::Null)
            } else {
                Response::new_err(req.id, -32601, format!("unknown command {}", params.command))
            }
        }
        _ => Response::new_err(req.id, -32601, format!("method not supported: {}", req.method)),
    }
}

/// Handles a notification. Returns a new configuration if it changed.
fn handle_notification(not: Notification, core: &Sender<CoreMsg>) -> Option<Config> {
    use lsp_types::notification as n;
    match not.method.as_str() {
        n::DidOpenTextDocument::METHOD => {
            let p: lsp_types::DidOpenTextDocumentParams = serde_json::from_value(not.params).ok()?;
            let path = uri_to_path(&p.text_document.uri)?;
            let _ = core.send(CoreMsg::Open { path, text: p.text_document.text });
        }
        n::DidChangeTextDocument::METHOD => {
            let p: lsp_types::DidChangeTextDocumentParams = serde_json::from_value(not.params).ok()?;
            let path = uri_to_path(&p.text_document.uri)?;
            let changes = p
                .content_changes
                .into_iter()
                .map(|c| TextChange {
                    range: c.range.map(|r| (r.start.line, r.start.character, r.end.line, r.end.character)),
                    text: c.text,
                })
                .collect();
            let _ = core.send(CoreMsg::Change { path, changes });
        }
        n::DidCloseTextDocument::METHOD => {
            let p: lsp_types::DidCloseTextDocumentParams = serde_json::from_value(not.params).ok()?;
            let path = uri_to_path(&p.text_document.uri)?;
            let _ = core.send(CoreMsg::Close { path });
        }
        n::DidChangeWatchedFiles::METHOD => {
            let _ = core.send(CoreMsg::FilesChanged);
        }
        n::DidChangeWorkspaceFolders::METHOD => {
            let p: lsp_types::DidChangeWorkspaceFoldersParams = serde_json::from_value(not.params).ok()?;
            let roots = p.event.added.iter().filter_map(|f| uri_to_path(&f.uri)).collect();
            let _ = core.send(CoreMsg::WorkspaceRoots(roots));
        }
        n::DidChangeConfiguration::METHOD => {
            let p: lsp_types::DidChangeConfigurationParams = serde_json::from_value(not.params).ok()?;
            return Config::from_json(&p.settings);
        }
        _ => {}
    }
    None
}
