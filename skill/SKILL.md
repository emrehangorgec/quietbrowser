---
name: quietbrowser
description: "Browse web pages without stealing the user's keyboard focus. Use when you need to open a website, read a page, click through a site, fill a form, or check a local web app (localhost) in a real browser. Prefer this over launching Chrome or a headed Playwright browser, which pull the window in front of the user."
---

# quietbrowser

`quietbrowser` drives a real browser that never takes focus: a WebView2 window on Windows, and your installed Chrome without a window on macOS and Linux.
The user can keep typing while you work. Each command prints its result as plain text.

## Commands

```
quietbrowser open <url>              open a page, prints a snapshot
quietbrowser snapshot                the page as a tree with [ref=eN] handles
quietbrowser click <ref|selector>    click, prints what changed
quietbrowser fill <ref|selector> <text>
quietbrowser type <text>             type into the focused element
quietbrowser press <key>             Enter, Tab, Escape, Backspace, ArrowDown, a ...
quietbrowser hover <ref|selector>    move the mouse over an element
quietbrowser select <ref|selector> <value>...   choose options in a dropdown
quietbrowser text                    visible text of the page
quietbrowser eval <js>               run JavaScript, prints the JSON result
quietbrowser back | reload
quietbrowser wait <text>             wait until the text shows up (--gone: until it disappears)
quietbrowser console [errors]        console messages and uncaught errors since the page loaded
quietbrowser network [failed]        requests since the page loaded, with their status
quietbrowser tabs                    list your tabs
quietbrowser tab new [url] | tab select <i> | tab close [i]
quietbrowser dialogs                 alert/confirm/prompt dialogs answered so far
quietbrowser screenshot [file.png]   save a screenshot of the page, prints the path
quietbrowser record start            every command after this adds a frame
quietbrowser record stop [file.png]  save them as an animated PNG (plays on GitHub)
quietbrowser media dark|light|reset  emulate the page color scheme
quietbrowser login <url>             ask the user to sign in from the dashboard
quietbrowser quit                    close your tabs when you are done
```

## How to work

1. `open` the page. Read the snapshot it prints.
2. Act with the refs from the latest snapshot: `click e12`, `fill e7 "hello"`.
   A CSS selector works too: `click "#submit"`.
3. After an action you get only the changed lines (`### Changes`), or a new snapshot
   if the page navigated. Refs stay the same while the page stays loaded and reset
   after a navigation.
4. Use `text` to read long articles and `snapshot` when you need refs again.
5. When a page is slow, `wait "some text"` instead of sleeping.
6. When you test a web app, actions report new console errors under `### Console errors`.
   Run `console errors` and `network failed` to see what went wrong.
7. Run `quit` when the task is finished.

If other agents may use quietbrowser at the same time, put `--session <a name of your own>`
before every command (`quietbrowser --session api-fix open localhost:3000`). Each session has
its own tabs.

## Rules

* Dialogs are accepted automatically. Check `dialogs` if a page seems stuck.
* Downloads are blocked and popups open in the same tab.
* If a page needs a login or a CAPTCHA, run `quietbrowser login <url>`, tell the user
  what it printed, and wait until they return control. Take a fresh snapshot before continuing. Never type passwords, card
  numbers or other secrets. The profile is kept, so the login stays for later runs.
* Do not try to bring the window to the front. Leaving focus with the user is the point.
