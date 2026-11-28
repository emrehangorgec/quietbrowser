// Local HTTP API. Each request is handed to the engine's thread and answered with plain text.
// One command runs at a time.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The job that closes the browser, with the tabs of every session.
pub const QUIT: &str = "__quit";
pub const DEFAULT_SESSION: &str = "default";
pub const UI: &str = "__ui";

pub struct Job {
    /// Which agent sent it. Each session has its own tabs.
    pub session: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub reply: Sender<Result<String, String>>,
}

static ID: OnceLock<String> = OnceLock::new();
static WAKE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
static QUEUE: Mutex<Option<Sender<Job>>> = Mutex::new(None);

thread_local! {
    static JOBS: std::cell::RefCell<Option<Receiver<Job>>> = const { std::cell::RefCell::new(None) };
}

/// Called on the engine's thread (the thread that called `start`).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn next_job() -> Option<Job> {
    JOBS.with(|j| j.borrow().as_ref().and_then(|rx| rx.try_recv().ok()))
}

/// Blocks until a job arrives or the timeout passes. Called on the engine's thread.
pub fn wait_job(timeout: Duration) -> Option<Job> {
    JOBS.with(|j| j.borrow().as_ref().and_then(|rx| rx.recv_timeout(timeout).ok()))
}

/// Binds the port and serves in background threads. Call on the engine's thread once the browser
/// is ready. `id` is reported at /hwnd (the window handle on Windows). `wake` is called after a job
/// is queued, for engines whose thread is busy with something else, like a message loop.
pub fn start(port: u16, id: String, wake: Box<dyn Fn() + Send + Sync>) -> std::io::Result<()> {
    let _ = ID.set(id);
    let _ = WAKE.set(wake);
    let (tx, rx) = mpsc::channel();
    JOBS.with(|j| *j.borrow_mut() = Some(rx));
    *QUEUE.lock().unwrap() = Some(tx);
    let v4 = TcpListener::bind(("127.0.0.1", port))?;
    let mut listeners = vec![v4];
    if let Ok(v6) = TcpListener::bind(("::1", port)) {
        listeners.push(v6); // "localhost" may resolve to ::1 first
    }
    for l in listeners {
        std::thread::spawn(move || {
            for stream in l.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = handle(stream);
                });
            }
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let port = stream.local_addr()?.port();
    let request = read_request(&mut stream)?;
    let (code, mime, body) = if request.path.starts_with("/ui") {
        serve_ui(&request, port)
    } else if request.header("X-QuietBrowser-Client") != Some("1")
        || request.header("Origin").is_some()
        || request.header("Sec-Fetch-Site").is_some()
    {
        (403, "text/plain", "Quietbrowser client required".into())
    } else {
        match dispatch(&request.path, &request.query, &request.body) {
            Ok(text) => (200, "text/plain", text),
            Err(e) => (500, "text/plain", e),
        }
    };
    let reason = match code {
        200 => "OK", 403 => "Forbidden",
        404 => "Not Found", 409 => "Conflict", _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {mime}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'self'; img-src data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    head: String,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }
}

fn serve_ui(req: &Request, port: u16) -> (u16, &'static str, String) {
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    if req.header("Host") != Some(host.as_str())
        || req.header("Origin").is_some_and(|o| o != origin)
    {
        return (403, "text/plain", "Open the local address printed by quietbrowser ui".into());
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/ui" | "/ui/") => return (200, "text/html", crate::ui::HTML.into()),
        ("GET", "/ui/app.js") => return (200, "text/javascript", crate::ui::JS.into()),
        ("GET", "/ui/app.css") => return (200, "text/css", crate::ui::CSS.into()),
        _ => {}
    }
    if req.header("X-QuietBrowser-UI") != Some("1")
        || req.header("Sec-Fetch-Site").is_some_and(|s| !matches!(s, "same-origin" | "none"))
    {
        return (403, "text/plain", "Quietbrowser UI required".into());
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/ui/state") => (200, "application/json", crate::ui::state()),
        ("POST", "/ui/action") => match run("", UI, vec![req.body.clone()]) {
            Ok(text) => (200, "application/json", text),
            Err(e) => (409, "text/plain", e),
        },
        _ => (404, "text/plain", "Unknown UI route".into()),
    }
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Request> {
    const MAX_HEADER: usize = 16 * 1024;
    const MAX_BODY: usize = 1024 * 1024;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            if i <= MAX_HEADER { break i + 4; }
        }
        if buf.len() > MAX_HEADER {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut line = head.split_whitespace();
    let method = line.next().unwrap_or("").to_string();
    let target = line.next().unwrap_or("/");
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let path = path.to_string();
    let query = q.split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (decode(k), decode(v))
    }).collect();
    let mut req = Request { method, path, query, head, body: String::new() };
    let length: usize = req.header("Content-Length").unwrap_or("0").parse()
        .map_err(|_| std::io::ErrorKind::InvalidData)?;
    if length > MAX_BODY || req.header("Transfer-Encoding").is_some() {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let already_read = buf.len().min(header_end + length);
    buf.resize(header_end + length, 0);
    stream.read_exact(&mut buf[already_read..])?;
    req.body = String::from_utf8(buf.split_off(header_end)).map_err(|_| std::io::ErrorKind::InvalidData)?;
    Ok(req)
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn run(session: &str, cmd: &str, args: Vec<String>) -> Result<String, String> {
    let tx = QUEUE.lock().map_err(|_| "busy".to_string())?.clone().ok_or("not ready")?;
    let (reply, rx) = mpsc::channel();
    tx.send(Job { session: session.to_string(), cmd: cmd.to_string(), args, reply }).map_err(|_| "the browser closed".to_string())?;
    if let Some(wake) = WAKE.get() {
        wake();
    }
    rx.recv_timeout(Duration::from_secs(300)).map_err(|_| "the browser did not answer".to_string())?
}

fn dispatch(path: &str, q: &[(String, String)], body: &str) -> Result<String, String> {
    let get = |k: &str, d: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or(d.to_string());
    match path {
        "/run" => {
            let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("bad request: {e}"))?;
            let cmd = v["cmd"].as_str().ok_or("bad request: cmd missing")?;
            if cmd == UI { return Err("unknown command".into()); }
            let args = v["args"]
                .as_array()
                .map(|a| a.iter().map(|x| x.as_str().unwrap_or_default().to_string()).collect())
                .unwrap_or_default();
            let session = v["session"].as_str().filter(|s| !s.is_empty()).unwrap_or(DEFAULT_SESSION);
            run(session, cmd, args)
        }
        "/hwnd" => Ok(ID.get().cloned().unwrap_or_default()),
        // Short query-string forms used by the focus test.
        "/open" => run(DEFAULT_SESSION, "open", vec![get("url", "about:blank")]),
        "/eval" => run(DEFAULT_SESSION, "eval", vec![get("js", "null")]),
        "/click" => run(DEFAULT_SESSION, "click", vec![get("sel", "")]),
        "/fill" => run(DEFAULT_SESSION, "fill", vec![get("sel", ""), get("text", "")]),
        "/dialogs" => run(DEFAULT_SESSION, "dialogs", vec![]),
        "/quit" => run(DEFAULT_SESSION, QUIT, vec![]),
        _ => Err(format!("unknown endpoint {path}")),
    }
}
