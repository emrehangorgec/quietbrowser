// The engine for macOS and Linux (and Windows with QUIETBROWSER_ENGINE=chrome): the user's own
// Chrome, Edge, Brave or Chromium, started in its own profile without being brought to the front,
// and driven over the DevTools Protocol.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::engine::{Engine, Nav, R, Shared};
use crate::{log, ws};

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, String>>>>>;

/// Runs in every page before its own scripts: popups and target=_blank links open in the same
/// window, so no new window can come to the front.
const IN_PLACE: &str = r#"(() => {
  const open = (u) => { if (u) location.href = new URL(String(u), location.href).href; return window; };
  try { Object.defineProperty(window, "open", { value: open, configurable: true, writable: true }); } catch (e) {}
  const fix = (e) => {
    const el = e.target && e.target.closest && e.target.closest("a[target], form[target], area[target]");
    if (el && el.target && el.target !== "_self") el.target = "_self";
  };
  document.addEventListener("click", fix, true);
  document.addEventListener("submit", fix, true);
})();"#;

fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("QUIETBROWSER_CHROME") {
        v.push(PathBuf::from(p));
    }
    if cfg!(target_os = "macos") {
        let home = std::env::var("HOME").unwrap_or_default();
        for app in ["Google Chrome", "Chromium", "Microsoft Edge", "Brave Browser", "Google Chrome Canary"] {
            for root in ["/Applications".to_string(), format!("{home}/Applications")] {
                v.push(PathBuf::from(format!("{root}/{app}.app")));
            }
        }
    } else if cfg!(windows) {
        for var in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Ok(root) = std::env::var(var) {
                v.push(Path::new(&root).join(r"Google\Chrome\Application\chrome.exe"));
                v.push(Path::new(&root).join(r"Microsoft\Edge\Application\msedge.exe"));
            }
        }
    } else {
        let path = std::env::var("PATH").unwrap_or_default();
        for name in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge", "brave-browser"] {
            for dir in path.split(':').filter(|d| !d.is_empty()) {
                v.push(Path::new(dir).join(name));
            }
        }
    }
    v
}

fn find_browser() -> R<PathBuf> {
    candidates().into_iter().find(|p| p.exists()).ok_or_else(|| {
        "no Chromium browser found. Install Google Chrome, Chromium, Edge or Brave, \
         or set QUIETBROWSER_CHROME to its path"
            .to_string()
    })
}

/// GET on the DevTools HTTP endpoint.
fn http_get(port: u16, path: &str) -> R<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").map_err(|e| e.to_string())?;
    // Chrome may keep the connection open, so stop at Content-Length instead of reading to the end.
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = s.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..i]).to_ascii_lowercase();
            let len = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok());
            if len.is_some_and(|len| raw.len() >= i + 4 + len) {
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    Ok(text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
}

/// Port and browser socket path from the DevToolsActivePort file Chrome writes into its profile.
fn read_active_port(profile: &Path) -> Option<(u16, String)> {
    let text = std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?;
    let mut lines = text.lines();
    let port = lines.next()?.trim().parse().ok()?;
    Some((port, lines.next().unwrap_or("").trim().to_string()))
}

/// Chrome raises its own window when it creates one, so by default it runs without a window and
/// can never take focus. QUIETBROWSER_HEADED=1 shows the window (to watch, or to log in by hand),
/// at the cost of that guarantee.
pub fn headed() -> bool {
    std::env::var("QUIETBROWSER_HEADED").is_ok_and(|v| v == "1")
}

fn launch(profile: &Path) -> R<()> {
    let browser = find_browser()?;
    let headed = headed();
    log(&format!("starting {} ({})", browser.display(), if headed { "headed" } else { "headless" }));
    let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));
    let mut flags = vec![
        format!("--user-data-dir={}", profile.display()),
        "--remote-debugging-port=0".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-session-crashed-bubble".into(),
        "--hide-crash-restore-bubble".into(),
        "--disable-features=Translate,MediaRouter".into(),
        "--window-position=60,60".into(),
        "--window-size=1100,760".into(),
    ];
    if cfg!(target_os = "macos") {
        // the keychain prompt is a dialog that would take focus
        flags.push("--use-mock-keychain".into());
        flags.push("--password-store=basic".into());
    }
    if !headed {
        flags.push("--headless=new".into());
    }
    flags.push("about:blank".into());

    let mut exe = browser.clone();
    let is_bundle = browser.extension().is_some_and(|e| e == "app");
    if is_bundle && !headed {
        let name = browser.file_stem().unwrap_or_default().to_os_string();
        exe = browser.join("Contents").join("MacOS").join(name);
    } else if cfg!(target_os = "macos") && is_bundle {
        // -g: launch in the background, -n: a separate instance even if the user's browser runs
        // `open` normally returns at once. When macOS holds the launch (a first run prompt, for
        // example) it blocks and the app stays suspended, so give up on it after a while.
        let child = std::process::Command::new("open")
            .arg("-g")
            .arg("-n")
            .arg("-a")
            .arg(&browser)
            .arg("--args")
            .args(&flags)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match child {
            Ok(mut c) => {
                let start = Instant::now();
                loop {
                    match c.try_wait() {
                        Ok(Some(status)) if status.success() => return Ok(()),
                        Ok(Some(status)) => {
                            log(&format!("open failed: {status}"));
                            break;
                        }
                        Ok(None) if start.elapsed() < Duration::from_secs(10) => {
                            std::thread::sleep(Duration::from_millis(100))
                        }
                        _ => {
                            log("open did not return, starting the browser binary directly");
                            let _ = c.kill();
                            let _ = c.wait();
                            break;
                        }
                    }
                }
            }
            Err(e) => log(&format!("open failed: {e}")),
        }
        // fall back to the binary inside the bundle
        let name = browser.file_stem().unwrap_or_default().to_os_string();
        exe = browser.join("Contents").join("MacOS").join(name);
    }
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&flags);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map_err(|e| format!("could not start {}: {e}", exe.display()))?;
    Ok(())
}

