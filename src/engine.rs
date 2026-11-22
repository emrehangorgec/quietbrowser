// What the agent commands need from a browser: DevTools calls, navigation and a way to wait.
// On Windows this is a WebView2 window; elsewhere it is Chrome (or another Chromium) over CDP.
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

pub type R<T> = Result<T, String>;

pub enum Nav {
    Url(String),
    Back,
    Reload,
}

/// State written by the engine's event handlers and read by the commands.
#[derive(Default)]
pub struct Shared {
    pub navigating: AtomicBool,
    pub navigations: AtomicU32,
    pub nav_result: Mutex<Option<bool>>,
    pub dialogs: Mutex<Vec<String>>,
    pub refs: Mutex<crate::agent::Refs>,
    pub last_snapshot: Mutex<Option<String>>,
}

impl Shared {
    /// A new document started loading: refs from the old one are no longer valid.
    pub fn nav_started(&self) {
        self.navigating.store(true, Ordering::SeqCst);
        self.navigations.fetch_add(1, Ordering::SeqCst);
        self.refs.lock().unwrap().clear();
        *self.last_snapshot.lock().unwrap() = None;
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
}

pub trait Engine {
    fn shared(&self) -> &Shared;
    /// One DevTools Protocol call. Fails after `timeout`.
    fn cdp_call(&self, method: &str, params: &Value, timeout: Duration) -> R<Value>;
    /// Waits until `done` returns true or the timeout passes, keeping the engine responsive.
    fn pump(&self, timeout: Duration, done: &mut dyn FnMut() -> bool) -> R<bool>;
    fn go(&self, nav: &Nav) -> R<()>;

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
