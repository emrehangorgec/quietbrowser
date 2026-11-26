// What the agent commands need from a browser: DevTools calls, navigation, tabs and a way to wait.
// On Windows this is a WebView2 window; elsewhere it is Chrome (or another Chromium) over CDP.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub type R<T> = Result<T, String>;

pub enum Nav {
    Url(String),
    Back,
    Reload,
}

const MAX_CONSOLE: usize = 500;
const MAX_REQUESTS: usize = 1000;
const MAX_MESSAGE: usize = 500;

pub struct ConsoleEntry {
    pub seq: u64,
    /// Which page load of the tab it belongs to (see `Shared::navigation_count`).
    pub load: u32,
    pub level: String,
    pub text: String,
}

pub struct Request {
    pub id: String,
    pub load: u32,
    pub at: Instant,
    pub method: String,
    pub url: String,
    pub kind: String,
    pub status: Option<i64>,
    pub failed: Option<String>,
}

/// State of one tab, written by the engine's event handlers and read by the commands.
#[derive(Default)]
pub struct Shared {
    pub navigating: AtomicBool,
    pub navigations: AtomicU32,
    pub nav_result: Mutex<Option<bool>>,
    pub dialogs: Mutex<Vec<String>>,
    pub refs: Mutex<crate::agent::Refs>,
    pub last_snapshot: Mutex<Option<String>>,
    pub console: Mutex<VecDeque<ConsoleEntry>>,
    pub requests: Mutex<VecDeque<Request>>,
    next_seq: AtomicU64,
}

impl Shared {
    /// A new document started loading: refs from the old one are no longer valid.
    pub fn nav_started(&self) {
        self.navigating.store(true, Ordering::SeqCst);
        let old = self.navigations.fetch_add(1, Ordering::SeqCst);
        self.refs.lock().unwrap().clear();
        *self.last_snapshot.lock().unwrap() = None;
        // The request for the new document can be reported just before the load starts. It has no
        // answer yet then, unlike the request of the document that is being left.
        let mut reqs = self.requests.lock().unwrap();
        if let Some(r) = reqs.iter_mut().rev().take(20).find(|r| {
            r.kind == "Document"
                && r.load == old
                && r.status.is_none()
                && r.failed.is_none()
                && r.at.elapsed() < Duration::from_secs(5)
        }) {
            r.load = old + 1;
        }
    }
    pub fn nav_finished(&self, ok: bool) {
        self.navigating.store(false, Ordering::SeqCst);
        let mut r = self.nav_result.lock().unwrap();
        // a failure stays a failure even when the error page finishes loading afterwards
        if *r != Some(false) {
            *r = Some(ok);
        }
    }
    pub fn dialog(&self, line: String) {
        self.dialogs.lock().unwrap().push(line);
    }
    pub fn is_navigating(&self) -> bool {
        self.navigating.load(Ordering::SeqCst)
    }
    pub fn navigation_count(&self) -> u32 {
        self.navigations.load(Ordering::SeqCst)
    }
    /// Sequence number of the next console message, to find the ones an action produced.
    pub fn console_seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst)
    }

    fn console_add(&self, level: &str, text: String) {
        let mut text = text.trim_end().to_string();
        if text.chars().count() > MAX_MESSAGE {
            text = text.chars().take(MAX_MESSAGE).collect::<String>() + "…";
        }
        let mut c = self.console.lock().unwrap();
        if c.len() >= MAX_CONSOLE {
            c.pop_front();
        }
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        c.push_back(ConsoleEntry { seq, load: self.navigation_count(), level: level.to_string(), text });
    }

    /// DevTools events the commands care about: console messages, uncaught errors and requests.
    /// Both engines forward the same events here.
    pub fn cdp_event(&self, method: &str, p: &Value) {
        match method {
            "Runtime.consoleAPICalled" => {
                let level = match p["type"].as_str().unwrap_or("log") {
                    "warning" => "warning",
                    "error" | "assert" => "error",
                    "debug" => "debug",
                    "info" => "info",
                    _ => "log",
                };
                let args = p["args"].as_array().map(|a| a.iter().map(remote_text).collect::<Vec<_>>().join(" "));
                let mut text = args.unwrap_or_default();
                if level == "error" || level == "warning" {
                    text.push_str(&location(&p["stackTrace"]["callFrames"][0]));
                }
                self.console_add(level, text);
            }
            "Runtime.exceptionThrown" => {
                let d = &p["exceptionDetails"];
                let desc = d["exception"]["description"].as_str().or(d["text"].as_str()).unwrap_or("error");
                let first = desc.lines().next().unwrap_or("");
                let mut text = if first.starts_with("Uncaught") { first.to_string() } else { format!("Uncaught {first}") };
                text.push_str(&location(d));
                self.console_add("error", text);
            }
            "Log.entryAdded" => {
                let e = &p["entry"];
                let level = e["level"].as_str().unwrap_or("info");
                if level == "verbose" {
                    return;
                }
                let mut text = e["text"].as_str().unwrap_or("").to_string();
                if let Some(url) = e["url"].as_str().filter(|u| !u.is_empty()) {
                    text.push_str(&format!(" ({})", short_url(url)));
                }
                self.console_add(level, text);
            }
            "Network.requestWillBeSent" => {
                let id = p["requestId"].as_str().unwrap_or("").to_string();
                let mut reqs = self.requests.lock().unwrap();
                if let Some(redirect) = p.get("redirectResponse") {
                    if let Some(r) = reqs.iter_mut().rev().find(|r| r.id == id && r.status.is_none()) {
                        r.status = redirect["status"].as_i64();
                    }
                }
                let url = p["request"]["url"].as_str().unwrap_or("").to_string();
                if url.starts_with("data:") {
                    return;
                }
                if reqs.len() >= MAX_REQUESTS {
                    reqs.pop_front();
                }
                reqs.push_back(Request {
                    id,
                    load: self.navigation_count(),
                    at: Instant::now(),
                    method: p["request"]["method"].as_str().unwrap_or("GET").to_string(),
                    url,
                    kind: p["type"].as_str().unwrap_or("Other").to_string(),
                    status: None,
                    failed: None,
                });
            }
            "Network.responseReceived" => {
                let id = p["requestId"].as_str().unwrap_or("");
                if let Some(r) = self.requests.lock().unwrap().iter_mut().rev().find(|r| r.id == id) {
                    r.status = p["response"]["status"].as_i64();
                }
            }
            "Network.loadingFailed" => {
                let id = p["requestId"].as_str().unwrap_or("");
                if let Some(r) = self.requests.lock().unwrap().iter_mut().rev().find(|r| r.id == id) {
                    let canceled = p["canceled"].as_bool() == Some(true);
                    let err = p["errorText"].as_str().unwrap_or("failed");
                    r.failed = Some(if canceled { "canceled".into() } else { err.to_string() });
                }
            }
            _ => {}
        }
    }
}

