#!/usr/bin/env python3
"""End-to-end smoke test: drives the server like Zed does.

Opens a document, types into it, moves the cursor and clicks into the preview
(synthesized via TYPST_LIVEPREVIEW_TEST_CLICK), then checks that the server
asks the editor to show the clicked source location.

Usage: python3 -I tests/lsp_smoke.py target/release/typst-livepreview
Opens a real preview window for a few seconds.
"""

import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import queue
from pathlib import Path

BIN = sys.argv[1] if len(sys.argv) > 1 else "target/release/typst-livepreview"

TEXT = """#set page(width: 400pt, height: auto, margin: 20pt)
= Smoke test

Hello brave new world.
"""


def main():
    tmp = Path(tempfile.mkdtemp()).resolve()
    doc = tmp / "main.typ"
    doc.write_text(TEXT)
    dump = tmp / "frame.png"
    uri = doc.as_uri()

    env = dict(os.environ)
    env["TYPST_LIVEPREVIEW_DUMP"] = str(dump)
    if os.environ.get("NODUMP"):
        # Only dump the first frames: stop dumping after the window is up.
        env["TYPST_LIVEPREVIEW_DUMP_ONCE"] = "1"
    env["RUST_LOG"] = os.environ.get("RUST_LOG", "typst_livepreview=debug")
    # Click into the first text line of the page (window px; page starts at the
    # top margin). Any text will do: we only check that a jump is requested.
    env["TYPST_LIVEPREVIEW_TEST_CLICK"] = env.get("CLICK", "")
    env["TYPST_LIVEPREVIEW_NO_EMBED"] = "1"  # force the standalone window
    log = open(tmp / "server.log", "w")
    proc = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, env=env)
    inbox: "queue.Queue[dict]" = queue.Queue()

    def reader():
        while True:
            headers = {}
            while True:
                line = proc.stdout.readline()
                if not line:
                    return
                line = line.decode().strip()
                if not line:
                    break
                k, v = line.split(":", 1)
                headers[k.lower()] = v.strip()
            body = proc.stdout.read(int(headers["content-length"]))
            inbox.put(json.loads(body))

    threading.Thread(target=reader, daemon=True).start()

    def send(msg):
        data = json.dumps({"jsonrpc": "2.0", **msg}).encode()
        proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
        proc.stdin.flush()

    backlog = []

    def wait_for(pred, timeout=10.0):
        for msg in backlog:
            if pred(msg):
                backlog.remove(msg)
                return msg
        deadline = time.time() + timeout
        seen = []
        while time.time() < deadline:
            try:
                msg = inbox.get(timeout=0.1)
            except queue.Empty:
                continue
            # Answer server → client requests like Zed would.
            if "method" in msg and "id" in msg:
                if msg["method"] == "window/showDocument":
                    send({"id": msg["id"], "result": {"success": True}})
                else:
                    send({"id": msg["id"], "result": None})
            if pred(msg):
                return msg
            seen.append(msg)
            backlog.append(msg)
        raise AssertionError(f"timed out; saw {seen}")

    send({"id": 1, "method": "initialize", "params": {
        "processId": os.getpid(), "rootUri": tmp.as_uri(), "capabilities": {},
        "workspaceFolders": [{"uri": tmp.as_uri(), "name": "tmp"}],
    }})
    init = wait_for(lambda m: m.get("id") == 1)
    caps = init["result"]["capabilities"]
    assert caps["textDocumentSync"] == 2, caps
    assert caps["codeActionProvider"] is True
    send({"method": "initialized", "params": {}})
    print("✓ initialize")

    send({"method": "textDocument/didOpen", "params": {"textDocument": {
        "uri": uri, "languageId": "typst", "version": 1, "text": TEXT}}})

    # The code action offered while the preview is closed.
    send({"id": 5, "method": "textDocument/codeAction", "params": {
        "textDocument": {"uri": uri},
        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
        "context": {"diagnostics": []}}})
    actions = wait_for(lambda m: m.get("id") == 5)["result"]
    assert actions and actions[0]["command"]["command"] == "typst-livepreview.open", actions
    assert [a["command"]["command"] for a in actions] == ["typst-livepreview.open", "typst-livepreview.exportPdf"], actions
    send({"id": 6, "method": "workspace/executeCommand", "params": actions[0]["command"]})
    wait_for(lambda m: m.get("id") == 6)
    print("✓ 'Open Typst Live Preview' code action")

    t0 = time.time()
    while not dump.exists() and time.time() - t0 < 10:
        time.sleep(0.05)
    assert dump.exists(), "preview window did not draw"
    print(f"✓ preview opened and drew in {time.time() - t0:.2f}s (incl. font loading)")

    # Type: replace "world" by "Typst" (incremental change, UTF-16 positions).
    line = 3
    col = TEXT.splitlines()[line].index("world")
    before = dump.stat().st_mtime_ns
    t0 = time.time()
    send({"method": "textDocument/didChange", "params": {
        "textDocument": {"uri": uri, "version": 2},
        "contentChanges": [{"range": {"start": {"line": line, "character": col},
                                      "end": {"line": line, "character": col + 5}},
                            "text": "Typst"}]}})
    if os.environ.get("NODUMP"):
        time.sleep(0.3)
    else:
        while dump.stat().st_mtime_ns == before and time.time() - t0 < 5:
            time.sleep(0.002)
        assert dump.stat().st_mtime_ns != before, "preview did not update"
        print(f"✓ edit visible after {(time.time() - t0) * 1000:.1f} ms (incl. PNG dump)")

    # A burst of keystrokes, like fast typing (one change per message).
    for i, ch in enumerate("abcdefghij"):
        c = col + 5 + i
        send({"method": "textDocument/didChange", "params": {
            "textDocument": {"uri": uri, "version": 3 + i},
            "contentChanges": [{"range": {"start": {"line": line, "character": c},
                                          "end": {"line": line, "character": c}},
                                "text": ch}]}})
        time.sleep(0.08)

    # Cursor move → code action request (how Zed reports the cursor).
    send({"id": 2, "method": "textDocument/codeAction", "params": {
        "textDocument": {"uri": uri},
        "range": {"start": {"line": line, "character": 2}, "end": {"line": line, "character": 2}},
        "context": {"diagnostics": []}}})
    resp = wait_for(lambda m: m.get("id") == 2)
    # Outside Zed's pane, "Export PDF" stays available (no toolbar button).
    titles = [a["title"] for a in resp["result"]]
    assert titles == ["Export PDF"], f"only 'Export PDF' expected while open: {resp}"
    print("✓ cursor sync request handled")

    # Export PDF, with the arguments a toolbar button sends. The PDF reflects
    # the unsaved buffer, so it must contain the typed text.
    pdf = tmp / "main.pdf"
    mods = {"modifiers": {"alt": False, "shift": False, "control": False, "platform": False}}
    send({"id": 7, "method": "workspace/executeCommand",
          "params": {"command": "typst-livepreview.exportPdf", "arguments": [uri, mods]}})
    resp = wait_for(lambda m: m.get("id") == 7, timeout=30)
    assert "error" not in resp, resp
    assert Path(resp["result"]) == pdf, resp
    assert pdf.read_bytes().startswith(b"%PDF"), "not a PDF"
    info = wait_for(lambda m: m.get("method") == "window/showMessage")
    assert info["params"]["type"] == 3 and "main.pdf" in info["params"]["message"], info
    print(f"✓ exportPdf wrote {pdf.name} ({pdf.stat().st_size} bytes) and notified")

    # A compile error: error response + error message, no PDF written.
    pdf.unlink()
    send({"method": "textDocument/didChange", "params": {
        "textDocument": {"uri": uri, "version": 100},
        "contentChanges": [{"range": {"start": {"line": 0, "character": 0},
                                      "end": {"line": 0, "character": 0}},
                            "text": "#undefined-function()\n"}]}})
    send({"id": 8, "method": "workspace/executeCommand",
          "params": {"command": "typst-livepreview.exportPdf", "arguments": [uri]}})
    resp = wait_for(lambda m: m.get("id") == 8, timeout=30)
    assert "error" in resp and "main.typ:1:" in resp["error"]["message"], resp
    err = wait_for(lambda m: m.get("method") == "window/showMessage")
    assert err["params"]["type"] == 1, err
    assert not pdf.exists(), "PDF written despite the error"
    print(f"✓ exportPdf reports compile errors: {resp['error']['message']}")

    # The toolbar button's "open" (alt-click = this pane) is accepted.
    send({"id": 9, "method": "workspace/executeCommand", "params": {
        "command": "typst-livepreview.open",
        "arguments": [uri, {"modifiers": {"alt": True}}]}})
    resp = wait_for(lambda m: m.get("id") == 9)
    assert "error" not in resp, resp
    print("✓ open with modifiers accepted")

    if env["TYPST_LIVEPREVIEW_TEST_CLICK"]:
        msg = wait_for(lambda m: m.get("method") == "window/showDocument")
        sel = msg["params"]["selection"]["start"]
        assert msg["params"]["uri"].endswith("main.typ"), msg
        print(f"✓ click → showDocument line {sel['line'] + 1}, col {sel['character'] + 1}")

    send({"id": 3, "method": "shutdown"})
    wait_for(lambda m: m.get("id") == 3)
    send({"method": "exit"})
    proc.wait(timeout=5)
    print(f"✓ clean exit (code {proc.returncode})")
    log.close()
    lat = [l.split("on screen: ")[1].strip() for l in (tmp / "server.log").read_text().splitlines() if "on screen" in l]
    print(f"  in-process edit → on screen: {', '.join(lat)}")


if __name__ == "__main__":
    main()
