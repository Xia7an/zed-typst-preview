"""End to end: patched Zed + this extension + the real typst-livepreview server.

Copies the installed Typst extension (for the language definition) into an
isolated user data dir; reads Zed's log (~/Library/Logs/Zed/Zed.log).
Usage: python3 -I zed-patch/tests/e2e_test.py <zed binary> <server binary> <extension .wasm> <scratch dir>
"""
import json, os, re, shutil, subprocess, sys, time
from pathlib import Path

ZED, SERVER, WASM, base = sys.argv[1], sys.argv[2], sys.argv[3], Path(sys.argv[4])
shutil.rmtree(base, ignore_errors=True)
proj = base / "proj"; proj.mkdir(parents=True)
data = base / "zed-data"
main = proj / "main.typ"
main.write_text("#set page(width: 300pt, height: auto)\n= Hello Zed\n\nLive preview inside the editor.\n")

user_ext = Path.home() / "Library/Application Support/Zed/extensions"
inst = data / "extensions/installed"; inst.mkdir(parents=True)
shutil.copytree(user_ext / "installed/typst", inst / "typst")
(data / "extensions/work").mkdir(parents=True)
shutil.copytree(user_ext / "work/typst", data / "extensions/work/typst", symlinks=True)
ours = inst / "typst-livepreview"; ours.mkdir()
shutil.copy(WASM, ours / "extension.wasm")
(ours / "extension.toml").write_text('''id = "typst-livepreview"
name = "Typst Live Preview"
version = "0.1.0"
schema_version = 1
authors = []
description = "test"
repository = "https://github.com/inoyu/zed-typst-livepreview"

[lib]
kind = "Rust"
version = "0.7.0"

[language_servers.typst-livepreview]
name = "Typst Live Preview"
languages = ["Typst"]
''')
(data / "config").mkdir(parents=True)
(data / "config/settings.json").write_text(json.dumps({
    "session": {"trust_all_worktrees": True},
    "lsp": {"typst-livepreview": {"binary": {"path": SERVER}, "settings": {"auto_open": True}}},
}))

LOG = Path.home() / "Library/Logs/Zed/Zed.log"
log_start = LOG.stat().st_size if LOG.exists() else 0
out = open(base / "zed.out", "w")
env = dict(os.environ, ZED_LOG="typst_preview=debug,info", RUST_LOG="typst_livepreview=debug")
proc = subprocess.Popen([ZED, "--user-data-dir", str(data), str(proj), str(main)], stdout=out, stderr=subprocess.STDOUT, env=env)

def wait_log(pattern, timeout, start=0):
    t0 = time.time()
    while time.time() - t0 < timeout:
        with open(LOG, "rb") as f:
            f.seek(log_start); text = f.read().decode(errors="replace")
        m = [x for x in re.finditer(pattern, text)]
        if len(m) > start:
            return len(m), time.time() - t0
        if proc.poll() is not None:
            raise SystemExit(f"Zed exited ({proc.returncode})")
        time.sleep(0.1)
    raise SystemExit(f"timeout waiting for {pattern!r}")

try:
    _, t = wait_log(r"language server connected", 90)
    print(f"✓ typst-livepreview started by Zed and connected to the pane host ({t:.1f}s)")
    _, t = wait_log(r"document with 1 pages", 60)
    print(f"✓ preview pane opened (auto_open) and received the compiled document ({t:.1f}s)")
    n, t = wait_log(r"requesting 1 bitmaps", 30)
    print(f"✓ pane requested the page bitmap")
    time.sleep(1)
    docs, _ = wait_log(r"document with \d+ pages", 1)
    # Edit on disk → Zed reloads the buffer → didChange → recompile.
    main.write_text("#set page(width: 300pt, height: auto)\n= Hello Zed\n\nEdited!\n\n#pagebreak()\nSecond page.\n")
    _, t = wait_log(r"document with 2 pages", 30)
    print(f"✓ edit reached the pane as a new 2-page document ({t*1000:.0f} ms after writing the file)")
    assert proc.poll() is None
    print("✓ Zed still running")
finally:
    proc.terminate()
    try: proc.wait(timeout=10)
    except subprocess.TimeoutExpired: proc.kill()
    text = (base / "zed.out").read_text(errors="replace")
    if "panicked" in text:
        print(text[text.index("panicked") - 200:][:1500])