/// The events `Shared::cdp_event` handles. WebView2 subscribes to these one by one (Chrome sends all).
#[cfg_attr(not(windows), allow(dead_code))]
pub const EVENTS: &[&str] = &[
    "Runtime.consoleAPICalled",
    "Runtime.exceptionThrown",
    "Log.entryAdded",
    "Network.requestWillBeSent",
    "Network.responseReceived",
    "Network.loadingFailed",
];

/// A console argument as text: strings as they are, other values as JSON or their description.
fn remote_text(o: &Value) -> String {
    if let Some(v) = o.get("value") {
        return v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
    }
    o["description"].as_str().or(o["unserializableValue"].as_str()).or(o["type"].as_str()).unwrap_or("").to_string()
}

/// " (file.js:12)" for a call frame or exception details, empty when there is no URL.
fn location(v: &Value) -> String {
    match v["url"].as_str().filter(|u| !u.is_empty()) {
        Some(url) => format!(" ({}:{})", short_url(url), v["lineNumber"].as_i64().unwrap_or(0) + 1),
        None => String::new(),
    }
}

fn short_url(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.trim_end_matches('/').rsplit_once('/') {
        Some((_, last)) if !last.is_empty() => last,
        _ => url,
    }
}

pub trait Engine {
    /// The state of the selected tab.
    fn shared(&self) -> Arc<Shared>;
    /// One DevTools Protocol call on the selected tab. Fails after `timeout`.
    fn cdp_call(&self, method: &str, params: &Value, timeout: Duration) -> R<Value>;
    /// Waits until `done` returns true or the timeout passes, keeping the engine responsive.
    fn pump(&self, timeout: Duration, done: &mut dyn FnMut() -> bool) -> R<bool>;
    fn go(&self, nav: &Nav) -> R<()>;

    /// Ids of the open tabs, oldest first.
    fn tabs(&self) -> Vec<u32>;
    /// Opens a blank tab in the background and selects it.
    fn new_tab(&self) -> R<u32>;
    /// Makes the tab the one the commands act on.
    fn select_tab(&self, id: u32) -> R<()>;
    fn close_tab(&self, id: u32) -> R<()>;
    /// Title and URL of any tab.
    fn tab_info(&self, id: u32) -> (String, String);
    /// Asks for the user's attention without taking focus, if the engine can.
    fn attention(&self) {}
    /// Names the agent (session) a tab belongs to, for engines that show their tabs.
    fn label_tab(&self, _id: u32, _label: &str) {}
    /// The tab a command is running in, or None once it is done, for engines that show it.
    fn set_busy(&self, _id: Option<u32>) {}

    fn sleep(&self, ms: u64) -> R<()> {
        self.pump(Duration::from_millis(ms), &mut || false).map(|_| ())
    }

    fn title_url(&self) -> (String, String) {
        let r = self.cdp_call(
            "Runtime.evaluate",
            &json!({ "expression": "[document.title, location.href]", "returnByValue": true }),
            Duration::from_secs(5),
        );
        let v = r.ok().map(|r| r["result"]["value"].clone()).unwrap_or_default();
        (v[0].as_str().unwrap_or("").to_string(), v[1].as_str().unwrap_or("").to_string())
    }
}
