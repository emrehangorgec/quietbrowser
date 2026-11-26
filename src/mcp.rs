// `quietbrowser mcp`: a Model Context Protocol server over stdio. Tool names and arguments follow
// Playwright MCP, so agents that know it can use quietbrowser without new instructions.
// The browser window starts on the first tool call, not when the agent starts. Each MCP server
// (one per agent) works in a session of its own, so agents running at the same time get their own
// tabs in the same browser.
use std::io::{BufRead, Write};

use serde_json::{Value, json};

const VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

const INSTRUCTIONS: &str = "quietbrowser is a real browser (a WebView2 window on Windows, headless Chrome elsewhere) that never takes \
keyboard focus, so the user can keep typing while you browse. Start with browser_navigate, then act with the \
refs from the latest snapshot. Actions return only the lines that changed, or a new snapshot after a \
navigation, plus any console errors the action caused. Use browser_console_messages and \
browser_network_requests to debug a page. Dialogs are accepted automatically, popups open in the same tab and \
downloads are blocked. If a page needs a login or a CAPTCHA, call browser_login and ask the user to do it \
themselves; never type passwords or other secrets.";

fn tools() -> Value {
    let target = |extra: Value| {
        let mut props = json!({
            "element": { "type": "string", "description": "Human readable description of the element" },
            "ref": { "type": "string", "description": "Exact element ref from the page snapshot, like e12" }
        });
        if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        props
    };
    let tool = |name: &str, desc: &str, props: Value, required: &[&str]| {
        json!({ "name": name, "description": desc,
                "inputSchema": { "type": "object", "properties": props, "required": required } })
    };
    json!([
        tool("browser_navigate", "Navigate to a URL. Returns a snapshot of the page.",
            json!({ "url": { "type": "string", "description": "The URL to navigate to" } }), &["url"]),
        tool("browser_navigate_back", "Go back to the previous page.", json!({}), &[]),
        tool("browser_snapshot", "Capture an accessibility snapshot of the current page, with [ref=eN] handles. \
            Better than a screenshot for acting on the page.", json!({}), &[]),
        tool("browser_click", "Click an element with real mouse events. Returns what changed on the page.",
            target(json!({})), &["element", "ref"]),
        tool("browser_hover", "Hover the mouse over an element.", target(json!({})), &["element", "ref"]),
        tool("browser_type", "Replace the text of an editable element.",
            target(json!({
                "text": { "type": "string", "description": "Text to type into the element" },
                "submit": { "type": "boolean", "description": "Whether to press Enter after typing" }
            })), &["element", "ref", "text"]),
        tool("browser_press_key", "Press a key on the keyboard.",
            json!({ "key": { "type": "string", "description": "Key name like Enter, Tab, Escape, Backspace, ArrowDown, or a single character" } }),
            &["key"]),
        tool("browser_select_option", "Select options in a dropdown by value or label.",
            target(json!({ "values": { "type": "array", "items": { "type": "string" }, "description": "Values or labels to select" } })),
            &["element", "ref", "values"]),
        tool("browser_evaluate", "Evaluate a JavaScript expression or function on the page. Returns the JSON result.",
            json!({ "function": { "type": "string", "description": "() => { /* code */ } or an expression" } }), &["function"]),
        tool("browser_get_text", "Get the visible text of the page. Use it to read long pages.", json!({}), &[]),
        tool("browser_take_screenshot", "Take a screenshot of the page. You cannot act on it, use browser_snapshot for actions.",
            json!({ "filename": { "type": "string", "description": "File to save the PNG to. A temp file if omitted." } }), &[]),
        tool("browser_wait_for", "Wait for text to appear or disappear, or for a number of seconds.",
            json!({
                "time": { "type": "number", "description": "Seconds to wait" },
                "text": { "type": "string", "description": "Text to wait for" },
                "textGone": { "type": "string", "description": "Text to wait to disappear" }
            }), &[]),
        tool("browser_console_messages", "Console messages and uncaught errors of the page since it loaded.",
            json!({ "onlyErrors": { "type": "boolean", "description": "Only errors" } }), &[]),
        tool("browser_network_requests", "Requests of the page since it loaded, with method, status and type.",
            json!({ "onlyFailed": { "type": "boolean", "description": "Only failed requests and HTTP errors (4xx, 5xx)" } }), &[]),
        tool("browser_tabs", "List, open, select or close tabs. New tabs open in the background.",
            json!({
                "action": { "type": "string", "enum": ["list", "new", "select", "close"], "description": "What to do" },
                "index": { "type": "number", "description": "Tab index for select and close. Close without it closes the current tab." },
                "url": { "type": "string", "description": "URL to open in the new tab" }
            }), &["action"]),
        tool("browser_record", "Record the page: every action after start adds a frame. Stop saves an animated PNG \
            (it plays in browsers and on GitHub), for example to show a change in a pull request.",
            json!({
                "action": { "type": "string", "enum": ["start", "stop"], "description": "Start or stop recording" },
                "filename": { "type": "string", "description": "File to save the PNG to on stop. A temp file if omitted." }
            }), &["action"]),
        tool("browser_login", "Show a page to the user so they can log in or solve a CAPTCHA by hand. \
            The browser keeps the login for later. Tell the user what to do and wait for them.",
            json!({ "url": { "type": "string", "description": "The login page" } }), &["url"]),
        tool("browser_close", "Close this agent's tabs. The browser closes with the last agent.", json!({}), &[]),
    ])
}