/// One call on the browser's own DevTools socket (creating and closing tabs).
fn browser_call(port: u16, path: &str, method: &str, params: Value) -> R<Value> {
    let (mut reader, mut writer) = ws::connect(&format!("ws://127.0.0.1:{port}{path}")).map_err(|e| e.to_string())?;
    reader.set_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    writer
        .send_text(&json!({ "id": 1, "method": method, "params": params }).to_string())
        .map_err(|e| format!("the browser closed: {e}"))?;
    loop {
        let text = reader.recv().map_err(|e| format!("{method}: {e}"))?;
        let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
        if msg["id"] == 1 {
            return match msg.get("error") {
                Some(e) => Err(format!("{method} failed: {}", e["message"].as_str().unwrap_or("error"))),
                None => Ok(msg["result"].clone()),
            };
        }
    }
}

/// A tab and its own DevTools socket.
#[derive(Clone)]
struct Tab {
    id: u32,
    target: String,
    writer: Arc<Mutex<ws::Writer>>,
    pending: Pending,
    s: Arc<Shared>,
    /// Set before quietbrowser closes the tab itself, so its socket closing is expected.
    closing: Arc<AtomicBool>,
}

pub struct Chrome {
    port: u16,
    browser_path: String,
    next_id: Arc<AtomicU64>,
    tabs: Arc<Mutex<Vec<Tab>>>,
    current: AtomicU32,
    next_tab: AtomicU32,
    closing: Arc<AtomicBool>,
}

