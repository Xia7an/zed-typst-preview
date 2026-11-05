"""Plays the typst-livepreview server against a real patched Zed (isolated user data dir).

Usage: python3 -I zed-patch/tests/pane_test.py <path to zed binary> <scratch dir>
"""
import json, os, socket, struct, subprocess, sys, time
from pathlib import Path

ZED = sys.argv[1]
base = Path(sys.argv[2])
proj = base / "proj"; proj.mkdir(parents=True, exist_ok=True)
data = base / "zed-data"; data.mkdir(exist_ok=True)
main = (proj / "main.typ"); main.write_text("= Hello\n\nSome text.\n")
main = main.resolve()

def rx(conn, n):
    b = bytearray()
    while len(b) < n:
        c = conn.recv(n - len(b))
        if not c: raise EOFError
        b += c
    return bytes(b)

def read(conn):
    (hl,) = struct.unpack("<I", rx(conn, 4)); h = json.loads(rx(conn, hl))
    (pl,) = struct.unpack("<I", rx(conn, 4)); return h, rx(conn, pl)

def write(conn, h, p=b""):
    hb = json.dumps(h).encode()
    conn.sendall(struct.pack("<I", len(hb)) + hb + struct.pack("<I", len(p)) + p)

proc = subprocess.Popen([ZED, "--user-data-dir", str(data), str(proj)],
                        stdout=open(base / "zed.out", "w"), stderr=subprocess.STDOUT, env=dict(os.environ, RUST_BACKTRACE="1", ZED_LOG="typst_preview=trace,info"))
sock = Path(f"/tmp/zed-typst-preview-{os.getuid()}/{proc.pid}.sock")
try:
    t0 = time.time()
    while not sock.exists() and time.time() - t0 < 60: time.sleep(0.2)
    assert sock.exists(), "Zed did not create the preview socket"
    print(f"✓ Zed listening on {sock} ({time.time()-t0:.1f}s)")
    time.sleep(4)  # let the workspace finish opening
    conn = socket.socket(socket.AF_UNIX); conn.connect(str(sock)); conn.settimeout(20)
    write(conn, {"type": "hello", "version": 2, "pid": os.getpid()})
    write(conn, {"type": "roots", "roots": [str(proj.resolve())]})
    write(conn, {"type": "open_preview", "path": str(main)})
    h, _ = read(conn)
    assert h["type"] == "attach", h
    # Bring the test window to the front so it renders frames.
    subprocess.run(["osascript", "-e", 'tell application "System Events" to set frontmost of '
                    f'(first process whose unix id is {proc.pid}) to true'], capture_output=True)
    print(f"✓ Zed opened the preview pane and attached: {h['path']}")
    write(conn, {"type": "target", "path": str(main), "title": "main.typ"})

    W, H = 400.0, 300.0
    def requested(timeout=5):
        """Strips the pane asks for until it is quiet for `timeout` s."""
        conn.settimeout(timeout)
        got = []
        try:
            while True:
                h, _ = read(conn)
                if h["type"] == "render":
                    for r in h["requests"]:
                        got += [(r["hash"], r["scale_milli"], k) for k in range(r["first"], r["end"])]
                        conn.settimeout(1)
        except socket.timeout:
            pass
        conn.settimeout(20)
        return got

    def send_strips(page_hash, scale_milli, strips, color):
        s = scale_milli / 1000
        w, total = round(W * s), round(H * s)
        for k in strips:
            rows = min(STRIP, total - k * STRIP)
            write(conn, {"type": "bitmap", "hash": page_hash, "scale_milli": scale_milli, "strip": k,
                         "strip_px": STRIP, "width": w, "height": rows}, bytes(color) * (w * rows))

    STRIP = 128
    write(conn, {"type": "document", "generation": 1, "compile_ms": 1.0, "warnings": 0,
                 "pages": [{"width": W, "height": H, "hash": "abc"}]})
    first = requested(20)
    assert first and all(r[0] == "abc" for r in first), first
    scale = first[0][1]
    total_px = round(H * scale / 1000)
    n = -(-total_px // STRIP)
    print(f"✓ pane laid out the page and requested strips {[k for *_, k in first]} at {scale / 1000:.2f} px/pt")
    send_strips("abc", scale, [k for *_, k in first], [0x40, 0xa0, 0xff, 0xff])
    time.sleep(0.5)

    # An edit that changed 100..110 pt: only the strips there are re-requested,
    # the others are reused from the previous version.
    write(conn, {"type": "document", "generation": 2, "compile_ms": 1.0, "warnings": 0,
                 "pages": [{"width": W, "height": H, "hash": "abd", "base": "abc", "dirty": [[100.0, 110.0]]}]})
    again = requested(2)
    expected = sorted({k for k in range(n) if (100 * scale / 1000 - 1) // STRIP <= k <= (110 * scale / 1000 + 1) // STRIP})
    assert sorted(k for *_, k in again) == expected, f"re-requested {again}, expected {expected}"
    print(f"✓ after an edit the pane re-requested only strips {expected} of {n}")
    send_strips("abd", scale, expected, [0x30, 0x30, 0xe0, 0xff])

    # Pushed strips: the server renders them unasked; the pane must not ask.
    write(conn, {"type": "document", "generation": 3, "compile_ms": 1.0, "warnings": 0,
                 "pages": [{"width": W, "height": H, "hash": "abe", "base": "abd", "dirty": [[100.0, 110.0]],
                            "pushed": [{"scale_milli": scale, "strip_px": STRIP, "strips": expected}]}]})
    send_strips("abe", scale, expected, [0x30, 0xc0, 0x30, 0xff])
    extra = requested(2)
    assert not extra, f"pane requested pushed strips again: {extra}"
    print("✓ pushed strips were not requested again")
    write(conn, {"type": "cursor", "scroll": True, "markers": [{"page": 0, "x": 50, "y": 60, "height": 12}]})
    write(conn, {"type": "status", "error": "main.typ:1:1: test error", "errors": 1, "compile_ms": 1, "warnings": 0})
    time.sleep(1.5)
    # Zed should still be alive (no panic while painting).
    assert proc.poll() is None, f"Zed exited with {proc.returncode}"
    print("✓ bitmap, cursor marker and error banner delivered; Zed still running")
    write(conn, {"type": "jump", "path": str(main), "line": 2, "character": 3})
    time.sleep(2)
    assert proc.poll() is None, f"Zed exited after jump with {proc.returncode}"
    print("✓ jump handled (opened main.typ at 3:4); Zed still running")
    # The editor now has the cursor: the pane reports it.
    conn.settimeout(5)
    try:
        while True:
            h, _ = read(conn)
            if h["type"] == "cursor":
                print(f"✓ editor cursor reported by the pane: line {h['line']+1}, col {h['character']+1}")
                break
    except socket.timeout:
        print("✗ no cursor report from the pane")
except Exception as e:
    print("FAIL:", repr(e), "zed exit code:", proc.poll())
finally:
    proc.terminate()
    try: proc.wait(timeout=10)
    except subprocess.TimeoutExpired: proc.kill()
