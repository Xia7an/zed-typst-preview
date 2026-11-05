//! Zed extension: starts the `typst-livepreview` language server, which
//! compiles Typst on every keystroke and shows the preview (in Zed's preview
//! pane when available, otherwise in a native window).

use std::fs;

use zed_extension_api::{self as zed, serde_json, settings::LspSettings, LanguageServerId, Result};

const SERVER: &str = "typst-livepreview";
const REPO: &str = "inoyu/zed-typst-livepreview";

struct TypstLivePreview {
    cached_binary: Option<String>,
}

impl TypstLivePreview {
    fn binary(&mut self, id: &LanguageServerId, worktree: &zed::Worktree) -> Result<(String, Vec<String>)> {
        // 1. `lsp.typst-livepreview.binary.path` in the settings.
        if let Some(binary) = LspSettings::for_worktree(SERVER, worktree).ok().and_then(|s| s.binary) {
            if let Some(path) = binary.path {
                return Ok((path, binary.arguments.unwrap_or_default()));
            }
        }
        // 2. On the PATH (e.g. `nix profile install`, `cargo install`).
        if let Some(path) = worktree.which(SERVER) {
            return Ok((path, Vec::new()));
        }
        // 3. A previously downloaded release.
        if let Some(path) = &self.cached_binary {
            if fs::metadata(path).is_ok_and(|m| m.is_file()) {
                return Ok((path.clone(), Vec::new()));
            }
        }
        // 4. Download the latest release from GitHub.
        zed::set_language_server_installation_status(id, &zed::LanguageServerInstallationStatus::CheckingForUpdate);
        let release = zed::latest_github_release(
            REPO,
            zed::GithubReleaseOptions { require_assets: true, pre_release: false },
        )?;
        let (os, arch) = zed::current_platform();
        let target = match (os, arch) {
            (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
            (zed::Os::Mac, zed::Architecture::X8664) => "x86_64-apple-darwin",
            (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-gnu",
            (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
            _ => return Err(format!("{SERVER}: no prebuilt binary for this platform; install it on your PATH")),
        };
        let asset_name = format!("{SERVER}-{target}.tar.gz");
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| format!("{SERVER}: release {} has no asset {asset_name}", release.version))?;

        let dir = format!("{SERVER}-{}", release.version);
        let bin = format!("{dir}/{SERVER}");
        if !fs::metadata(&bin).is_ok_and(|m| m.is_file()) {
            zed::set_language_server_installation_status(id, &zed::LanguageServerInstallationStatus::Downloading);
            zed::download_file(&asset.download_url, &dir, zed::DownloadedFileType::GzipTar)
                .map_err(|e| format!("{SERVER}: download failed: {e}"))?;
            zed::make_file_executable(&bin)?;
            // Remove older versions.
            if let Ok(entries) = fs::read_dir(".") {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with(&format!("{SERVER}-")) && name != dir {
                        fs::remove_dir_all(entry.path()).ok();
                    }
                }
            }
        }
        self.cached_binary = Some(bin.clone());
        Ok((bin, Vec::new()))
    }

    fn user_options(worktree: &zed::Worktree) -> Option<serde_json::Value> {
        let settings = LspSettings::for_worktree(SERVER, worktree).ok()?;
        settings.settings.or(settings.initialization_options)
    }
}

impl zed::Extension for TypstLivePreview {
    fn new() -> Self {
        Self { cached_binary: None }
    }

    fn language_server_command(&mut self, id: &LanguageServerId, worktree: &zed::Worktree) -> Result<zed::Command> {
        let (command, args) = self.binary(id, worktree)?;
        Ok(zed::Command { command, args, env: worktree.shell_env() })
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

zed::register_extension!(TypstLivePreview);
