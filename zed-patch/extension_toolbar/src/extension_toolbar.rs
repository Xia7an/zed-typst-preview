//! Toolbar buttons declared by extensions.
//!
//! An extension lists its buttons in `extension.toml`:
//!
//! ```toml
//! [[toolbar_buttons]]
//! id = "export-pdf"
//! icon = "file_doc"                 # an `IconName`, in snake_case
//! tooltip = "Export PDF"
//! alt_tooltip = "open in this pane"  # optional: "⌥-click to …" hint
//! languages = ["Typst"]
//! language_server = "typst-livepreview"
//! command = "typst-livepreview.exportPdf"
//! ```
//!
//! The buttons show up in the editor toolbar, in this order, for files of
//! `languages`. A click sends `workspace/executeCommand` with `command` to the
//! language server `language_server` of that file, with the arguments
//! `[<file URI>, {"modifiers": {"alt", "shift", "control", "platform"}}]`.
//!
//! The server gives any feedback itself (e.g. `window/showMessage`); Zed only
//! reports when the command could not be run or failed.
//!
//! Zed's extension manifest ignores unknown keys, so the table is read here
//! from the installed extensions' `extension.toml` files.

use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::Arc;

use editor::Editor;
use extension_host::ExtensionStore;
use gpui::{AnyElement, App, Entity, Focusable as _, Global, Modifiers, WeakEntity, Window};
use icons::IconName;
use project::project_settings::ProjectSettings;
use serde::Deserialize;
use serde_json::{Value, json};
use settings::Settings as _;
use ui::{Tooltip, prelude::*, text_for_keystroke};
use workspace::item::ItemHandle;
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};

#[derive(Clone, Debug, Deserialize)]
struct ButtonManifest {
    id: String,
    icon: String,
    tooltip: String,
    #[serde(default)]
    alt_tooltip: Option<String>,
    languages: Vec<String>,
    language_server: String,
    command: String,
}

/// The part of `extension.toml` read here.
#[derive(Deserialize)]
struct ManifestToml {
    #[serde(default)]
    toolbar_buttons: Vec<ButtonManifest>,
}

struct Button {
    extension: Arc<str>,
    manifest: ButtonManifest,
    icon: IconName,
}

#[derive(Default)]
struct Buttons(Arc<Vec<Arc<Button>>>);

impl Global for Buttons {}

pub fn init(cx: &mut App) {
    cx.set_global(Buttons::default());
    let Some(store) = ExtensionStore::try_global(cx) else {
        return;
    };
    reload(&store, cx);
    // Sent after every (re)load of the extensions, including installs and
    // uninstalls.
    cx.subscribe(&store, |store, event: &extension_host::Event, cx| {
        if matches!(event, extension_host::Event::ExtensionsUpdated) {
            reload(&store, cx);
        }
    })
    .detach();
}

fn reload(store: &Entity<ExtensionStore>, cx: &mut App) {
    let store = store.read(cx);
    let extensions: Vec<(Arc<str>, PathBuf)> = store
        .installed_extensions()
        .keys()
        .map(|id| (id.clone(), store.installed_dir.join(id.as_ref())))
        .collect();
    let task = cx.background_spawn(async move {
        extensions
            .into_iter()
            .flat_map(|(id, dir)| load_buttons(id, dir))
            .collect::<Vec<_>>()
    });
    cx.spawn(async move |cx| {
        let buttons = task.await;
        cx.update(|cx| {
            cx.set_global(Buttons(Arc::new(buttons)));
            cx.refresh_windows();
        });
    })
    .detach();
}

fn load_buttons(extension: Arc<str>, dir: PathBuf) -> Vec<Arc<Button>> {
    let path = dir.join("extension.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let manifest: ManifestToml = match toml::from_str(&text) {
        Ok(manifest) => manifest,
        Err(error) => {
            log::warn!("extension {extension}: invalid toolbar_buttons: {error}");
            return Vec::new();
        }
    };
    manifest
        .toolbar_buttons
        .into_iter()
        .filter_map(|manifest| {
            let Ok(icon) = IconName::from_str(&manifest.icon) else {
                log::warn!(
                    "extension {extension}: unknown icon {:?} for toolbar button {:?}",
                    manifest.icon,
                    manifest.id
                );
                return None;
            };
            Some(Arc::new(Button {
                extension: extension.clone(),
                manifest,
                icon,
            }))
        })
        .collect()
}

