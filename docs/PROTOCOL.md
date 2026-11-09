# Zed preview pane protocol (v3)

The patched Zed (`zed-patch/external_preview`) shows previews rendered by
external servers: it lays out and draws page bitmaps the server sends, and
reports clicks and the editor cursor back. The pane knows nothing about the
document format, so any renderer (Typst, Markdown, …) can use it.

Zed listens on a Unix socket:

```
/tmp/zed-preview-<uid>/<zed pid>.sock      (directory mode 0700)
```

A preview server is started by Zed as a language server (declared by an
extension), so it finds the socket through its parent pid (override:
`ZED_PREVIEW_SOCKET`). `typst-livepreview` also tries the socket of protocol v2
(`/tmp/zed-typst-preview-<uid>/…`), and opens its own native window if it
cannot connect at all.

## Framing

Both directions use the same frame:

```
u32 LE header length | JSON header | u32 LE payload length | payload bytes
```

The header always has a `"type"`. Only `bitmap` carries a payload.
Page hashes are 128-bit, hex-encoded strings. Coordinates are in the
server's document units (Typst: typographic points), from the top-left of a
page; pages are 0-based.

## Kinds and layouts

A server says in its `hello` which **kind** of document it renders (an
identifier such as `"typst"` or `"markdown"`) and for which file
**extensions**. Zed keeps one preview per kind and workspace, binds it to the
server of that kind whose workspace root contains the previewed file, and
follows the cursor of editors showing files with those extensions.

The **layout** decides who sizes the pages:

- `fixed`: the server decides the page sizes (Typst, PDF). The pane fits the
  widest page to its width, or zooms freely.
- `reflow`: the server lays the document out to the width the pane asks for
  in `viewport` messages (like a web page). The pages fill the pane width;
  zooming changes that width (so the text gets bigger or smaller).

## Server → Zed

| type           | fields                                                                 |
| -------------- | ---------------------------------------------------------------------- |
| `hello`        | `version` (3), `pid`, `kind`, `name` (shown to users), `extensions` (e.g. `["typ"]`, no dot), `layout` (`"fixed"` or `"reflow"`) |
| `roots`        | `roots`: workspace folders (used to route panes to servers)            |
| `open_preview` | `path`: user asked for a preview (toolbar button / code action / `auto_open`); `split` (default `true`): next to the active pane, else in it; `explicit` (default `false`): reveal the preview even if one is open |
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
- `dirty: [[top, bottom], …]` (units): the bands that may look different. All
  other pixels are identical to `base`, so the pane reuses those strips,
- `pushed: [{scale_milli, strip_px, strips: [k, …]}]`: strips the server is
  already rendering (the dirty strips at the resolutions the pane last
  asked for). They arrive as `bitmap`s without a request.

`bitmap` without `strip` is a whole page (answer to a request without strip
fields).

## Zed → server

| type       | fields                                                                          |
| ---------- | ------------------------------------------------------------------------------- |
| `attach`   | `path`: a pane opened (or was re-targeted) for this file                         |
| `detach`   | the last pane closed (the server can stop rendering)                             |
| `viewport` | `width` (units): lay the pages out this wide. `layout: reflow` only; sent when the pane is bound, resized or zoomed |
| `render`   | `wanted: [{hash, scale_milli}]`, `requests: [{index, hash, scale_milli, strip_px, first, end}]` |
| `click`    | `page`, `x`, `y`                                                                 |
| `cursor`   | `path`, `line`, `character` (UTF-16): editor selection moved                      |

`scale_milli` is pixels per unit × 1000 (zoom × display scale factor).

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

## Writing a preview server

The smallest server that works:

1. Declare it in the extension's `extension.toml` as a language server for
   the languages it previews, and (optionally) a toolbar button whose command
   makes the server send `open_preview` (see `[[toolbar_buttons]]` in this
   repository's `extension.toml`). It must answer the LSP `initialize` and
   `shutdown` requests; document contents arrive through `didOpen`/`didChange`.
2. Connect to the socket and send `hello` (with `kind`, `extensions`,
   `layout`), then `roots`.
3. On `attach`, render the file and send a `document`. With `layout:
   reflow`, wait for the `viewport` width first and lay out to it; send a new
   `document` when it changes.
4. Answer `render` requests with `bitmap` strips.

Diffing (`base`/`dirty`/`pushed`), `cursor`, `click`/`jump` and `reveal` are
optional: a server without them sends every changed page with a new hash and
the pane requests it again.

## Compatibility

- A `hello` without `kind` (protocol v2) is treated as `kind: "typst"`,
  `extensions: ["typ"]`, `layout: "fixed"`.
- Zed ignores fields and message types it does not know, and so does
  `typst-livepreview`.
