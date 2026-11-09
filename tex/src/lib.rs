//! Zed extension: starts the `tex-livepreview` language server, which
//! compiles LaTeX (latexmk) on every edit and shows the pages in the preview
//! pane of the patched Zed.

use zed_extension_api::{self as zed, serde_json, settings::LspSettings, LanguageServerId, Result};

const SERVER: &str = "tex-livepreview";

struct TexLivePreview;

impl TexLivePreview {
    fn user_options(worktree: &zed::Worktree) -> Option<serde_json::Value> {
        let settings = LspSettings::for_worktree(SERVER, worktree).ok()?;
        settings.settings.or(settings.initialization_options)
    }
}

impl zed::Extension for TexLivePreview {
    fn new() -> Self {
        Self
    }

    fn language_server_command(&mut self, _id: &LanguageServerId, worktree: &zed::Worktree) -> Result<zed::Command> {
        let env = worktree.shell_env();
        // 1. `lsp.tex-livepreview.binary.path` in the settings.
        if let Some(binary) = LspSettings::for_worktree(SERVER, worktree).ok().and_then(|s| s.binary) {
            if let Some(path) = binary.path {
                return Ok(zed::Command { command: path, args: binary.arguments.unwrap_or_default(), env });
            }
        }
        // 2. On the PATH (home-manager module, `nix profile install`, `cargo install`).
        match worktree.which(SERVER) {
            Some(path) => Ok(zed::Command { command: path, args: Vec::new(), env }),
            None => Err(format!(
                "{SERVER} not found: install it on your PATH or set lsp.{SERVER}.binary.path"
            )),
        }
    }

    fn language_server_initialization_options(
        &mut self,
        _id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<serde_json::Value>> {
        Ok(Self::user_options(worktree))
    }

    fn language_server_workspace_configuration(
        &mut self,
        _id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<serde_json::Value>> {
        Ok(Self::user_options(worktree))
    }
}

zed::register_extension!(TexLivePreview);
