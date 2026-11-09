#!/usr/bin/env python3
"""Smoke test for the Zed pane protocol: plays the role of patched Zed.

Listens on a socket, starts the server as a language server, opens a
document, attaches a "pane", requests bitmaps, clicks, and checks the jump.

Usage: python3 -I tests/embed_smoke.py target/release/typst-livepreview
"""

import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

BIN = sys.argv[1] if len(sys.argv) > 1 else "target/release/typst-livepreview"
TEXT = """#set page(width: 400pt, height: auto, margin: 20pt)
= Embedded

Hello brave new world.

#lorem(300)
"""
STRIP_PX = 128


def lsp_send(proc, msg):
    data = json.dumps({"jsonrpc": "2.0", **msg}).encode()
    proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
    proc.stdin.flush()


def read_exact(conn, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return bytes(buf)


def read_frame(conn):
    (hl,) = struct.unpack("<I", read_exact(conn, 4))
    header = json.loads(read_exact(conn, hl))
    (pl,) = struct.unpack("<I", read_exact(conn, 4))
    return header, read_exact(conn, pl)


def write_frame(conn, header, payload=b""):
    h = json.dumps(header).encode()
    conn.sendall(struct.pack("<I", len(h)) + h + struct.pack("<I", len(payload)) + payload)


def main():
    tmp = Path(tempfile.mkdtemp()).resolve()
    doc = tmp / "main.typ"
    doc.write_text(TEXT)
    sock_path = tmp / "zed.sock"
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(sock_path))
    server.listen(1)

    env = dict(os.environ, ZED_PREVIEW_SOCKET=str(sock_path), RUST_LOG="typst_livepreview=info")
    proc = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
    # Drain LSP stdout and answer server→client requests.
    def drain():
        while proc.stdout.readline():
            pass
    threading.Thread(target=drain, daemon=True).start()

    server.settimeout(10)
    conn, _ = server.accept()
    conn.settimeout(10)
    hello, _ = read_frame(conn)
    assert hello["type"] == "hello", hello
    assert hello["version"] == 3 and hello["kind"] == "typst" and hello["extensions"] == ["typ"] \
        and hello["layout"] == "fixed", hello
    print(f"✓ server connected (protocol v{hello['version']})")

    lsp_send(proc, {"id": 1, "method": "initialize", "params": {
        "processId": os.getpid(), "rootUri": tmp.as_uri(), "capabilities": {}}})
    lsp_send(proc, {"method": "initialized", "params": {}})
    lsp_send(proc, {"method": "textDocument/didOpen", "params": {"textDocument": {
        "uri": doc.as_uri(), "languageId": "typst", "version": 1, "text": TEXT}}})
    write_frame(conn, {"type": "attach", "path": str(doc)})

    def wait(kind):
        while True:
            h, p = read_frame(conn)
            if h["type"] == kind:
                return h, p

    t0 = time.time()
    d, _ = wait("document")
    page = d["pages"][0]
    print(f"✓ document: {len(d['pages'])} page(s), {page['width']:.0f}×{page['height']:.0f} pt "
          f"({(time.time() - t0) * 1000:.0f} ms incl. font loading)")

    scale = 2000  # 2 px/pt
    key = {"hash": page["hash"], "scale_milli": scale}
    t0 = time.time()
    write_frame(conn, {"type": "render", "wanted": [key], "requests": [{"index": 0, **key}]})
    b, payload = wait("bitmap")
    assert len(payload) == b["width"] * b["height"] * 4, "BGRA payload size"
    assert payload[3] == 255, "opaque alpha"
    print(f"✓ bitmap {b['width']}×{b['height']} in {(time.time() - t0) * 1000:.1f} ms")
    full = payload

    # The same page as strips (what Zed's pane asks for).
    n = -(-b["height"] // STRIP_PX)
    t0 = time.time()
    write_frame(conn, {"type": "render", "wanted": [key], "requests": [
        {"index": 0, **key, "strip_px": STRIP_PX, "first": 0, "end": n}]})
    strips = {}
    while len(strips) < n:
        h, p = wait("bitmap")
        assert h["strip_px"] == STRIP_PX and len(p) == h["width"] * h["height"] * 4
        strips[h["strip"]] = p
    dt = (time.time() - t0) * 1000
    joined = b"".join(strips[k] for k in range(n))
    assert len(joined) == len(full), "strips cover the page"
    differing = sum(1 for i in range(0, len(full), 4) if full[i:i + 4] != joined[i:i + 4])
    assert differing <= 16, f"{differing} pixels differ between strips and the full page"
    print(f"✓ {n} strips in {dt:.1f} ms, same pixels as the full page ({differing} rounding diffs)")

    # Edit → new document → render: the whole keystroke path over the socket.
    lines = TEXT.splitlines()
    col = lines[3].index("world")
    t0 = time.time()
    lsp_send(proc, {"method": "textDocument/didChange", "params": {
        "textDocument": {"uri": doc.as_uri(), "version": 2},
        "contentChanges": [{"range": {"start": {"line": 3, "character": col},
                                      "end": {"line": 3, "character": col + 5}}, "text": "Zed"}]}})
    d, _ = wait("document")
    page = d["pages"][0]
    assert "base" in page and page["dirty"], f"edit diffed against the previous version: {page}"
    pushed = page.get("pushed", [])
    assert pushed and pushed[0]["scale_milli"] == scale, f"changed strips pushed: {page}"
    expected = set(pushed[0]["strips"])
    assert 0 < len(expected) < n, f"only some strips are re-rendered: {expected} of {n}"
    got = set()
    while got != expected:
        h, p = wait("bitmap")
        assert h["hash"] == page["hash"], h
        got.add(h["strip"])
    dt = (time.time() - t0) * 1000
    print(f"✓ keystroke → changed strips {sorted(got)} of {n} pushed without a request: {dt:.1f} ms "
          f"(dirty bands {[[round(a), round(b)] for a, b in page['dirty']]} pt)")

    # Click on the heading → jump message.
    write_frame(conn, {"type": "click", "page": 0, "x": 40.0, "y": 32.0})
    j, _ = wait("jump")
    assert j["path"].endswith("main.typ")
    print(f"✓ click → jump to line {j['line'] + 1}, col {j['character'] + 1}")

    # Cursor from the editor → marker.
    write_frame(conn, {"type": "cursor", "path": str(doc), "line": 3, "character": 3})
    c, _ = wait("cursor")
    print(f"✓ cursor → marker on page {c['markers'][0]['page'] + 1} at y={c['markers'][0]['y']:.1f}pt")

    lsp_send(proc, {"id": 9, "method": "shutdown"})
    lsp_send(proc, {"method": "exit"})
    proc.wait(timeout=5)
    print(f"✓ clean exit (code {proc.returncode})")


if __name__ == "__main__":
    main()
