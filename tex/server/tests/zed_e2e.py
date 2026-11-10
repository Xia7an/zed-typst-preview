"""End to end: patched Zed + the LaTeX extension + this extension + the real
tex-livepreview server, in an isolated Zed user data dir. Reads Zed's log
(~/Library/Logs/Zed/Zed.log).

Usage: python3 -I tex/server/tests/zed_e2e.py <zed binary> <server binary>
         <this extension dir (extension.toml + extension.wasm)>
         <LaTeX extension dir (unpacked from api.zed.dev)> <scratch dir>
"""
import json, os, re, shutil, subprocess, sys, time
from pathlib import Path

ZED, SERVER, EXT, LATEX, base = sys.argv[1], sys.argv[2], Path(sys.argv[3]), Path(sys.argv[4]), Path(sys.argv[5])
if base.exists():
    subprocess.run(["chmod", "-R", "u+w", str(base)])  # copies from the Nix store are read-only
    shutil.rmtree(base)
proj = base / "proj"; (proj / "sec").mkdir(parents=True)
data = base / "zed-data"
main = proj / "main.tex"
MAIN = "% !TEX program = pdflatex\n\\documentclass{article}\n\\begin{document}\n\\section{Hello Zed}\nLive preview inside the editor.\n\n\\input{sec/a}\n\\end{document}\n"
main.write_text(MAIN)
(proj / "sec/a.tex").write_text("Included text.\n")

inst = data / "extensions/installed"; inst.mkdir(parents=True)
(data / "extensions/work").mkdir(parents=True)
shutil.copytree(LATEX, inst / "latex")
shutil.copytree(EXT, inst / "tex-livepreview")
subprocess.run(["chmod", "-R", "u+w", str(inst)])
(data / "config").mkdir(parents=True)
(data / "config/settings.json").write_text(json.dumps({
    "session": {"trust_all_worktrees": True},
    # No texlab download in the test.
    "languages": {"LaTeX": {"language_servers": ["tex-livepreview"]}},
    "lsp": {"tex-livepreview": {"binary": {"path": SERVER}, "settings": {"auto_open": True}}},
}))

LOG = Path.home() / "Library/Logs/Zed/Zed.log"
log_start = LOG.stat().st_size if LOG.exists() else 0
out = open(base / "zed.out", "w")
env = dict(os.environ, ZED_LOG="external_preview=debug,info")
proc = subprocess.Popen([ZED, "--user-data-dir", str(data), str(proj), str(main)], stdout=out, stderr=subprocess.STDOUT, env=env)

def wait_log(pattern, timeout, start=0):
    t0 = time.time()
    while time.time() - t0 < timeout:
        with open(LOG, "rb") as f:
            f.seek(log_start); text = f.read().decode(errors="replace")
        m = list(re.finditer(pattern, text))
        if len(m) > start:
            return len(m), time.time() - t0
        if proc.poll() is not None:
            raise SystemExit(f"Zed exited ({proc.returncode})")
        time.sleep(0.1)
    raise SystemExit(f"timeout waiting for {pattern!r}")

try:
    _, t = wait_log(r"server \d+ is LaTeX \(latex", 90)
    print(f"✓ tex-livepreview started by Zed and announced itself to the pane host ({t:.1f}s)")
    _, t = wait_log(r"document with 1 pages", 60)
    print(f"✓ preview pane opened (auto_open) and received the compiled document ({t:.1f}s)")
    wait_log(r"requesting [1-9]\d* strip runs", 30)
    print("✓ pane requested the page strips")
    time.sleep(1)
    # Edit on disk → Zed reloads the buffer → didChange → recompile.
    main.write_text(MAIN.replace("\\input{sec/a}", "\\input{sec/a}\n\\newpage\nSecond page."))
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