/// The buttons for the item shown in a pane, for the quick action bar.
pub fn render_buttons(
    active_item: Option<&dyn ItemHandle>,
    workspace: &WeakEntity<Workspace>,
    cx: &mut App,
) -> Vec<AnyElement> {
    let Some(editor) = active_item.and_then(|item| item.act_as::<Editor>(cx)) else {
        return Vec::new();
    };
    let Some(language) = editor_local_file(&editor, cx).map(|(_, language)| language) else {
        return Vec::new();
    };
    let buttons = cx.try_global::<Buttons>().map(|b| b.0.clone()).unwrap_or_default();
    buttons
        .iter()
        .filter(|button| button.manifest.languages.iter().any(|l| *l == language))
        .map(|button| render_button(button.clone(), editor.clone(), workspace.clone()))
        .collect()
}

/// The path and language of a local file shown in `editor`.
fn editor_local_file(editor: &Entity<Editor>, cx: &App) -> Option<(PathBuf, String)> {
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let buffer = buffer.read(cx);
    let path = buffer.file()?.as_local()?.abs_path(cx);
    let language = buffer.language()?.name().0.to_string();
    Some((path, language))
}

fn render_button(button: Arc<Button>, editor: Entity<Editor>, workspace: WeakEntity<Workspace>) -> AnyElement {
    let id = SharedString::from(format!("extension-toolbar-{}-{}", button.extension, button.manifest.id));
    let title = SharedString::from(button.manifest.tooltip.clone());
    let alt_tooltip = button.manifest.alt_tooltip.clone();
    IconButton::new(id, button.icon)
        .icon_size(IconSize::Small)
        .style(ButtonStyle::Subtle)
        .tooltip(move |_window, cx| match &alt_tooltip {
            Some(hint) => Tooltip::with_meta(
                title.clone(),
                None,
                format!("{} to {hint}", text_for_keystroke(&Modifiers::alt(), "click", cx)),
                cx,
            ),
            None => Tooltip::simple(title.clone(), cx),
        })
        .on_click(move |_, window, cx| {
            let modifiers = window.modifiers();
            run(&button, &editor, &workspace, modifiers, window, cx);
        })
        .into_any_element()
}

fn run(
    button: &Button,
    editor: &Entity<Editor>,
    workspace: &WeakEntity<Workspace>,
    modifiers: Modifiers,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    // Clicking the toolbar does not focus the pane. Focus the editor, so its
    // pane is the active one: what the server opens in response (a preview)
    // is placed relative to it.
    window.focus(&editor.focus_handle(cx), cx);

    let manifest = &button.manifest;
    let Some((path, _)) = editor_local_file(editor, cx) else {
        return;
    };
    let Ok(uri) = lsp::Uri::from_file_path(&path) else {
        return;
    };
    let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
        return;
    };
    let lsp_store = workspace.read(cx).project().read(cx).lsp_store();
    let server = buffer.update(cx, |buffer, cx| {
        lsp_store.update(cx, |lsp_store, cx| {
            lsp_store
                .running_language_servers_for_local_buffer(buffer, cx)
                .find(|(_, server)| &*server.name().0 == manifest.language_server.as_str())
                .map(|(_, server)| server.clone())
        })
    });
    let Some(server) = server else {
        notify_error(
            &workspace,
            format!("{}: the language server {} is not running", manifest.tooltip, manifest.language_server),
            cx,
        );
        return;
    };

    let params = lsp::ExecuteCommandParams {
        command: manifest.command.clone(),
        arguments: vec![
            Value::String(uri.to_string()),
            json!({ "modifiers": {
                "alt": modifiers.alt,
                "shift": modifiers.shift,
                "control": modifiers.control,
                "platform": modifiers.platform,
            }}),
        ],
        ..Default::default()
    };
    let timeout = ProjectSettings::get_global(cx).global_lsp_settings.get_request_timeout();
    let request = server.request::<lsp::request::ExecuteCommand>(params, timeout);
    let title = manifest.tooltip.clone();
    let workspace = workspace.downgrade();
    cx.spawn(async move |cx| {
        if let Err(error) = request.await.into_response() {
            log::warn!("{title}: {error:#}");
            cx.update(|cx| {
                if let Some(workspace) = workspace.upgrade() {
                    notify_error(&workspace, format!("{title}: {error:#}"), cx);
                }
            });
        }
    })
    .detach();
}

struct ExtensionToolbarError;

fn notify_error(workspace: &Entity<Workspace>, message: String, cx: &mut App) {
    workspace.update(cx, |workspace, cx| {
        workspace.show_toast(
            Toast::new(NotificationId::unique::<ExtensionToolbarError>(), message),
            cx,
        );
    });
}
