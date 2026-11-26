# quietbrowser

**One browser for all your coding agents.** Each agent gets its own tabs, they share your logins, and none of them ever steals your focus.

![Three agents drive one quietbrowser window at the same time, each in its own named tab, while text is typed into a focused editor with 0 focus steals](https://raw.githubusercontent.com/emrehangorgec/quietbrowser/master/docs/demo.gif)

On Windows, a real browser window (Microsoft Edge WebView2) that opens behind whatever you are doing and never takes keyboard focus. On macOS and Linux, your installed Chrome, Edge, Brave or Chromium, run headless so it cannot take focus. Your agents drive it over MCP or from the command line while you keep typing. On Windows the tab strip shows which agent owns each tab and which one is working.

## MCP

The tools use the same names and arguments as Playwright MCP (`browser_navigate`, `browser_snapshot`, `browser_click`, `browser_tabs`, `browser_console_messages`, ...). Agents that run at the same time each get their own tabs in the same browser, and share its logins.

```
claude mcp add quietbrowser -- npx -y quietbrowser mcp
codex mcp add quietbrowser -- npx -y quietbrowser mcp
```

## CLI

```
npx -y quietbrowser open example.com
npx -y quietbrowser click e3
npx -y quietbrowser install-skill
```

Windows x64, macOS (Intel and Apple Silicon) and Linux x64. The package ships a small binary for each, nothing is downloaded at install time.

Docs, focus measurements and source: https://github.com/emrehangorgec/quietbrowser
