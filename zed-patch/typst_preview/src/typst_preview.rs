//! Native Typst live preview pane.
//!
//! Compilation and rasterization happen in the `typst-livepreview` language
//! server (installed by the Typst Live Preview extension). This crate hosts a
//! Unix socket the server connects to, and shows the page bitmaps it sends in
//! a regular workspace pane, drawn by GPUI like any other image.
//!
//! - Clicking the preview moves the editor cursor to the clicked source.
//! - Moving the cursor scrolls the preview and shows a cursor indicator.

mod hub;
mod protocol;
pub mod typst_preview_view;

use gpui::{App, actions};
use workspace::Workspace;

pub use typst_preview_view::TypstPreviewView;

actions!(
    typst_preview,
    [
        /// Opens a live preview of the current Typst file.
        OpenPreview,
        /// Opens a live preview of the current Typst file in a split pane.
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
        TypstPreviewView::register(workspace, window, cx);
    })
    .detach();
}
