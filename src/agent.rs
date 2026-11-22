// The commands an agent runs: open, snapshot, click, fill, type, press, text, eval, back, dialogs, ...
// Snapshots are accessibility trees with [ref=eN] handles. Refs stay the same for an element
// while the page stays loaded. Actions answer with only the lines that changed.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::engine::{Engine, Nav, R};
use crate::log;

const MAX_NAME: usize = 100;
const MAX_LINES: usize = 1500;
const MAX_TEXT_BYTES: usize = 50_000;

const TRANSPARENT: &[&str] = &[
    "generic", "none", "InlineTextBox", "LineBreak", "RootWebArea", "WebArea", "Section", "LayoutTable",
    "LayoutTableRow", "LayoutTableCell", "Pre", "Ruby", "Iframe", "LabelText", "ListMarker",
];

#[derive(Default)]
pub struct Refs {
    by_node: HashMap<i64, String>,
    by_ref: HashMap<String, i64>,
    next: u32,
}

impl Refs {
    pub fn clear(&mut self) {
        self.by_node.clear();
        self.by_ref.clear();
        self.next = 0;
    }
    fn get(&mut self, node: i64) -> String {
        if let Some(r) = self.by_node.get(&node) {
            return r.clone();
        }
        self.next += 1;
        let r = format!("e{}", self.next);
        self.by_node.insert(node, r.clone());
        self.by_ref.insert(r.clone(), node);
        r
    }
}

/// A DevTools call sent while the page swaps renderer processes can stay unanswered, so every call has a timeout.
fn cdp(b: &dyn Engine, method: &str, params: Value) -> R<Value> {
    let start = Instant::now();
    let r = b.cdp_call(method, &params, Duration::from_secs(10));
    log(&format!("cdp {method} {} ms{}", start.elapsed().as_millis(), if r.is_err() { " failed" } else { "" }));
    r
}

/// Runs JavaScript in the page and returns the result as JSON text.
fn script(b: &dyn Engine, js: &str) -> R<String> {
    let r = cdp(
        b,
        "Runtime.evaluate",
        json!({ "expression": js, "returnByValue": true, "awaitPromise": true, "userGesture": true }),
    )?;
    if let Some(ex) = r.get("exceptionDetails") {
        let msg = ex["exception"]["description"].as_str().or(ex["text"].as_str()).unwrap_or("error");
        return Err(format!("page error: {}", msg.lines().next().unwrap_or("")));
    }
    let v = &r["result"];
    Ok(if v.get("value").is_some() { v["value"].to_string() } else { "null".into() })
}

pub fn init(b: &dyn Engine) -> R<()> {
    cdp(b, "DOM.enable", json!({}))?;
    cdp(b, "Accessibility.enable", json!({}))?;
    Ok(())
}

fn header(b: &dyn Engine) -> String {
    let (title, url) = b.title_url();
    format!("Page: {title}\nURL: {url}")
}

fn normalize(url: &str) -> String {
    if url.contains("://") || url.starts_with("about:") || url.starts_with("data:") {
        url.to_string()
    } else if ["localhost", "127.0.0.1", "[::1]"].iter().any(|h| {
        url.strip_prefix(h).is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/']))
    }) {
        format!("http://{url}") // local dev servers rarely have TLS
    } else {
        format!("https://{url}")
    }
}

