//! The language server side: receives buffer contents from the editor and
//! the commands of the toolbar buttons.
//!
//! This server publishes no diagnostics or completions: it runs next to
//! texlab (from the LaTeX extension), which handles those.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crossbeam_channel::Sender;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use lsp_types::{
    CodeActionKind, CodeActionOrCommand, CodeActionProviderCapability, Command, ExecuteCommandOptions,
    InitializeParams, MessageType, ServerCapabilities, ServerInfo, ShowMessageParams, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri,
};
use serde_json::{Value, json};

use crate::config::Config;
use crate::core::{CoreMsg, Shared};

pub const CMD_OPEN: &str = "tex-livepreview.open";
pub const CMD_EXPORT_PDF: &str = "tex-livepreview.exportPdf";

pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let path = url::Url::parse(uri.as_str()).ok()?.to_file_path().ok()?;
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

fn show_message(sender: &Sender<Message>, typ: MessageType, message: String) {
    let params = ShowMessageParams { typ, message };
    let not = Notification::new(lsp_types::notification::ShowMessage::METHOD.into(), params);
    let _ = sender.send(Message::Notification(not));
}

const NO_PANE: &str = "TeX Live Preview needs Zed with the preview pane patch (zed-typst-livepreview/zed-patch).";

/// Runs the language server until the editor shuts it down. `embedded`:
/// connected to Zed's preview pane.
pub fn run(connection: Connection, core: Sender<CoreMsg>, shared: Arc<Shared>, embedded: bool) -> anyhow::Result<()> {
    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![CMD_OPEN.into(), CMD_EXPORT_PDF.into()],
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
            "serverInfo": ServerInfo { name: "tex-livepreview".into(), version: Some(env!("CARGO_PKG_VERSION").into()) },
        }),
    )?;

    if let Some(cfg) = init.initialization_options.as_ref().and_then(Config::from_json) {
        let _ = core.send(CoreMsg::Config(cfg));
    }
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
    let _ = core.send(CoreMsg::Roots(roots));
    register_file_watcher(&connection);

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    break;
                }
                if let Some(resp) = handle_request(req, &core, &shared, &connection.sender, embedded) {
                    connection.sender.send(Message::Response(resp))?;
                }
            }
            Message::Notification(not) => {
                if not.method == lsp_types::notification::Exit::METHOD {
                    break;
                }
                handle_notification(not, &core);
            }
            Message::Response(_) => {}
        }
    }
    log::info!("shutting down");
    Ok(())
}

