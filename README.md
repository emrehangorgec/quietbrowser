# quietbrowser

**A browser for coding agents that never steals your focus.**

<p align="center">
  <img src="docs/demo.gif" alt="Screen recording: text is typed into a focused editor while an agent opens a sign up page in quietbrowser behind it, fills the email, ticks the box and submits. The focus line stays on the editor with 0 focus steals." width="900">
</p>

When Claude Code, Codex or another agent opens a browser, the window jumps in front of you and your next keystrokes land in the wrong place. On Windows, quietbrowser is a real browser window (Microsoft Edge WebView2) that shows up without taking focus. The agent drives it from the command line while you keep typing. Click the window when you want to use it yourself.

On macOS and Linux it drives the Chrome, Edge, Brave or Chromium you already have. Chrome raises its own window whenever it opens one, so there it runs without a window by default and cannot take focus. Set `QUIETBROWSER_HEADED=1` to see the window, for example to log in once; Chrome then comes to the front when it starts.

## Measured

On Windows. A test window stays in front while the browser is started and driven in the background. It records every moment another window becomes the foreground window and counts typed keys that never arrive. The test pages try hard to grab focus: `autofocus`, `window.focus()` every 50 ms, `alert`/`confirm`/`prompt`, `window.open`.

| Run | Commands | Focus taken | Keys lost |
|---|---|---|---|
| Microsoft Edge, new tabs over DevTools | 8 | 1 (on launch) | |
| quietbrowser with protections off (control) | 24 | 74 | |
| **quietbrowser** | **240** | **0** | |
| **quietbrowser while typing** | **120** | **0** | **0 of 871** |

Windows already blocks focus changes while you are typing, so most focus theft happens when you are idle: reading, or waiting for the agent. The idle runs are the ones that matter. Run it yourself with `test/FocusTest`.

## Commands

```
quietbrowser open <url>              open a page, print a snapshot
quietbrowser snapshot                the page as an accessibility tree with [ref=eN] handles
quietbrowser click <ref|selector>    click with real mouse events, print what changed
quietbrowser fill <ref|selector> <text>
quietbrowser type <text>             type into the focused element
quietbrowser press <key>             Enter, Tab, Escape, Backspace, ArrowDown, a ...
quietbrowser hover <ref|selector>
quietbrowser select <ref|selector> <value>...
quietbrowser text                    visible text of the page
quietbrowser eval <js>               run JavaScript, print the JSON result
quietbrowser back | reload
quietbrowser dialogs                 dialogs answered so far
quietbrowser screenshot [file.png]   save a screenshot, print the path
quietbrowser media dark|light|reset  emulate the color scheme
quietbrowser start | quit
quietbrowser install-skill           install the agent skill for Claude Code
quietbrowser mcp                     run as an MCP server over stdio
```

The first command starts the browser. A snapshot looks like this:

```
Page: Shop
URL: http://localhost:3000/signup
- heading "Sign up" [level=1] [ref=e1]
- textbox "Email" [ref=e2]
- checkbox "I agree" [ref=e3]
- button "Create account" [ref=e4]
```

After an action you get only what changed, which keeps agent context small:

```
> quietbrowser fill e2 dev@example.com
### Changes
+ - textbox "Email" [ref=e2]: dev@example.com
- - textbox "Email" [ref=e2]
```

## Install

With Node.js, one line adds it to your agent as an MCP server:

```
claude mcp add quietbrowser -- npx -y quietbrowser mcp
codex mcp add quietbrowser -- npx -y quietbrowser mcp
```

The tools use the same names and arguments as Playwright MCP (`browser_navigate`, `browser_snapshot`, `browser_click`, `browser_type`, ...), so agents already know how to use them. Then ask your agent to open a page. The window opens on the first command, behind whatever you are doing.

The npm package and the release downloads are Windows only for now. On macOS and Linux, build from source (see Build).

Without Node.js, download the exe (one file, about 0.5 MB, Windows 10 and 11 x64) from [Releases](https://github.com/emrehangorgec/quietbrowser/releases/latest) or in PowerShell, and add that path instead of `npx -y quietbrowser`:

```powershell
mkdir "$env:LOCALAPPDATA\quietbrowser\bin" -Force
curl.exe -L -o "$env:LOCALAPPDATA\quietbrowser\bin\quietbrowser.exe" https://github.com/emrehangorgec/quietbrowser/releases/latest/download/quietbrowser.exe
claude mcp add quietbrowser -- "$env:LOCALAPPDATA\quietbrowser\bin\quietbrowser.exe" mcp
```

## Command line and skill

Instead of MCP, agents can call the command line tool. This installs a skill in `~/.claude/skills/quietbrowser` that tells Claude Code when and how to use it:

```
npx -y quietbrowser install-skill
```

Other agents can read [`skill/SKILL.md`](skill/SKILL.md) directly.

## How it works

* The window is shown with `WS_EX_NOACTIVATE` and never calls `SetForegroundWindow`. A click by you activates it like a normal window, and it goes quiet again when you switch away.
* `alert`, `confirm` and `prompt` are answered in code, popups open in the same window and downloads are blocked, so no dialog appears.
* Snapshots come from the Chrome DevTools Protocol accessibility tree. Clicks and typing are sent as input events, so pages see a real user gesture.
* On macOS and Linux, Chrome is started with its own profile and driven over the same protocol through a WebSocket. Without `QUIETBROWSER_HEADED=1` it runs headless.
* One small executable (about 0.5 MB, written in Rust). `quietbrowser serve` runs the window; every other command is a client that starts it when needed and talks to it over a local HTTP port (`QUIETBROWSER_PORT`, default 9555).

## Build

Windows 10 or 11 with the WebView2 runtime (included in current Windows) and the Rust toolchain.

```
cargo build --release
target\release\quietbrowser.exe open example.com
```

The result is a single `quietbrowser.exe` with no other files to ship. The focus test in `test/FocusTest` needs the .NET SDK.

On macOS and Linux, install Rust and one Chromium based browser, then:

```
cargo build --release
target/release/quietbrowser open example.com
```

If the browser is not found, point `QUIETBROWSER_CHROME` at it. `python test/smoke.py target/release/quietbrowser` runs the end to end test on any system; CI runs it on Windows, macOS and Linux.

## Status

Early. The exe is not code signed yet, so a browser download may show a SmartScreen warning once. Not done yet: several agents in separate workspaces, a DevTools endpoint for Playwright, and packages for winget and Scoop.

Logins are kept in `%LOCALAPPDATA%\quietbrowser\profile` on Windows, `~/Library/Application Support/quietbrowser` on macOS and `~/.local/share/quietbrowser` on Linux. If a site needs a login or a CAPTCHA, the skill tells the agent to ask you to do it in the window. On macOS and Linux, run once with `QUIETBROWSER_HEADED=1` to get a window for that.

## License

[MIT](LICENSE)