fn str_arg(a: &Value, k: &str) -> Result<String, String> {
    a[k].as_str().map(String::from).ok_or_else(|| format!("missing argument {k}"))
}

fn text(t: String) -> Value {
    json!([{ "type": "text", "text": t }])
}

fn call_tool(name: &str, a: &Value) -> Result<Value, String> {
    let run = |cmd: &str, args: Vec<String>| crate::call(cmd, &args).map(text);
    match name {
        "browser_navigate" => run("open", vec![str_arg(a, "url")?]),
        "browser_navigate_back" => run("back", vec![]),
        "browser_snapshot" => run("snapshot", vec![]),
        "browser_click" => run("click", vec![str_arg(a, "ref")?]),
        "browser_hover" => run("hover", vec![str_arg(a, "ref")?]),
        "browser_type" => {
            let r = str_arg(a, "ref")?;
            let out = crate::call("fill", &[r, str_arg(a, "text")?])?;
            if a["submit"].as_bool() == Some(true) {
                let after = crate::call("press", &["Enter".into()])?;
                return Ok(text(format!("{out}\n\nAfter Enter:\n{after}")));
            }
            Ok(text(out))
        }
        "browser_press_key" => run("press", vec![str_arg(a, "key")?]),
        "browser_select_option" => {
            let mut args = vec![str_arg(a, "ref")?];
            let values = a["values"].as_array().ok_or("missing argument values")?;
            args.extend(values.iter().filter_map(|v| v.as_str().map(String::from)));
            run("select", args)
        }
        "browser_evaluate" => {
            let f = str_arg(a, "function")?;
            let t = f.trim_start();
            let js = if t.starts_with("function") || t.starts_with("async") || t.starts_with('(') && t.contains("=>") {
                format!("({f})()")
            } else {
                f
            };
            run("eval", vec![js])
        }
        "browser_get_text" => run("text", vec![]),
        "browser_take_screenshot" => {
            let args = a["filename"].as_str().map(|f| vec![f.to_string()]).unwrap_or_default();
            let path = crate::call("screenshot", &args)?;
            let data = std::fs::read(&path).map_err(|e| e.to_string())?;
            Ok(json!([
                { "type": "text", "text": format!("Saved to {path}") },
                { "type": "image", "data": crate::base64_encode(&data), "mimeType": "image/png" }
            ]))
        }
        "browser_wait_for" => {
            let mut out = Vec::new();
            if let Some(t) = a["time"].as_f64() {
                out.push(crate::wait(&[t.to_string()])?);
            }
            if let Some(w) = a["text"].as_str() {
                out.push(crate::wait(&[w.to_string()])?);
            }
            if let Some(g) = a["textGone"].as_str() {
                out.push(crate::wait(&[g.to_string(), "--gone".into()])?);
            }
            if out.is_empty() {
                return Err("give time, text or textGone".into());
            }
            Ok(text(out.join("\n\n")))
        }
        "browser_console_messages" => {
            run("console", if a["onlyErrors"].as_bool() == Some(true) { vec!["errors".into()] } else { vec![] })
        }
        "browser_network_requests" => {
            run("network", if a["onlyFailed"].as_bool() == Some(true) { vec!["failed".into()] } else { vec![] })
        }
        "browser_tabs" => {
            let index = a["index"].as_u64().map(|i| i.to_string());
            match str_arg(a, "action")?.as_str() {
                "list" => run("tabs", vec![]),
                "new" => {
                    let mut args = vec!["new".to_string()];
                    args.extend(a["url"].as_str().map(String::from));
                    run("tab", args)
                }
                "select" => run("tab", vec!["select".into(), index.ok_or("select needs an index")?]),
                "close" => run("tab", std::iter::once("close".to_string()).chain(index).collect()),
                other => Err(format!("unknown action {other}")),
            }
        }
        "browser_record" => {
            let mut args = vec![str_arg(a, "action")?];
            args.extend(a["filename"].as_str().map(String::from));
            run("record", args)
        }
        "browser_login" => run("login", vec![str_arg(a, "url")?]),
        "browser_close" => crate::call("quit", &[]).map(text),
        _ => Err(format!("unknown tool {name}")),
    }
}

fn handle(msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone(); // notifications get no answer
    let method = msg["method"].as_str().unwrap_or("");
    let result = match method {
        "initialize" => {
            let asked = msg["params"]["protocolVersion"].as_str().unwrap_or("");
            let version = if VERSIONS.contains(&asked) { asked } else { VERSIONS[VERSIONS.len() - 1] };
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "quietbrowser", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or("");
            let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
            crate::log(&format!("mcp {name}"));
            match call_tool(name, &args) {
                Ok(content) => json!({ "content": content }),
                Err(e) => json!({ "content": text(e), "isError": true }),
            }
        }
        _ => {
            return Some(json!({ "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") } }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

pub fn serve() -> i32 {
    if std::env::var("QUIETBROWSER_SESSION").is_err() {
        crate::set_session(format!("mcp-{}", std::process::id()));
    }
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch.iter().filter_map(handle).collect();
                (!replies.is_empty()).then(|| Value::Array(replies))
            }
            Ok(msg) => handle(&msg),
            Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": format!("parse error: {e}") } })),
        };
        if let Some(r) = reply {
            let _ = writeln!(out, "{r}");
            let _ = out.flush();
        }
    }
    // The agent is gone: close its tabs (and the browser, if no other agent uses it).
    let _ = crate::call("quit", &[]);
    0
}