/// Asks the client to report file changes on disk (figures, bibliographies,
/// files not open in the editor).
fn register_file_watcher(connection: &Connection) {
    let params = json!({
        "registrations": [{
            "id": "tex-livepreview-watch",
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

/// `[uri, {"modifiers": {"alt": bool, …}}]`; the second argument comes from
/// the toolbar buttons of the patched Zed.
fn command_args(args: &[Value]) -> (Option<PathBuf>, bool) {
    let path = args.first().and_then(Value::as_str).and_then(|s| Uri::from_str(s).ok()).and_then(|u| uri_to_path(&u));
    let alt = args.get(1).and_then(|c| c.pointer("/modifiers/alt")).and_then(Value::as_bool).unwrap_or(false);
    (path, alt)
}

fn code_action(title: &str, command: &str, uri: &Uri) -> CodeActionOrCommand {
    CodeActionOrCommand::CodeAction(lsp_types::CodeAction {
        title: title.into(),
        kind: Some(CodeActionKind::EMPTY),
        command: Some(Command { title: title.into(), command: command.into(), arguments: Some(vec![json!(uri.as_str())]) }),
        ..Default::default()
    })
}

/// Handles a request. Returns `None` when the response is sent later (by
/// another thread, through `sender`).
fn handle_request(
    req: Request,
    core: &Sender<CoreMsg>,
    shared: &Shared,
    sender: &Sender<Message>,
    embedded: bool,
) -> Option<Response> {
    Some(match req.method.as_str() {
        lsp_types::request::CodeActionRequest::METHOD => {
            let Ok(params) = serde_json::from_value::<lsp_types::CodeActionParams>(req.params) else {
                return Some(Response::new_ok(req.id, Value::Null));
            };
            let only_other_kinds = params
                .context
                .only
                .as_ref()
                .is_some_and(|only| !only.iter().any(|k| k.as_str().is_empty() || *k == CodeActionKind::EMPTY));
            let mut actions = Vec::new();
            let uri = &params.text_document.uri;
            // Only while the preview is closed: the code action indicator
            // would be on all the time otherwise.
            if embedded && !only_other_kinds && !shared.attached.load(Ordering::Relaxed) {
                actions.push(code_action("Open LaTeX Live Preview", CMD_OPEN, uri));
            }
            Response::new_ok(req.id, actions)
        }
        lsp_types::request::ExecuteCommand::METHOD => {
            let params: lsp_types::ExecuteCommandParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return Some(Response::new_err(req.id, -32602, e.to_string())),
            };
            let (path, alt) = command_args(&params.arguments);
            match params.command.as_str() {
                CMD_OPEN if !embedded => {
                    show_message(sender, MessageType::WARNING, NO_PANE.into());
                    Response::new_err(req.id, -32603, NO_PANE.into())
                }
                CMD_OPEN => {
                    // Alt-click on the toolbar button: open in the editor's pane.
                    let _ = core.send(CoreMsg::OpenPreview { path, split: !alt, explicit: true });
                    Response::new_ok(req.id, Value::Null)
                }
                CMD_EXPORT_PDF => {
                    let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
                    let _ = core.send(CoreMsg::ExportPdf { path, reply: reply_tx });
                    // Compiling takes a while: wait off the LSP thread.
                    let sender = sender.clone();
                    let id = req.id;
                    std::thread::spawn(move || {
                        let result = reply_rx.recv().unwrap_or_else(|_| Err("the server is shutting down".into()));
                        let resp = match result {
                            Ok(pdf) => {
                                show_message(&sender, MessageType::INFO, format!("Exported {}", pdf.display()));
                                Response::new_ok(id, json!(pdf))
                            }
                            // The patched Zed's toolbar reports failed commands.
                            Err(e) => Response::new_err(id, -32603, e),
                        };
                        let _ = sender.send(Message::Response(resp));
                    });
                    return None;
                }
                other => Response::new_err(req.id, -32601, format!("unknown command {other}")),
            }
        }
        _ => Response::new_err(req.id, -32601, format!("method not supported: {}", req.method)),
    })
}

fn handle_notification(not: Notification, core: &Sender<CoreMsg>) {
    use lsp_types::notification as n;
    let path_of = |uri: &Uri| uri_to_path(uri);
    let msg = match not.method.as_str() {
        n::DidOpenTextDocument::METHOD => serde_json::from_value::<lsp_types::DidOpenTextDocumentParams>(not.params)
            .ok()
            .and_then(|p| Some(CoreMsg::Open { path: path_of(&p.text_document.uri)?, text: p.text_document.text })),
        n::DidChangeTextDocument::METHOD => {
            serde_json::from_value::<lsp_types::DidChangeTextDocumentParams>(not.params).ok().and_then(|p| {
                // Full sync: the last change is the whole text.
                let text = p.content_changes.into_iter().last()?.text;
                Some(CoreMsg::Change { path: path_of(&p.text_document.uri)?, text })
            })
        }
        n::DidCloseTextDocument::METHOD => serde_json::from_value::<lsp_types::DidCloseTextDocumentParams>(not.params)
            .ok()
            .and_then(|p| Some(CoreMsg::Close { path: path_of(&p.text_document.uri)? })),
        n::DidChangeWatchedFiles::METHOD => {
            serde_json::from_value::<lsp_types::DidChangeWatchedFilesParams>(not.params).ok().map(|p| {
                CoreMsg::FilesChanged(p.changes.iter().filter_map(|c| path_of(&c.uri)).collect())
            })
        }
        n::DidChangeWorkspaceFolders::METHOD => {
            serde_json::from_value::<lsp_types::DidChangeWorkspaceFoldersParams>(not.params)
                .ok()
                .map(|p| CoreMsg::Roots(p.event.added.iter().filter_map(|f| path_of(&f.uri)).collect()))
        }
        n::DidChangeConfiguration::METHOD => {
            serde_json::from_value::<lsp_types::DidChangeConfigurationParams>(not.params)
                .ok()
                .and_then(|p| Config::from_json(&p.settings))
                .map(CoreMsg::Config)
        }
        _ => None,
    };
    if let Some(msg) = msg {
        let _ = core.send(msg);
    }
}