pub fn run(b: &dyn Engine, cmd: &str, args: &[String]) -> R<String> {
    let short: Vec<String> = args
        .iter()
        .map(|a| if a.chars().count() > 60 { a.chars().take(60).collect::<String>() + "…" } else { a.clone() })
        .collect();
    log(&format!("run {cmd} {}", short.join(" ")));
    let arg = |i: usize, what: &str| args.get(i).cloned().ok_or_else(|| format!("{cmd}: missing {what}"));
    match cmd {
        "open" | "goto" => {
            let url = normalize(&arg(0, "url")?);
            navigate(b, Nav::Url(url))
        }
        "back" => navigate(b, Nav::Back),
        "reload" => navigate(b, Nav::Reload),
        "snapshot" => {
            let snap = snapshot(b)?;
            *b.shared().last_snapshot.lock().unwrap() = Some(snap.clone());
            Ok(format!("{}\n{snap}", header(b)))
        }
        "click" => {
            let t = arg(0, "ref")?;
            after(b, || click(b, &t))
        }
        "fill" => {
            let (t, text) = (arg(0, "ref")?, arg(1, "text")?);
            after(b, || fill(b, &t, &text))
        }
        "type" => {
            let text = arg(0, "text")?;
            after(b, || cdp(b, "Input.insertText", json!({ "text": text })).map(|_| ()))
        }
        "press" => {
            let key = arg(0, "key")?;
            after(b, || press(b, &key))
        }
        "hover" => {
            let t = arg(0, "ref")?;
            after(b, || hover(b, &t))
        }
        "select" => {
            let t = arg(0, "ref")?;
            arg(1, "value")?;
            let values = args[1..].to_vec();
            after(b, || select(b, &t, &values))
        }
        "text" => {
            let json = script(b, "document.body ? document.body.innerText : ''")?;
            let mut text: String = serde_json::from_str(&json).unwrap_or_default();
            if text.len() > MAX_TEXT_BYTES {
                let mut cut = MAX_TEXT_BYTES;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                text.push_str("\n[truncated]");
            }
            Ok(format!("{}\n{text}", header(b)))
        }
        "eval" => script(b, &arg(0, "script")?),
        "dialogs" => {
            let d = b.shared().dialogs.lock().unwrap();
            Ok(if d.is_empty() { "No dialogs.".into() } else { d.join("\n") })
        }
        "screenshot" => {
            let path = match args.first() {
                Some(p) => std::path::absolute(p).map_err(|e| e.to_string())?,
                None => std::env::temp_dir().join(format!("quietbrowser-{}.png", crate::timestamp())),
            };
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            let shot = cdp(b, "Page.captureScreenshot", json!({ "format": "png" }))?;
            let data = crate::base64_decode(shot["data"].as_str().unwrap_or_default())?;
            std::fs::write(&path, data).map_err(|e| e.to_string())?;
            Ok(path.display().to_string())
        }
        "media" => {
            let scheme = arg(0, "dark, light or reset")?;
            let features = match scheme.as_str() {
                "dark" | "light" => json!([{ "name": "prefers-color-scheme", "value": scheme }]),
                "reset" => json!([]),
                _ => return Err("media: use dark, light or reset".into()),
            };
            cdp(b, "Emulation.setEmulatedMedia", json!({ "features": features }))?;
            Ok(format!("prefers-color-scheme: {scheme}"))
        }
        _ => Err(format!("unknown command: {cmd}")),
    }
}

fn navigate(b: &dyn Engine, nav: Nav) -> R<String> {
    let s = b.shared();
    *s.nav_result.lock().unwrap() = None;
    b.go(&nav)?;
    let start = Instant::now();
    // history navigations served from the back/forward cache may not raise the events
    let finished = b.pump(Duration::from_secs(20), &mut || {
        s.nav_result.lock().unwrap().is_some() || (start.elapsed() >= Duration::from_secs(2) && !s.is_navigating())
    })?;
    if !finished && s.is_navigating() {
        return Ok(format!("Still loading after 20 s. Run snapshot later.\n{}", header(b)));
    }
    let snap = snapshot(b)?;
    *s.last_snapshot.lock().unwrap() = Some(snap.clone());
    let failed = *s.nav_result.lock().unwrap() == Some(false);
    let note = if failed { "\n(navigation failed)" } else { "" };
    Ok(format!("{}{note}\n{snap}", header(b)))
}

