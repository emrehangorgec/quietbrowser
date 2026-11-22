---
name: quietbrowser
description: "Browse web pages on Windows without stealing the user's keyboard focus. Use when you need to open a website, read a page, click through a site, fill a form, or check a local web app (localhost) in a real browser. Prefer this over launching Chrome or a headed Playwright browser, which pull the window in front of the user."
---

# quietbrowser

`quietbrowser` drives a real browser window (Microsoft Edge WebView2) that never takes focus.
The user can keep typing while you work. Each command prints its result as plain text.

## Commands

```
quietbrowser open <url>              open a page, prints a snapshot
quietbrowser snapshot                the page as a tree with [ref=eN] handles
quietbrowser click <ref|selector>    click, prints what changed
quietbrowser fill <ref|selector> <text>
quietbrowser type <text>             type into the focused element
quietbrowser press <key>             Enter, Tab, Escape, Backspace, ArrowDown, a ...
quietbrowser text                    visible text of the page
quietbrowser eval <js>               run JavaScript, prints the JSON result
quietbrowser back | reload
quietbrowser dialogs                 alert/confirm/prompt dialogs answered so far
quietbrowser screenshot [file.png]   save a screenshot of the page, prints the path
quietbrowser media dark|light|reset  emulate the page color scheme
quietbrowser quit                    close the window when you are done
```

## How to work

1. `open` the page. Read the snapshot it prints.
2. Act with the refs from the latest snapshot: `click e12`, `fill e7 "hello"`.
   A CSS selector works too: `click "#submit"`.
3. After an action you get only the changed lines (`### Changes`), or a new snapshot
   if the page navigated. Refs stay the same while the page stays loaded and reset
   after a navigation.
4. Use `text` to read long articles and `snapshot` when you need refs again.
5. Run `quit` when the task is finished.

## Rules

* Dialogs are accepted automatically. Check `dialogs` if a page seems stuck.
* Downloads are blocked and popups open in the same window.
* If a page needs a login or a CAPTCHA, stop and ask the user to click the
  quietbrowser window and do it themselves. Never type passwords, card numbers or
  other secrets. The profile is kept, so the login stays for later runs.
* Do not try to bring the window to the front. Leaving focus with the user is the point.
