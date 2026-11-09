//! User configuration.
//!
//! Read from the LSP `initializationOptions` and updated through
//! `workspace/didChangeConfiguration` (Zed: `lsp.tex-livepreview.settings`).

use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Always preview this file (relative to the workspace root or absolute).
    /// When unset, the main file is found from `% !TEX root`, from
    /// `\documentclass`, or from the files that `\input` the edited one.
    #[serde(alias = "mainFile", alias = "main_file")]
    pub main: Option<String>,
    /// `auto` (from `% !TEX program`, a latexmkrc that sets `$pdf_mode`, or
    /// the document class), `latexmkrc` (only what latexmkrc says),
    /// `pdflatex`, `lualatex`, `xelatex`, `uplatex` or `platex`.
    pub engine: String,
    /// Extra arguments for latexmk.
    #[serde(alias = "latexmkArgs")]
    pub latexmk_args: Vec<String>,
    /// Replaces latexmk: the program and its arguments. `%MAIN%` is the main
    /// file name (relative to its directory, which is the working directory)
    /// and `%OUTDIR%` the output directory, where `<jobname>.pdf` and
    /// `<jobname>.synctex.gz` must end up.
    pub command: Option<Vec<String>>,
    /// Wait this long after the last edit before compiling.
    #[serde(alias = "debounceMs")]
    pub debounce_ms: u64,
    /// Open the preview when the first TeX file is opened.
    #[serde(alias = "autoOpen")]
    pub auto_open: bool,
    /// Scroll the preview to the cursor and show a cursor indicator.
    #[serde(alias = "followCursor")]
    pub follow_cursor: bool,
    /// Invert page lightness (dark mode) while keeping hues.
    #[serde(alias = "invertColors")]
    pub invert_colors: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            main: None,
            engine: "auto".into(),
            latexmk_args: Vec::new(),
            command: None,
            debounce_ms: 150,
            auto_open: false,
            follow_cursor: true,
            invert_colors: false,
        }
    }
}

impl Config {
    /// Parses a config from JSON. Accepts both the bare object and an object
    /// nested under `tex-livepreview`.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        if value.is_null() || value.as_object().is_some_and(|o| o.is_empty()) {
            return None;
        }
        for key in ["tex-livepreview", "texLivePreview", "tex_livepreview"] {
            if let Some(inner) = value.get(key) {
                return serde_json::from_value(inner.clone()).ok();
            }
        }
        serde_json::from_value(value.clone()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_and_aliases() {
        let v = serde_json::json!({ "tex-livepreview": { "invertColors": true, "main": "a.tex", "engine": "lualatex" } });
        let c = Config::from_json(&v).unwrap();
        assert!(c.invert_colors);
        assert_eq!(c.main.as_deref(), Some("a.tex"));
        assert_eq!(c.engine, "lualatex");
        assert!(c.follow_cursor);
        assert_eq!(c.debounce_ms, 150);
    }
}
