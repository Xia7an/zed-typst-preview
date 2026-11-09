//! Native live preview panes for external renderers.
//!
//! Layout and rasterization happen in a preview server: a language server
//! started by an extension (e.g. `typst-livepreview`). This crate hosts a Unix
//! socket the servers connect to, and shows the page bitmaps they send in a
//! regular workspace pane, drawn by GPUI like any other image. Each server
//! says which kind of document it renders and for which file extensions, so
//! several kinds (Typst, Markdown, …) can be previewed side by side.
//!
//! - Clicking the preview moves the editor cursor to the clicked source.
//! - Moving the cursor scrolls the preview and shows a cursor indicator.

mod hub;
mod protocol;
pub mod external_preview_view;

use gpui::{App, actions};
use workspace::Workspace;

pub use external_preview_view::ExternalPreviewView;

actions!(
    external_preview,
    [
        /// Opens a live preview of the current file, if a preview server
        /// handles its file type.
        OpenPreview,
        /// Opens a live preview of the current file in a split pane.
        OpenPreviewToTheSide,
        /// Zooms the preview in.
        ZoomIn,
        /// Zooms the preview out.
        ZoomOut,
        /// Fits the preview pages to the pane width.
        FitWidth,
    ]
);

pub fn init(cx: &mut App) {
    hub::init(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        ExternalPreviewView::register(workspace, window, cx);
    })
    .detach();
}
