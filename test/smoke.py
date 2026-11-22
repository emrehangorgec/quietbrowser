"""End to end smoke test: drives the quietbrowser CLI against local files.

Usage: python test/smoke.py path/to/quietbrowser [--engine chrome|webview2]
Uses port 9577 so it does not touch a browser you already have open.
On macOS it also checks after every command that the browser never became the front app.
"""

import os
import pathlib
import subprocess
import sys
import tempfile

PAGE = """<!doctype html><title>Smoke home</title>
<h1>Smoke test</h1>
<input id="name" aria-label="Name">
<button id="go" onclick="document.getElementById('out').textContent='Hello '+document.getElementById('name').value">Greet</button>
<p id="out">waiting</p>
<select id="color" aria-label="Color"><option>red</option><option>green</option></select>
<button onclick="alert('hi there')">Alert</button>
<a href="two.html">Second page</a>
"""
TWO = "<!doctype html><title>Smoke two</title><h1>Second page</h1><p>arrived</p>"


def front_app():
    """Name of the frontmost app on macOS, None elsewhere."""
    if sys.platform != "darwin":
        return None
    asn = subprocess.run(["lsappinfo", "front"], capture_output=True, text=True).stdout.strip()
    info = subprocess.run(["lsappinfo", "info", "-only", "name", asn], capture_output=True, text=True).stdout
    return info.split("=", 1)[-1].strip().strip('"') or "?"


BROWSER_NAMES = ("chrome", "chromium", "edge", "brave")


def main():
    exe = os.path.abspath(sys.argv[1])
    env = dict(os.environ, QUIETBROWSER_PORT="9577")
    if "--engine" in sys.argv:
        env["QUIETBROWSER_ENGINE"] = sys.argv[sys.argv.index("--engine") + 1]

    # plain files, so the test runs no server of its own
    site = pathlib.Path(tempfile.mkdtemp(prefix="quietbrowser-smoke-"))
    (site / "index.html").write_text(PAGE, encoding="utf-8")
    (site / "two.html").write_text(TWO, encoding="utf-8")
    base = (site / "index.html").as_uri()

    failures = 0

    def run(*args, expect=None, timeout=90):
        nonlocal failures
        p = subprocess.run([exe, *args], env=env, capture_output=True, text=True, timeout=timeout)
        out = p.stdout + p.stderr
        ok = p.returncode == 0 and (expect is None or expect in out)
        front = front_app()
        if front and any(n in front.lower() for n in BROWSER_NAMES):
            ok = False
            out += f"\nthe browser took focus: front app is {front}"
        print(("ok   " if ok else "FAIL ") + " ".join(args) + (f"  [front: {front}]" if front else ""))
        if not ok:
            failures += 1
            print("     exit", p.returncode, "expected", repr(expect))
            print("     " + out.strip().replace("\n", "\n     "))
        return out

    if sys.platform == "darwin":
        print("front app before:", front_app())
    try:
        run("open", base, expect="Smoke test", timeout=120)
        run("fill", "#name", "Ada", expect=None)
        run("click", "#go", expect="Hello Ada")
        run("eval", "document.getElementById('name').value", expect='"Ada"')
        run("select", "#color", "green")
        run("eval", "document.getElementById('color').value", expect='"green"')
        run("click", "button:nth-of-type(2)")
        run("dialogs", expect="hi there")
        run("text", expect="Smoke test")
        run("click", "a", expect="arrived")
        run("back", expect="Smoke home")
        run("press", "Tab")
        run("screenshot", expect=None)
    finally:
        run("quit")

    print("passed" if failures == 0 else f"{failures} failed")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
