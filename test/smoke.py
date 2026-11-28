"""End to end smoke test: drives the quietbrowser CLI against local files.

Usage: python test/smoke.py path/to/quietbrowser [--engine chrome|webview2] [--http]
Uses port 9577 (override with QUIETBROWSER_TEST_PORT), separate from the default browser.
Use --http if the test browser is configured to block file URLs.
After every command it checks that the browser did not take focus: on macOS the front app must
not be a browser, on Windows the foreground window must not belong to quietbrowser or Chrome.
"""

import functools
import http.server
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
import urllib.error
import urllib.request

PAGE = """<!doctype html><title>Smoke home</title>
<h1>Smoke test</h1>
<input id="name" aria-label="Name">
<button id="go" onclick="document.getElementById('out').textContent='Hello '+document.getElementById('name').value">Greet</button>
<p id="out">waiting</p>
<select id="color" aria-label="Color"><option>red</option><option>green</option></select>
<button onclick="alert('hi there')">Alert</button>
<a href="two.html">Second page</a>
<button id="fail" onclick="console.error('smoke broke')">Fail</button>
<button id="later" onclick="setTimeout(()=>{document.getElementById('slow').textContent='later done'},800)">Later</button>
<p id="slow">not yet</p>
<img src="missing.png" alt="">
<script>console.log('smoke loaded')</script>
"""
TWO = "<!doctype html><title>Smoke two</title><h1>Second page</h1><p>arrived</p>"


def front_app():
    """Name of the frontmost app on macOS, None elsewhere."""
    if sys.platform != "darwin":
        return None
    asn = subprocess.run(["lsappinfo", "front"], capture_output=True, text=True).stdout.strip()
    info = subprocess.run(["lsappinfo", "info", "-only", "name", asn], capture_output=True, text=True).stdout
    return info.split("=", 1)[-1].strip().strip('"') or "?"


def foreground_program():
    """File name of the program that owns the foreground window on Windows, None elsewhere."""
    if sys.platform != "win32":
        return None
    import ctypes
    from ctypes import wintypes

    user32, kernel32 = ctypes.windll.user32, ctypes.windll.kernel32
    hwnd = user32.GetForegroundWindow()
    if not hwnd:
        return "(none)"
    pid = wintypes.DWORD()
    user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
    process = kernel32.OpenProcess(0x1000, False, pid.value)  # PROCESS_QUERY_LIMITED_INFORMATION
    if not process:
        return "?"
    buf = ctypes.create_unicode_buffer(1024)
    size = wintypes.DWORD(1024)
    kernel32.QueryFullProcessImageNameW(process, 0, buf, ctypes.byref(size))
    kernel32.CloseHandle(process)
    return os.path.basename(buf.value).lower()


BROWSER_NAMES = ("chrome", "chromium", "edge", "brave")
BROWSER_PROGRAMS = ("quietbrowser.exe", "msedgewebview2.exe", "chrome.exe", "msedge.exe")


