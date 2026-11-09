#!/usr/bin/env python3
"""Smoke test for tex-livepreview: plays the role of patched Zed.

Listens on a socket, starts the server as a language server, opens a
document, attaches a "pane", requests bitmaps, edits without saving, moves
the cursor, clicks, breaks the document, and exports a PDF.

Usage: python3 -I tests/embed_smoke.py target/debug/tex-livepreview
Needs latexmk and pdflatex on the PATH.
"""

import json
import os
import queue
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

BIN = sys.argv[1] if len(sys.argv) > 1 else "target/debug/tex-livepreview"
MAIN = r"""% !TEX program = pdflatex
\documentclass{article}
\begin{document}
\section{Embedded}
Hello brave new world, this is the first paragraph.

\input{sec/a}
\end{document}
"""
SECTION = "Text from an included file.\n"
STRIP_PX = 128


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


class Lsp:
    def __init__(self, proc):
        self.proc = proc
        self.responses = queue.Queue()
        self.next_id = 1
        threading.Thread(target=self.read, daemon=True).start()

    def send(self, msg):
        data = json.dumps({"jsonrpc": "2.0", **msg}).encode()
        self.proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
        self.proc.stdin.flush()

    def read(self):
        while True:
            length = None
            while True:
                line = self.proc.stdout.readline()
                if not line:
                    return
                if line.strip() == b"":
                    break
                if line.lower().startswith(b"content-length:"):
                    length = int(line.split(b":")[1])
            msg = json.loads(self.proc.stdout.read(length))
            if "method" in msg and "id" in msg:
                self.send({"id": msg["id"], "result": None})  # registerCapability
            elif "id" in msg:
                self.responses.put(msg)

    def request(self, method, params, timeout=60):
        rid = self.next_id
        self.next_id += 1
        self.send({"id": rid, "method": method, "params": params})
        while True:
            msg = self.responses.get(timeout=timeout)
            if msg["id"] == rid:
                return msg

    def notify(self, method, params):
        self.send({"method": method, "params": params})


class Pane:
    """Collects frames from the server on a thread."""

    def __init__(self, conn):
        self.conn = conn
        self.frames = queue.Queue()
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        try:
            while True:
                self.frames.put(read_frame(self.conn))
        except (EOFError, OSError):
            pass

    def wait(self, kind, timeout=60, pred=lambda h: True):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                h, p = self.frames.get(timeout=max(0.01, deadline - time.time()))
            except queue.Empty:
                break
            if h["type"] == kind and pred(h):
                return h, p
        raise AssertionError(f"no {kind} message")

    def send(self, header):
        write_frame(self.conn, header)


def check(cond, what):
    if not cond:
        raise AssertionError(what)
    print("ok:", what)


