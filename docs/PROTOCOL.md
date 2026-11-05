# Zed preview pane protocol (v1)

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
| `document`     | `generation`, `pages: [{width, height, hash}]`, `compile_ms`, `warnings` |
| `status`       | `error` (first message or null), `errors`, `compile_ms`, `warnings`    |
| `bitmap`       | `hash`, `scale_milli`, `width`, `height`; payload: BGRA8, opaque       |
| `skipped`      | `hash`, `scale_milli`: a requested render was dropped (stale)          |
| `cursor`       | `markers: [{page, x, y, height}]`, `scroll`: scroll into view?         |
| `reveal`       | `marker`: scroll there and flash (internal links)                      |
| `jump`         | `path`, `line`, `character` (UTF-16): move the editor cursor           |
| `invalidate`   | drop all bitmaps (e.g. `invert_colors` changed)                        |

A failed compilation only sends `status`: the pane keeps the last good pages.

## Zed → server

| type     | fields                                                                          |
| -------- | ------------------------------------------------------------------------------- |
| `attach` | `path`: a pane opened (or was re-targeted) for this file                         |
| `detach` | the last pane closed (the server stops compiling)                                |
| `render` | `wanted: [{hash, scale_milli}]`, `requests: [{index, hash, scale_milli}]`        |
| `click`  | `page`, `x`, `y`                                                                 |
| `cursor` | `path`, `line`, `character` (UTF-16): editor selection moved                      |

`scale_milli` is pixels per point × 1000 (zoom × display scale factor). The
pane requests the visible pages plus one on each side; `wanted` lets the
server skip queued renders that scrolled out of view.
