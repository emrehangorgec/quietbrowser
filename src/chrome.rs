// The engine for macOS and Linux (and Windows with QUIETBROWSER_ENGINE=chrome): the user's own
// Chrome, Edge, Brave or Chromium, started in its own profile without being brought to the front,
// and driven over the DevTools Protocol.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::engine::{Engine, Nav, R, Shared};
use crate::{log, ws};

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, String>>>>>;

pub struct Chrome {
    writer: Arc<Mutex<ws::Writer>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    s: Arc<Shared>,
    port: u16,
    browser_ws: String,
}

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

fn launch(profile: &Path) -> R<()> {
    let browser = find_browser()?;
    log(&format!("starting {}", browser.display()));
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
    flags.push("about:blank".into());

    let mut cmd = if cfg!(target_os = "macos") && browser.extension().is_some_and(|e| e == "app") {
        // -g: launch in the background, -n: a separate instance even if the user's browser runs
        let mut c = std::process::Command::new("open");
        c.arg("-g").arg("-n").arg("-a").arg(&browser).arg("--args").args(&flags);
        c
    } else {
        let mut c = std::process::Command::new(&browser);
        c.args(&flags);
        c
    };
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map_err(|e| format!("could not start {}: {e}", browser.display()))?;
    Ok(())
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
        let page = loop {
            let list: Value = serde_json::from_str(&http_get(port, "/json/list")?).unwrap_or_default();
            if let Some(p) = list.as_array().and_then(|a| a.iter().find(|t| t["type"] == "page").cloned()) {
                break p;
            }
            if start.elapsed() > Duration::from_secs(30) {
                return Err("the browser opened no page".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let url = page["webSocketDebuggerUrl"].as_str().ok_or("no DevTools socket for the page")?;
        let main_frame = page["id"].as_str().unwrap_or("").to_string();
        let (reader, writer) = ws::connect(url).map_err(|e| e.to_string())?;
        let c = Chrome {
            writer: Arc::new(Mutex::new(writer)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            s: Arc::new(Shared::default()),
            port,
            browser_ws: format!("ws://127.0.0.1:{port}{browser_path}"),
        };
        c.spawn_reader(reader, main_frame);
        let t = Duration::from_secs(10);
        c.cdp_call("Page.enable", &json!({}), t)?;
        c.cdp_call("Page.addScriptToEvaluateOnNewDocument", &json!({ "source": IN_PLACE }), t)?;
        let _ = c.cdp_call("Page.setDownloadBehavior", &json!({ "behavior": "deny" }), t);
        log(&format!("chrome ready on port {port}"));
        Ok(c)
    }

    fn send(writer: &Mutex<ws::Writer>, id: u64, method: &str, params: &Value) -> R<()> {
        let msg = json!({ "id": id, "method": method, "params": params }).to_string();
        writer.lock().unwrap().send_text(&msg).map_err(|e| format!("the browser closed: {e}"))
    }

    /// Reads every message from the page socket: answers go to the waiting call, events update the
    /// shared state. Dialogs are answered right away, even between commands.
    fn spawn_reader(&self, mut reader: ws::Reader, main_frame: String) {
        let (pending, s, writer, ids) = (self.pending.clone(), self.s.clone(), self.writer.clone(), self.next_id.clone());
        let (port, browser_ws) = (self.port, self.browser_ws.clone());
        std::thread::spawn(move || {
            loop {
                let text = match reader.recv() {
                    Ok(t) => t,
                    Err(e) => {
                        // The window was closed. Quit the browser too, so the next command starts fresh.
                        log(&format!("page socket closed: {e}"));
                        close_browser(port, browser_ws.trim_start_matches(&format!("ws://127.0.0.1:{port}")));
                        std::process::exit(0);
                    }
                };
                let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
                if let Some(id) = msg["id"].as_u64() {
                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                        let r = match msg.get("error") {
                            Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                            None => Ok(msg["result"].clone()),
                        };
                        let _ = tx.send(r);
                    }
                    continue;
                }
                let p = &msg["params"];
                match msg["method"].as_str().unwrap_or("") {
                    "Page.javascriptDialogOpening" => {
                        let kind = p["type"].as_str().unwrap_or("alert");
                        s.dialog(format!("{kind}: {}", p["message"].as_str().unwrap_or("")));
                        let answer = json!({ "accept": true, "promptText": p["defaultPrompt"].as_str().unwrap_or("") });
                        let id = ids.fetch_add(1, Ordering::SeqCst);
                        let _ = Chrome::send(&writer, id, "Page.handleJavaScriptDialog", &answer);
                    }
                    "Page.frameStartedLoading" if p["frameId"] == main_frame.as_str() => s.nav_started(),
                    "Page.frameStoppedLoading" if p["frameId"] == main_frame.as_str() => s.nav_finished(true),
                    _ => {}
                }
            }
        });
    }

    pub fn close(&self) {
        close_browser(self.port, self.browser_ws.trim_start_matches(&format!("ws://127.0.0.1:{}", self.port)));
    }
}

fn close_browser(port: u16, path: &str) {
    if let Ok((_r, mut w)) = ws::connect(&format!("ws://127.0.0.1:{port}{path}")) {
        let _ = w.send_text(&json!({ "id": 1, "method": "Browser.close" }).to_string());
        std::thread::sleep(Duration::from_millis(300));
    }
}

impl Engine for Chrome {
    fn shared(&self) -> &Shared {
        &self.s
    }

    fn cdp_call(&self, method: &str, params: &Value, timeout: Duration) -> R<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = Chrome::send(&self.writer, id, method, params) {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(r) => r.map_err(|e| format!("{method} failed: {e}")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("the page did not answer {method} within {} s", timeout.as_secs()))
            }
        }
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
                    self.s.nav_finished(false);
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
}