/// Runs an action, waits for the page to settle, then reports what changed.
fn after(b: &dyn Engine, action: impl FnOnce() -> R<()>) -> R<String> {
    let s = b.shared();
    let before = s.navigation_count();
    let cached = s.last_snapshot.lock().unwrap().clone();
    let previous = match cached {
        Some(snap) => snap,
        None => snapshot(b)?,
    };
    action()?;
    b.sleep(150)?;
    let loaded = b.pump(Duration::from_secs(20), &mut || !s.is_navigating())?;
    b.sleep(100)?;
    if !loaded {
        return Ok(format!(
            "Navigation started but the page is still loading after 20 s. Run snapshot later.\n{}",
            header(b)
        ));
    }
    let snap = snapshot(b)?;
    *s.last_snapshot.lock().unwrap() = Some(snap.clone());
    if s.navigation_count() != before {
        return Ok(format!("Navigated.\n{}\n{snap}", header(b)));
    }
    let d = diff(&previous, &snap);
    Ok(if d.is_empty() { "No visible changes.".into() } else { format!("### Changes\n{d}") })
}

fn call_on(b: &dyn Engine, object_id: &str, function: &str) -> R<()> {
    let res = cdp(
        b,
        "Runtime.callFunctionOn",
        json!({ "objectId": object_id, "functionDeclaration": function, "awaitPromise": true }),
    )?;
    if let Some(ex) = res.get("exceptionDetails") {
        return Err(format!("page error: {}", ex["text"].as_str().unwrap_or("")));
    }
    Ok(())
}