impl Chrome {
    pub fn start() -> R<Chrome> {
        let profile = crate::data_dir().join("chrome-profile");
        std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
        // A browser left over from an earlier run still owns the profile; launching again would only
        // hand the arguments to it. Reuse it instead.
        let alive = read_active_port(&profile).is_some_and(|(port, _)| http_get(port, "/json/version").is_ok());
        if alive {
            log("reusing the browser from an earlier run");
        } else {
            launch(&profile)?;
        }
        let start = Instant::now();
        let (port, browser_path) = loop {
            if let Some(found) = read_active_port(&profile) {
                if http_get(found.0, "/json/version").is_ok() {
                    break found;
                }
            }
            if start.elapsed() > Duration::from_secs(30) {
                return Err("the browser did not open its DevTools port within 30 s".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let target = loop {
            let list: Value = serde_json::from_str(&http_get(port, "/json/list")?).unwrap_or_default();
            if let Some(p) = list.as_array().and_then(|a| a.iter().find(|t| t["type"] == "page").cloned()) {
                break p["id"].as_str().unwrap_or("").to_string();
            }
            if start.elapsed() > Duration::from_secs(30) {
                return Err("the browser opened no page".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let c = Chrome {
            port,
            browser_path,
            next_id: Arc::new(AtomicU64::new(1)),
            tabs: Arc::new(Mutex::new(Vec::new())),
            current: AtomicU32::new(0),
            next_tab: AtomicU32::new(1),
            closing: Arc::new(AtomicBool::new(false)),
        };
        let id = c.attach(&target)?;
        c.current.store(id, Ordering::SeqCst);
        log(&format!("chrome ready on port {port}"));
        Ok(c)
    }

    /// Connects to a page target and prepares it. Returns the new tab's id.
    fn attach(&self, target: &str) -> R<u32> {
        let url = format!("ws://127.0.0.1:{}/devtools/page/{target}", self.port);
        let (reader, writer) = ws::connect(&url).map_err(|e| e.to_string())?;
        let tab = Tab {
            id: self.next_tab.fetch_add(1, Ordering::SeqCst),
            target: target.to_string(),
            writer: Arc::new(Mutex::new(writer)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            s: Arc::new(Shared::default()),
            closing: Arc::new(AtomicBool::new(false)),
        };
        self.spawn_reader(reader, tab.clone());
        self.tabs.lock().unwrap().push(tab.clone());
        let t = Duration::from_secs(10);
        self.call_on(&tab, "Page.enable", &json!({}), t)?;
        self.call_on(&tab, "Page.addScriptToEvaluateOnNewDocument", &json!({ "source": IN_PLACE }), t)?;
        let _ = self.call_on(&tab, "Page.setDownloadBehavior", &json!({ "behavior": "deny" }), t);
        Ok(tab.id)
    }

    fn send(writer: &Mutex<ws::Writer>, id: u64, method: &str, params: &Value) -> R<()> {
        let msg = json!({ "id": id, "method": method, "params": params }).to_string();
        writer.lock().unwrap().send_text(&msg).map_err(|e| format!("the browser closed: {e}"))
    }

    fn call_on(&self, tab: &Tab, method: &str, params: &Value, timeout: Duration) -> R<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        tab.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = Chrome::send(&tab.writer, id, method, params) {
            tab.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(r) => r.map_err(|e| format!("{method} failed: {e}")),
            Err(_) => {
                tab.pending.lock().unwrap().remove(&id);
                Err(format!("the page did not answer {method} within {} s", timeout.as_secs()))
            }
        }
    }

    fn current_tab(&self) -> R<Tab> {
        let id = self.current.load(Ordering::SeqCst);
        let tabs = self.tabs.lock().unwrap();
        tabs.iter().find(|t| t.id == id).cloned().ok_or_else(|| "the tab was closed".to_string())
    }

    /// Reads every message from a tab's socket: answers go to the waiting call, events update the
    /// tab's state. Dialogs are answered right away, even between commands.
    fn spawn_reader(&self, mut reader: ws::Reader, tab: Tab) {
        let (tabs, ids, closing) = (self.tabs.clone(), self.next_id.clone(), self.closing.clone());
        let (port, path) = (self.port, self.browser_path.clone());
        std::thread::spawn(move || {
            loop {
                let text = match reader.recv() {
                    Ok(t) => t,
                    Err(e) => {
                        log(&format!("tab {} socket closed: {e}", tab.id));
                        if tab.closing.load(Ordering::SeqCst) || closing.load(Ordering::SeqCst) {
                            return;
                        }
                        // Closed in the browser itself (a headed window, or a crash).
                        let left = {
                            let mut t = tabs.lock().unwrap();
                            t.retain(|x| x.id != tab.id);
                            t.len()
                        };
                        if left == 0 || http_get(port, "/json/version").is_err() {
                            // The window was closed. Quit the browser too, so the next command starts fresh.
                            close_browser(port, &path);
                            std::process::exit(0);
                        }
                        return;
                    }
                };
                let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
                if let Some(id) = msg["id"].as_u64() {
                    if let Some(tx) = tab.pending.lock().unwrap().remove(&id) {
                        let r = match msg.get("error") {
                            Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                            None => Ok(msg["result"].clone()),
                        };
                        let _ = tx.send(r);
                    }
                    continue;
                }
                let p = &msg["params"];
                let s = &tab.s;
                match msg["method"].as_str().unwrap_or("") {
                    "Page.javascriptDialogOpening" => {
                        let kind = p["type"].as_str().unwrap_or("alert");
                        s.dialog(format!("{kind}: {}", p["message"].as_str().unwrap_or("")));
                        let answer = json!({ "accept": true, "promptText": p["defaultPrompt"].as_str().unwrap_or("") });
                        let id = ids.fetch_add(1, Ordering::SeqCst);
                        let _ = Chrome::send(&tab.writer, id, "Page.handleJavaScriptDialog", &answer);
                    }
                    // a page target's id is also the id of its main frame
                    "Page.frameStartedLoading" if p["frameId"] == tab.target.as_str() => s.nav_started(),
                    "Page.frameStoppedLoading" if p["frameId"] == tab.target.as_str() => s.nav_finished(true),
                    m => s.cdp_event(m, p),
                }
            }
        });
    }

    pub fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
        close_browser(self.port, &self.browser_path);
    }
}

fn close_browser(port: u16, path: &str) {
    if let Ok((_r, mut w)) = ws::connect(&format!("ws://127.0.0.1:{port}{path}")) {
        let _ = w.send_text(&json!({ "id": 1, "method": "Browser.close" }).to_string());
    }
    // Wait until the browser is gone, so a browser started right after does not find the profile
    // still in use.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) && http_get(port, "/json/version").is_ok() {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
}

impl Engine for Chrome {
    fn shared(&self) -> Arc<Shared> {
        self.current_tab().map(|t| t.s).unwrap_or_default()
    }

    fn cdp_call(&self, method: &str, params: &Value, timeout: Duration) -> R<Value> {
        let tab = self.current_tab()?;
        self.call_on(&tab, method, params, timeout)
    }

    fn cdp_call_for(&self, id: u32, method: &str, params: &Value, timeout: Duration) -> R<Value> {
        let tab = self.tabs.lock().unwrap().iter().find(|t| t.id == id).cloned().ok_or("the tab was closed")?;
        self.call_on(&tab, method, params, timeout)
    }

    fn pump(&self, timeout: Duration, done: &mut dyn FnMut() -> bool) -> R<bool> {
        let start = Instant::now();
        loop {
            if done() {
                return Ok(true);
            }
            if start.elapsed() >= timeout {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn go(&self, nav: &Nav) -> R<()> {
        let t = Duration::from_secs(10);
        match nav {
            Nav::Url(url) => {
                let r = self.cdp_call("Page.navigate", &json!({ "url": url }), t)?;
                if let Some(err) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
                    log(&format!("navigation failed: {err}"));
                    self.shared().nav_finished(false);
                }
                Ok(())
            }
            Nav::Back => {
                let h = self.cdp_call("Page.getNavigationHistory", &json!({}), t)?;
                let i = h["currentIndex"].as_i64().unwrap_or(0);
                if i < 1 {
                    return Err("there is no page to go back to".into());
                }
                let id = h["entries"][(i - 1) as usize]["id"].clone();
                self.cdp_call("Page.navigateToHistoryEntry", &json!({ "entryId": id }), t).map(|_| ())
            }
            Nav::Reload => self.cdp_call("Page.reload", &json!({}), t).map(|_| ()),
        }
    }

    fn tabs(&self) -> Vec<u32> {
        self.tabs.lock().unwrap().iter().map(|t| t.id).collect()
    }

    fn new_tab(&self) -> R<u32> {
        // Without a window, a tab of its own window is never hidden (hidden pages are throttled).
        // With a window, a new window would come to the front, so open a background tab instead.
        let params = json!({ "url": "about:blank", "newWindow": !headed(), "background": true });
        let r = browser_call(self.port, &self.browser_path, "Target.createTarget", params)?;
        let target = r["targetId"].as_str().ok_or("the browser opened no tab")?.to_string();
        let id = self.attach(&target)?;
        self.current.store(id, Ordering::SeqCst);
        Ok(id)
    }

    fn select_tab(&self, id: u32) -> R<()> {
        if !self.tabs().contains(&id) {
            return Err("the tab was closed".into());
        }
        self.current.store(id, Ordering::SeqCst);
        Ok(())
    }

    fn close_tab(&self, id: u32) -> R<()> {
        let tab = {
            let mut tabs = self.tabs.lock().unwrap();
            let Some(i) = tabs.iter().position(|t| t.id == id) else { return Ok(()) };
            tabs.remove(i)
        };
        tab.closing.store(true, Ordering::SeqCst);
        browser_call(self.port, &self.browser_path, "Target.closeTarget", json!({ "targetId": tab.target }))?;
        Ok(())
    }

    fn tab_info(&self, id: u32) -> (String, String) {
        let Some(target) = self.tabs.lock().unwrap().iter().find(|t| t.id == id).map(|t| t.target.clone()) else {
            return Default::default();
        };
        let list: Value = http_get(self.port, "/json/list").ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let t = list.as_array().and_then(|a| a.iter().find(|t| t["id"] == target.as_str()).cloned()).unwrap_or_default();
        (t["title"].as_str().unwrap_or("").to_string(), t["url"].as_str().unwrap_or("").to_string())
    }
}
