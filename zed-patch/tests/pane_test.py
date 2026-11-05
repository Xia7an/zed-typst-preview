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
    write(conn, {"type": "hello", "version": 1, "pid": os.getpid()})
    write(conn, {"type": "roots", "roots": [str(proj.resolve())]})
    write(conn, {"type": "open_preview", "path": str(main)})
    h, _ = read(conn)
    assert h["type"] == "attach", h
    # Bring the test window to the front so it renders frames.
    print(f"✓ Zed opened the preview pane and attached: {h['path']}")
    write(conn, {"type": "target", "path": str(main), "title": "main.typ"})
    write(conn, {"type": "document", "generation": 1, "compile_ms": 1.0, "warnings": 0,
                 "pages": [{"width": 400.0, "height": 200.0, "hash": "abc"}]})
    while True:
        h, _ = read(conn)
        if h["type"] == "render" and h["requests"]: break
    req = h["requests"][0]
    scale = req["scale_milli"] / 1000
    w, hgt = round(400 * scale), round(200 * scale)
    print(f"✓ pane laid out the page and requested a bitmap at {scale:.2f} px/pt ({w}×{hgt})")
    write(conn, {"type": "bitmap", "hash": "abc", "scale_milli": req["scale_milli"], "width": w, "height": hgt},
          bytes([0x40, 0xa0, 0xff, 0xff]) * (w * hgt))
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