def main():
    exe = os.path.abspath(sys.argv[1])
    env = dict(os.environ, QUIETBROWSER_PORT=os.environ.get("QUIETBROWSER_TEST_PORT", "9577"))
    env.pop("QUIETBROWSER_SESSION", None)
    if "--engine" in sys.argv:
        env["QUIETBROWSER_ENGINE"] = sys.argv[sys.argv.index("--engine") + 1]

    # File URLs avoid macOS local network prompts. Use HTTP when file URLs are blocked.
    site = pathlib.Path(tempfile.mkdtemp(prefix="quietbrowser-smoke-"))
    (site / "index.html").write_text(PAGE, encoding="utf-8")
    (site / "two.html").write_text(TWO, encoding="utf-8")
    fixture = None
    if "--http" in sys.argv:
        handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=site)
        fixture = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=fixture.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{fixture.server_port}/index.html"
        two = f"http://127.0.0.1:{fixture.server_port}/two.html"
    else:
        base = (site / "index.html").as_uri()
        two = (site / "two.html").as_uri()

    failures = 0

    def run(*args, expect=None, absent=None, code=0, timeout=90):
        nonlocal failures
        p = subprocess.run([exe, *args], env=env, capture_output=True, text=True, timeout=timeout)
        out = p.stdout + p.stderr
        ok = p.returncode == code and (expect is None or expect in out) and (absent is None or absent not in out)
        front = front_app()
        if front and any(n in front.lower() for n in BROWSER_NAMES):
            ok = False
            out += f"\nthe browser took focus: front app is {front}"
        fg = foreground_program()
        if fg in BROWSER_PROGRAMS:
            ok = False
            out += f"\nthe browser took focus: the foreground window belongs to {fg}"
        print(("ok   " if ok else "FAIL ") + " ".join(args) + (f"  [front: {front}]" if front else ""))
        if not ok:
            failures += 1
            print("     exit", p.returncode, "expected", repr(expect), "absent", repr(absent))
            print("     " + out.strip().replace("\n", "\n     "))
        return out

    def check(name, ok, detail=""):
        nonlocal failures
        print(("ok   " if ok else "FAIL ") + name)
        if not ok:
            failures += 1
            print("     " + detail)

    def ui_request(path, data=None, headers=None):
        request = urllib.request.Request(
            f"http://127.0.0.1:{env['QUIETBROWSER_PORT']}/ui/{path}",
            data=json.dumps(data).encode() if data is not None else None,
            headers={"X-QuietBrowser-UI": "1"} if headers is None else headers,
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read()

    def ui_status(path, data=None, headers=None):
        try:
            ui_request(path, data, headers)
            return 200
        except urllib.error.HTTPError as error:
            return error.code

    if sys.platform == "darwin":
        print("front app before:", front_app())
    if sys.platform == "win32":
        print("foreground before:", foreground_program())
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

        # console and network
        run("console", expect="smoke loaded")
        run("network", expect="missing.png")
        run("network", "failed", expect="missing.png", absent="index.html")
        run("click", "#fail", expect="smoke broke")
        run("console", "errors", expect="smoke broke", absent="smoke loaded")

        # wait
        run("click", "#later")
        run("wait", "later done", expect="Found: later done")
        run("wait", "later done", "--gone", "--timeout", "1", expect="still there", code=1)
        run("wait", "0.2")

        # recording
        rec = site / "rec.png"
        run("record", "start", expect="Recording")
        run("fill", "#name", "Grace")
        run("click", "#go", expect="Hello Grace")
        run("record", "stop", str(rec), expect="frames")
        data = rec.read_bytes() if rec.exists() else b""
        check("record is an animated PNG", data.startswith(b"\x89PNG") and b"acTL" in data and b"fdAT" in data,
              f"{len(data)} bytes")

        # tabs
        run("tab", "new", two, expect="Second page")
        run("tabs", expect="(current) [Smoke two]")
        run("tab", "select", "0", expect="Smoke home")
        run("eval", "document.title", expect='"Smoke home"')
        run("tab", "close", "1", expect="Open tabs")
        run("tabs", absent="Smoke two")

        # sessions: another agent gets its own tab, and this one keeps its page
        run("--session", "other", "open", two, expect="Second page")
        run("eval", "document.title", expect='"Smoke home"')
        run("--session", "other", "eval", "document.title", expect='"Smoke two"')
        run("ui", "--no-open", expect="/ui/")
        check("dashboard loads locally", b"<title>quietbrowser</title>" in ui_request("", headers={}))
        check("dashboard data requires its request header", ui_status("state", headers={}) == 403)
        check("dashboard rejects foreign origins", ui_status("state", headers={
            "X-QuietBrowser-UI": "1", "Origin": "https://example.com"
        }) == 403)
        state = json.loads(ui_request("state"))
        listed = {tab["agent"]: tab for tab in state["tabs"]}
        check("dashboard shows both sessions", set(listed) == {"cli", "other"}, str(listed))
        current = listed["cli"]["id"]

        def action(name, **fields):
            return json.loads(ui_request("action", dict(instance=state["instance"], tab=current, action=name, **fields)))

        position = "(r => [r.left + r.width / 2, r.top + r.height / 2])(document.querySelector('#name').getBoundingClientRect()).map((v, i) => v / [innerWidth, innerHeight][i])"
        # The built-in eval output is JSON, so the click works at different viewport sizes.
        point = json.loads(run("eval", position))
        check("dashboard captures a preview", action("preview")["image"].startswith("/9j/"))
        check("dashboard input requires takeover", ui_status("action", dict(
            instance=state["instance"], tab=current, action="text", text="should not be sent"
        )) == 409)
        action("take")
        run("eval", "document.title", expect="reserved for the user", code=1)
        run("tab", "close", "0", expect="reserved for the user", code=1)
        run("--session", "other", "eval", "document.title", expect='"Smoke two"')
        action("click", x=point[0], y=point[1])
        action("key", key="a")
        action("text", text="typed by the user")
        action("return")
        run("eval", "document.querySelector('#name').value", expect='"typed by the user"')

        run("login", base, expect="Take control")
        waiting = json.loads(ui_request("state"))["tabs"]
        check("sign-in is waiting on the same tab", any(
            tab["id"] == current and tab["status"] == "waiting" for tab in waiting
        ), str(waiting))
        run("--session", "other", "eval", "document.title", expect='"Smoke two"')
        action("take")
        action("return")
        run("--session", "other", "quit")
        run("eval", "document.title", expect='"Smoke home"')

        run("click", "a", expect="arrived")
        run("network", expect="two.html", absent="index.html")
        run("back", expect="Smoke home")
        run("press", "Tab")
        run("screenshot", expect=None)

        # the MCP server lists the new tools
        msgs = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        ]
        p = subprocess.run([exe, "mcp"], env=env, input="\n".join(json.dumps(m) for m in msgs) + "\n",
                           capture_output=True, text=True, timeout=60)
        names = []
        for line in p.stdout.splitlines():
            msg = json.loads(line)
            if msg.get("id") == 2:
                names = [t["name"] for t in msg["result"]["tools"]]
        wanted = ["browser_console_messages", "browser_network_requests", "browser_tabs", "browser_record", "browser_login"]
        check("mcp tools/list", all(n in names for n in wanted), f"tools: {names}")
        # the MCP server closed its (unused) session on exit; this session must still be there
        run("eval", "document.title", expect='"Smoke home"')
    finally:
        run("quit", "--all")
        if fixture:
            fixture.shutdown()
            fixture.server_close()

    print("passed" if failures == 0 else f"{failures} failed")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