/// Clicks with real mouse events at the element's center, so the page sees a user gesture
/// (script clicks are skipped by the back button and blocked by some sites).
fn click(b: &dyn Engine, target: &str) -> R<()> {
    let id = resolve(b, target)?;
    let Some((x, y)) = center(b, &id)? else {
        return call_on(b, &id, "function(){this.click()}"); // not rendered as a box, fall back
    };
    cdp(b, "Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y }))?;
    for t in ["mousePressed", "mouseReleased"] {
        cdp(b, "Input.dispatchMouseEvent", json!({ "type": t, "x": x, "y": y, "button": "left", "clickCount": 1 }))?;
    }
    Ok(())
}

/// Scrolls the element into view and returns its center in page coordinates, if it has a box.
fn center(b: &dyn Engine, id: &str) -> R<Option<(f64, f64)>> {
    call_on(b, id, "function(){this.scrollIntoView({block:'center',inline:'center'})}")?;
    let quads = cdp(b, "DOM.getContentQuads", json!({ "objectId": id }))?;
    let Some(q) = quads["quads"].as_array().and_then(|a| a.first()).and_then(|q| q.as_array()) else {
        return Ok(None);
    };
    let q: Vec<f64> = q.iter().map(|v| v.as_f64().unwrap_or(0.0)).collect();
    if q.len() < 8 {
        return Ok(None);
    }
    Ok(Some(((q[0] + q[2] + q[4] + q[6]) / 4.0, (q[1] + q[3] + q[5] + q[7]) / 4.0)))
}

fn hover(b: &dyn Engine, target: &str) -> R<()> {
    let id = resolve(b, target)?;
    let (x, y) = center(b, &id)?.ok_or_else(|| format!("{target} has no box to hover"))?;
    cdp(b, "Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y })).map(|_| ())
}

/// Selects options of a <select> by value or label and fires input and change events.
fn select(b: &dyn Engine, target: &str, values: &[String]) -> R<()> {
    let id = resolve(b, target)?;
    let res = cdp(
        b,
        "Runtime.callFunctionOn",
        json!({
            "objectId": id,
            "functionDeclaration": "function(values){if(!this.options)throw new Error('not a select element');\
                let n=0;for(const o of this.options){o.selected=values.includes(o.value)||values.includes(o.label.trim());if(o.selected)n++;}\
                if(!n)throw new Error('no option matches '+values.join(', '));\
                this.dispatchEvent(new Event('input',{bubbles:true}));this.dispatchEvent(new Event('change',{bubbles:true}));}",
            "arguments": [{ "value": values }],
        }),
    )?;
    if let Some(ex) = res.get("exceptionDetails") {
        let msg = ex["exception"]["description"].as_str().or(ex["text"].as_str()).unwrap_or("");
        return Err(format!("page error: {}", msg.lines().next().unwrap_or("")));
    }
    Ok(())
}

/// Focuses the field, selects what is in it and types the new text over it.
fn fill(b: &dyn Engine, target: &str, text: &str) -> R<()> {
    let id = resolve(b, target)?;
    call_on(
        b,
        &id,
        "function(){this.scrollIntoView({block:'center'});this.focus();\
         if(typeof this.select==='function'){this.select();}else{const r=document.createRange();r.selectNodeContents(this);\
         const s=getSelection();s.removeAllRanges();s.addRange(r);}}",
    )?;
    if text.is_empty() {
        press(b, "Backspace")
    } else {
        cdp(b, "Input.insertText", json!({ "text": text })).map(|_| ())
    }
}

/// A ref from the last snapshot (e12), or a CSS selector.
fn resolve(b: &dyn Engine, target: &str) -> R<String> {
    let node = b.shared().refs.lock().unwrap().by_ref.get(target).copied();
    if let Some(node) = node {
        cdp(b, "DOM.getDocument", json!({ "depth": 0 }))?;
        let res = cdp(b, "DOM.resolveNode", json!({ "backendNodeId": node }))?;
        return res["object"]["objectId"].as_str().map(String::from).ok_or_else(|| format!("{target} is gone, take a new snapshot"));
    }
    let is_ref = target.len() > 1 && target.starts_with('e') && target[1..].bytes().all(|c| c.is_ascii_digit());
    if is_ref {
        return Err(format!("unknown ref {target}, take a new snapshot"));
    }
    let expr = format!("document.querySelector({})", Value::from(target));
    let res = cdp(b, "Runtime.evaluate", json!({ "expression": expr }))?;
    res["result"]["objectId"].as_str().map(String::from).ok_or_else(|| format!("no element matches {target}"))
}

fn press(b: &dyn Engine, key: &str) -> R<()> {
    let (code, text, vk): (String, String, u32) = match key {
        "Enter" => ("Enter".into(), "\r".into(), 13),
        "Tab" => ("Tab".into(), "".into(), 9),
        "Escape" => ("Escape".into(), "".into(), 27),
        "Backspace" => ("Backspace".into(), "".into(), 8),
        "ArrowDown" => ("ArrowDown".into(), "".into(), 40),
        "ArrowUp" => ("ArrowUp".into(), "".into(), 38),
        "ArrowLeft" => ("ArrowLeft".into(), "".into(), 37),
        "ArrowRight" => ("ArrowRight".into(), "".into(), 39),
        k if k.chars().count() == 1 => {
            let up = k.to_uppercase();
            (format!("Key{up}"), k.into(), up.chars().next().unwrap() as u32)
        }
        _ => return Err(format!("unsupported key {key}")),
    };
    let down = if text.is_empty() { "rawKeyDown" } else { "keyDown" };
    cdp(b, "Input.dispatchKeyEvent",
        json!({ "type": down, "key": key, "code": code, "text": text, "windowsVirtualKeyCode": vk }))?;
    cdp(b, "Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk }))?;
    Ok(())
}

fn snapshot(b: &dyn Engine) -> R<String> {
    let tree = cdp(b, "Accessibility.getFullAXTree", json!({}))?;
    let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
    if nodes.is_empty() {
        return Ok("(empty page)".into());
    }
    let by_id: HashMap<&str, &Value> = nodes.iter().filter_map(|n| Some((n["nodeId"].as_str()?, n))).collect();
    let mut lines = Vec::new();
    let mut refs = b.shared().refs.lock().unwrap();
    render(&nodes[0], 0, "", &by_id, &mut lines, &mut refs);
    if lines.len() > MAX_LINES {
        let more = lines.len() - MAX_LINES;
        lines.truncate(MAX_LINES);
        lines.push(format!("[{more} more lines]"));
    }
    Ok(if lines.is_empty() { "(empty page)".into() } else { lines.join("\n") })
}

fn prop_str<'a>(n: &'a Value, prop: &str) -> &'a str {
    n[prop]["value"].as_str().unwrap_or("")
}

fn clip(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > MAX_NAME {
        s.chars().take(MAX_NAME).collect::<String>() + "…"
    } else {
        s
    }
}

fn render(n: &Value, depth: usize, parent_name: &str, by_id: &HashMap<&str, &Value>, lines: &mut Vec<String>, refs: &mut Refs) {
    if lines.len() > MAX_LINES + 1 {
        return;
    }
    let ignored = n["ignored"].as_bool().unwrap_or(false);
    let role = prop_str(n, "role");
    let name = clip(prop_str(n, "name"));
    let mut child_depth = depth;
    let mut parent_name = parent_name.to_string();
    let indent = "  ".repeat(depth);

    if !ignored {
        if role == "StaticText" {
            if !name.is_empty() && !parent_name.contains(&name) {
                lines.push(format!("{indent}- text: {name}"));
            }
            return;
        }
        if !TRANSPARENT.contains(&role) {
            let mut line = format!("{indent}- {role}");
            if !name.is_empty() {
                line.push_str(&format!(" \"{}\"", name.replace('"', "\\\"")));
            }
            properties(n, role, &mut line);
            if let Some(node) = n["backendDOMNodeId"].as_i64() {
                line.push_str(&format!(" [ref={}]", refs.get(node)));
            }
            let value = clip(prop_str(n, "value"));
            if !value.is_empty() {
                line.push_str(&format!(": {value}"));
            }
            lines.push(line);
            // the text inside an input repeats its value
            if matches!(role, "textbox" | "searchbox" | "combobox" | "spinbutton") {
                return;
            }
            child_depth = depth + 1;
            parent_name = name;
        }
    }
    if let Some(kids) = n["childIds"].as_array() {
        for k in kids {
            if let Some(child) = k.as_str().and_then(|id| by_id.get(id)) {
                render(child, child_depth, &parent_name, by_id, lines, refs);
            }
        }
    }
}

fn properties(n: &Value, role: &str, line: &mut String) {
    let Some(props) = n["properties"].as_array() else { return };
    for p in props {
        let val = &p["value"]["value"];
        if val.is_null() {
            continue;
        }
        match p["name"].as_str().unwrap_or("") {
            "level" if role == "heading" => {
                let level = val.as_i64().map(|l| l.to_string()).unwrap_or_else(|| val.to_string());
                line.push_str(&format!(" [level={level}]"));
            }
            "checked" if !(val == "false" || val == false) => line.push_str(" [checked]"),
            "disabled" if val == true => line.push_str(" [disabled]"),
            "expanded" => line.push_str(if val == true { " [expanded]" } else { " [collapsed]" }),
            "selected" if val == true => line.push_str(" [selected]"),
            _ => {}
        }
    }
}

/// Line diff (LCS). Returns "- old" / "+ new" lines, empty when nothing changed.
pub fn diff(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.split('\n').collect();
    let b: Vec<&str> = after.split('\n').collect();
    if a.len() * b.len() > 4_000_000 {
        return if before == after { String::new() } else { format!("(page changed a lot)\n{after}") };
    }
    let w = b.len() + 1;
    let mut lcs = vec![0u32; (a.len() + 1) * w];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i * w + j] = if a[i] == b[j] {
                lcs[(i + 1) * w + j + 1] + 1
            } else {
                lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut x, mut y) = (0, 0);
    while x < a.len() || y < b.len() {
        if x < a.len() && y < b.len() && a[x] == b[y] {
            x += 1;
            y += 1;
        } else if y < b.len() && (x == a.len() || lcs[x * w + y + 1] >= lcs[(x + 1) * w + y]) {
            out.push(format!("+ {}", b[y]));
            y += 1;
        } else {
            out.push(format!("- {}", a[x]));
            x += 1;
        }
    }
    out.join("\n")
}
