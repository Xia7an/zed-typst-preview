# Zed preview pane protocol (v2)

The patched Zed (`zed-patch/typst_preview`) listens on a Unix socket:

```
/tmp/zed-typst-preview-<uid>/<zed pid>.sock      (directory mode 0700)
```

`typst-livepreview` is started by Zed as a language server, so it finds the
socket through its parent pid (override: `ZED_TYPST_PREVIEW_SOCKET`). If it
can connect, the preview is shown in Zed's pane; otherwise the server opens
its own native window.

## Framing

Both directions use the same frame:

```
u32 LE header length | JSON header | u32 LE payload length | payload bytes
```

The header always has a `"type"`. Only `bitmap` carries a payload.
Page hashes are 128-bit, hex-encoded strings. Coordinates are in typographic
points (pt), from the top-left of a page; pages are 0-based.

## Server → Zed

| type           | fields                                                                 |
| -------------- | ---------------------------------------------------------------------- |
| `hello`        | `version`, `pid`                                                       |
| `roots`        | `roots`: workspace folders (used to route panes to servers)            |
| `open_preview` | `path`: user asked for a preview (code action / `auto_open`)          |
| `target`       | `path`, `title`: the previewed main file changed                       |
| `document`     | `generation`, `pages: [page]` (below), `compile_ms`, `warnings`        |
| `status`       | `error` (first message or null), `errors`, `compile_ms`, `warnings`    |
| `bitmap`       | `hash`, `scale_milli`, `width`, `height`, `strip`, `strip_px`; payload: BGRA8, opaque |
| `skipped`      | `hash`, `scale_milli`, `first`, `end`: requested strips were dropped (stale) |
| `cursor`       | `markers: [{page, x, y, height}]`, `scroll`: scroll into view?         |
| `reveal`       | `marker`: scroll there and flash (internal links)                      |
| `jump`         | `path`, `line`, `character` (UTF-16): move the editor cursor           |
| `invalidate`   | drop all bitmaps (e.g. `invert_colors` changed)                        |

A failed compilation only sends `status`: the pane keeps the last good pages.

A `page` is `{width, height, hash}`, and after an edit of that page also:

- `base`: the hash of the previous version of the page (same index),
- `dirty: [[top, bottom], …]` (pt): the bands that may look different. All
  other pixels are identical to `base`, so the pane reuses those strips,
- `pushed: [{scale_milli, strip_px, strips: [k, …]}]`: strips the server is
  already rendering (the dirty strips at the resolutions the pane last
  asked for). They arrive as `bitmap`s without a request.

`bitmap` without `strip` is a whole page (answer to a request without strip
fields).

## Zed → server

| type     | fields                                                                          |
| -------- | ------------------------------------------------------------------------------- |
| `attach` | `path`: a pane opened (or was re-targeted) for this file                         |
| `detach` | the last pane closed (the server stops compiling)                                |
| `render` | `wanted: [{hash, scale_milli}]`, `requests: [{index, hash, scale_milli, strip_px, first, end}]` |
| `click`  | `page`, `x`, `y`                                                                 |
| `cursor` | `path`, `line`, `character` (UTF-16): editor selection moved                      |

`scale_milli` is pixels per point × 1000 (zoom × display scale factor).

## Strips

Bitmaps are horizontal strips of a page: strip `k` holds the pixel rows
`k × strip_px` up to `(k + 1) × strip_px` (the last one may be shorter) of
the page rendered at `scale_milli`. The page is `round(width × scale)` ×
`round(height × scale)` pixels, rounded like `typst-render` (in `f32`).
A request asks for strips `first..end` of page `index`; the pane uses
`strip_px = 128` and asks for the strips on screen plus one screen above and
below. `wanted` lets the server skip queued renders that scrolled out of view.

Strip `k` is dirty if `[top × scale − 1, bottom × scale + 1]` (rounded
outwards) of some dirty band touches its rows; server and pane compute this
the same way (`dirty_strips`).
