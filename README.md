# quietbrowser

**A browser for coding agents on Windows that never steals your focus.**

<p align="center">
  <img src="docs/hero.svg" alt="You keep typing in your editor while the agent opens a sign-up page, fills the email, ticks the box and submits it in a quietbrowser window behind it" width="860">
</p>

When Claude Code, Codex or another agent opens a browser, the window jumps in front of you and your next keystrokes land in the wrong place. quietbrowser is a real browser window (Microsoft Edge WebView2) that shows up without taking focus. The agent drives it from the command line while you keep typing. Click the window when you want to use it yourself.

## Measured

A test window stays in front while the browser is started and driven in the background. It records every moment another window becomes the foreground window and counts typed keys that never arrive. The test pages try hard to grab focus: `autofocus`, `window.focus()` every 50 ms, `alert`/`confirm`/`prompt`, `window.open`.

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
quietbrowser text                    visible text of the page
quietbrowser eval <js>               run JavaScript, print the JSON result
quietbrowser back | reload
quietbrowser dialogs                 dialogs answered so far
quietbrowser screenshot [file.png]   save a screenshot, print the path
quietbrowser media dark|light|reset  emulate the color scheme
quietbrowser start | quit
quietbrowser install-skill           install the agent skill for Claude Code
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

One file, about 0.5 MB, for Windows 10 and 11 (x64). Download it from [Releases](https://github.com/emrehangorgec/quietbrowser/releases/latest) or in PowerShell:

```powershell
mkdir "$env:LOCALAPPDATA\quietbrowser\bin" -Force
curl.exe -L -o "$env:LOCALAPPDATA\quietbrowser\bin\quietbrowser.exe" https://github.com/emrehangorgec/quietbrowser/releases/latest/download/quietbrowser.exe
```

## Use it from your agent

```powershell
& "$env:LOCALAPPDATA\quietbrowser\bin\quietbrowser.exe" install-skill
```

This installs a skill in `~/.claude/skills/quietbrowser` that tells Claude Code when and how to use the browser, with the path to the exe. Then ask your agent to open a page. Other agents can read [`skill/SKILL.md`](skill/SKILL.md) directly.

## How it works

* The window is shown with `WS_EX_NOACTIVATE` and never calls `SetForegroundWindow`. A click by you activates it like a normal window, and it goes quiet again when you switch away.
* `alert`, `confirm` and `prompt` are answered in code, popups open in the same window and downloads are blocked, so no dialog appears.
* Snapshots come from the Chrome DevTools Protocol accessibility tree. Clicks and typing are sent as input events, so pages see a real user gesture.
* One small executable (about 0.5 MB, written in Rust). `quietbrowser serve` runs the window; every other command is a client that starts it when needed and talks to it over a local HTTP port (`QUIETBROWSER_PORT`, default 9555).

## Build

Windows 10 or 11 with the WebView2 runtime (included in current Windows) and the Rust toolchain.

```
cargo build --release
target\release\quietbrowser.exe open example.com
```

The result is a single `quietbrowser.exe` with no other files to ship. The focus test in `test/FocusTest` needs the .NET SDK.

## Status

Early. The exe is not code signed yet, so a browser download may show a SmartScreen warning once. Not done yet: several agents in separate workspaces, a DevTools endpoint for Playwright, and packages for winget and Scoop.

Logins are kept in `%LOCALAPPDATA%\quietbrowser\profile`. If a site needs a login or a CAPTCHA, the skill tells the agent to ask you to do it in the window.

## License

[MIT](LICENSE)
