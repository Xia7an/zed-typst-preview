//! User configuration.
//!
//! Read from the LSP `initializationOptions` (Zed:
//! `lsp.typst-livepreview.initialization_options`) and updated through
//! `workspace/didChangeConfiguration` (Zed: `lsp.typst-livepreview.settings`).

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Always preview this file (relative to the workspace root or absolute).
    /// When unset, the preview follows the file you are editing.
    #[serde(alias = "mainFile", alias = "main_file")]
    pub main: Option<String>,
    /// Project root used to resolve absolute paths like `/assets/logo.png`.
    /// Defaults to the workspace folder containing the main file.
    #[serde(alias = "rootPath", alias = "root_path")]
    pub root: Option<String>,
    /// Open the preview when the first Typst file is opened (off by default,
    /// like VS Code: use the eye button, the command palette or the code
    /// action).
    #[serde(alias = "autoOpen")]
    pub auto_open: bool,
    /// Scroll the preview to the cursor and show a cursor indicator.
    #[serde(alias = "followCursor")]
    pub follow_cursor: bool,
    /// After clicking in the preview, bring the editor to the front.
    #[serde(alias = "focusEditorOnJump")]
    pub focus_editor_on_jump: bool,
    /// Keep the preview window above other windows.
    #[serde(alias = "alwaysOnTop")]
    pub always_on_top: bool,
    /// Invert page lightness (dark mode) while keeping hues.
    #[serde(alias = "invertColors")]
    pub invert_colors: bool,
    /// Background color behind the pages (`#rrggbb`).
    pub background: String,
    /// Additional directories to search for fonts.
    #[serde(alias = "fontPaths")]
    pub font_paths: Vec<String>,
    /// Do not load system fonts (only embedded + `font_paths`).
    #[serde(alias = "ignoreSystemFonts")]
    pub ignore_system_fonts: bool,
    /// Command used to jump to a source location when the editor does not
    /// support `window/showDocument`. Receives `path:line:column`.
    #[serde(alias = "editorCommand")]
    pub editor_command: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            main: None,
            root: None,
            auto_open: false,
            follow_cursor: true,
            focus_editor_on_jump: true,
            always_on_top: false,
            invert_colors: false,
            background: "#525659".into(),
            font_paths: Vec::new(),
            ignore_system_fonts: false,
            editor_command: None,
        }
    }
}

impl Config {
    /// Parses a config from JSON. Accepts both the bare object and an object
    /// nested under `typst-livepreview` / `typstLivePreview`.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        if value.is_null() || value.as_object().is_some_and(|o| o.is_empty()) {
            return None;
        }
        for key in ["typst-livepreview", "typstLivePreview", "typst_livepreview"] {
            if let Some(inner) = value.get(key) {
                return serde_json::from_value(inner.clone()).ok();
            }
        }
        serde_json::from_value(value.clone()).ok()
    }

    pub fn background_rgb(&self) -> u32 {
        parse_hex_color(&self.background).unwrap_or(0x525659)
    }
}

fn parse_hex_color(s: &str) -> Option<u32> {
    let s = s.trim().trim_start_matches('#');
    match s.len() {
        6 => u32::from_str_radix(s, 16).ok(),
        3 => {
            let v = u32::from_str_radix(s, 16).ok()?;
            let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
            Some((r * 17) << 16 | (g * 17) << 8 | (b * 17))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_and_aliases() {
        let v = serde_json::json!({ "typst-livepreview": { "invertColors": true, "main": "a.typ" } });
        let c = Config::from_json(&v).unwrap();
        assert!(c.invert_colors);
        assert_eq!(c.main.as_deref(), Some("a.typ"));
        assert!(!c.auto_open);
        assert!(c.follow_cursor);
    }

    #[test]
    fn hex() {
        assert_eq!(parse_hex_color("#fff"), Some(0xffffff));
        assert_eq!(parse_hex_color("#123456"), Some(0x123456));
        assert_eq!(parse_hex_color("zz"), None);
    }
}