def main():
    tmp = Path(tempfile.mkdtemp()).resolve()
    main_tex = tmp / "main.tex"
    main_tex.write_text(MAIN)
    (tmp / "sec").mkdir()
    (tmp / "sec" / "a.tex").write_text(SECTION)
    uri = main_tex.as_uri()
    sock_path = tmp / "zed.sock"
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(sock_path))
    server.listen(1)

    env = dict(os.environ, ZED_PREVIEW_SOCKET=str(sock_path), RUST_LOG="tex_livepreview=debug")
    log = open(tmp / "server.log", "wb")
    proc = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, env=env)
    try:
        server.settimeout(10)
        conn, _ = server.accept()
        pane = Pane(conn)
        hello, _ = pane.wait("hello", 5)
        check(hello["kind"] == "latex" and "tex" in hello["extensions"] and hello["layout"] == "fixed", "hello")

        lsp = Lsp(proc)
        init = lsp.request("initialize", {"processId": None, "rootUri": tmp.as_uri(), "capabilities": {}})
        cmds = init["result"]["capabilities"]["executeCommandProvider"]["commands"]
        check("tex-livepreview.open" in cmds and "tex-livepreview.exportPdf" in cmds, "commands")
        lsp.notify("initialized", {})
        lsp.notify("textDocument/didOpen", {"textDocument": {"uri": uri, "languageId": "latex", "version": 1, "text": MAIN}})
        roots, _ = pane.wait("roots", 5)
        check(str(tmp) in roots["roots"], "roots")

        # The toolbar button asks for a pane.
        lsp.request("workspace/executeCommand", {"command": "tex-livepreview.open", "arguments": [uri, {"modifiers": {"alt": False}}]})
        op, _ = pane.wait("open_preview", 5)
        check(op["path"] == str(main_tex) and op["split"] is True, "open_preview")

        t = time.time()
        pane.send({"type": "attach", "path": str(main_tex)})
        target, _ = pane.wait("target", 5)
        check(target["title"] == "main.tex", "target")
        doc, _ = pane.wait("document", 60)
        print(f"   first compile: {time.time() - t:.2f}s (server says {doc['compile_ms']} ms)")
        pages = doc["pages"]
        check(len(pages) == 1 and pages[0]["width"] > 500, f"one page: {pages}")

        # Strips at 2 px/pt.
        page = pages[0]
        scale = 2000
        pane.send({
            "type": "render",
            "wanted": [{"hash": page["hash"], "scale_milli": scale}],
            "requests": [{"index": 0, "hash": page["hash"], "scale_milli": scale, "strip_px": STRIP_PX, "first": 0, "end": 3}],
        })
        strips = {}
        for _ in range(3):
            h, p = pane.wait("bitmap", 30)
            strips[h["strip"]] = (h, p)
        h0, p0 = strips[0]
        width_px = round(page["width"] * 2)
        check(h0["width"] == width_px and h0["height"] == STRIP_PX and len(p0) == width_px * STRIP_PX * 4, "strip size")
        _, p2 = strips[2]
        check(any(b < 200 for b in p0 + p2), "strips have ink")

        # Cursor on "Hello" (line 4, 0-based).
        pane.send({"type": "cursor", "path": str(main_tex), "line": 4, "character": 0})
        cur, _ = pane.wait("cursor", 10)
        m = cur["markers"][0]
        check(m["page"] == 0 and cur["scroll"] is True and 100 < m["x"] < 200, "cursor marker")
        pane.send({"type": "cursor", "path": str(main_tex), "line": 4, "character": 45})
        cur2, _ = pane.wait("cursor", 10)
        check(cur2["markers"][0]["x"] > m["x"] + 50, "marker moves along the line")

        # Click there: jump back to the line.
        pane.send({"type": "click", "page": 0, "x": m["x"] + 30, "y": m["y"] + m["height"] / 2})
        jump, _ = pane.wait("jump", 10)
        check(jump["path"] == str(main_tex) and jump["line"] == 4 and 0 < jump["character"] < 30, f"jump {jump}")

        # Unsaved edit of the main file: a second page appears.
        edited = MAIN.replace("\\input{sec/a}", "\\input{sec/a}\n\\newpage\nUNSAVED second page.")
        t = time.time()
        lsp.notify("textDocument/didChange", {"textDocument": {"uri": uri, "version": 2}, "contentChanges": [{"text": edited}]})
        doc2, _ = pane.wait("document", 30)
        print(f"   edit → document: {time.time() - t:.2f}s (server says {doc2['compile_ms']} ms)")
        check(len(doc2["pages"]) == 2, "unsaved edit compiled")
        check(doc2["pages"][0]["hash"] == page["hash"], "unchanged page keeps its hash")
        check(main_tex.read_text() == MAIN, "file on disk untouched")

        # Unsaved edit of the included file.
        sec_uri = (tmp / "sec" / "a.tex").as_uri()
        lsp.notify("textDocument/didOpen", {"textDocument": {"uri": sec_uri, "languageId": "latex", "version": 1, "text": SECTION}})
        lsp.notify("textDocument/didChange", {"textDocument": {"uri": sec_uri, "version": 2}, "contentChanges": [{"text": "Changed include.\n"}]})
        doc3, _ = pane.wait("document", 30)
        check(doc3["pages"][0]["hash"] != page["hash"] and doc3["pages"][1]["hash"] == doc2["pages"][1]["hash"], "include edit changes page 1 only")
        # The cursor in the included file stays on this document.
        pane.send({"type": "cursor", "path": str(tmp / "sec" / "a.tex"), "line": 0, "character": 3})
        cur3, _ = pane.wait("cursor", 10)
        check(cur3["markers"][0]["page"] == 0, "cursor in the included file")

        # A TeX error: the pane keeps the pages and shows the error.
        broken = edited.replace("Hello", "\\textbx{Hello}")
        lsp.notify("textDocument/didChange", {"textDocument": {"uri": uri, "version": 3}, "contentChanges": [{"text": broken}]})
        st, _ = pane.wait("status", 30, lambda h: h["error"])
        check("main.tex:5" in st["error"] and "Undefined control sequence" in st["error"], f"error status: {st['error']}")

        # Fix it and export (the export waits for the latest edits).
        lsp.notify("textDocument/didChange", {"textDocument": {"uri": uri, "version": 4}, "contentChanges": [{"text": edited}]})
        resp = lsp.request("workspace/executeCommand", {"command": "tex-livepreview.exportPdf", "arguments": [uri]})
        pdf = tmp / "main.pdf"
        check(resp.get("result") == str(pdf) and pdf.read_bytes().startswith(b"%PDF"), f"export {resp}")

        lsp.request("shutdown", None)
        lsp.notify("exit", None)
        proc.wait(timeout=10)
        print("all ok")
    except Exception:
        print("--- server log ---")
        log.flush()
        print((tmp / "server.log").read_text()[-4000:])
        raise
    finally:
        if proc.poll() is None:
            proc.kill()


if __name__ == "__main__":
    main()
