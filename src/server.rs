// Local HTTP API. Each request is handed to the UI thread and answered with plain text.
// One command runs at a time.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::time::Duration;

use windows::Win32::UI::WindowsAndMessaging::{WM_APP, WM_CLOSE};

pub struct Job {
    pub cmd: String,
    pub args: Vec<String>,
    pub reply: Sender<Result<String, String>>,
}

static HWND: AtomicIsize = AtomicIsize::new(0);
static QUEUE: Mutex<Option<Sender<Job>>> = Mutex::new(None);

thread_local! {
    static JOBS: std::cell::RefCell<Option<Receiver<Job>>> = const { std::cell::RefCell::new(None) };
}

/// Called on the UI thread.
pub fn next_job() -> Option<Job> {
    JOBS.with(|j| j.borrow().as_ref().and_then(|rx| rx.try_recv().ok()))
}

/// Binds the port and serves in background threads. Call on the UI thread once the browser is ready.
pub fn start(port: u16, hwnd: isize) -> std::io::Result<()> {
    HWND.store(hwnd, Ordering::SeqCst);
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
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let (path, query, body) = read_request(&mut stream)?;
    let (code, text) = match dispatch(&path, &query, &body) {
        Ok(t) => (200, t),
        Err(e) => (500, e),
    };
    let reason = if code == 200 { "OK" } else { "Internal Server Error" };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        text.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(text.as_bytes())?;
    stream.flush()
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<(String, Vec<(String, String)>, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 1 << 20 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buf.len() < header_end + length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..buf.len().min(header_end + length)]).to_string();
    let target = head.lines().next().unwrap_or("").split(' ').nth(1).unwrap_or("/").to_string();
    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
    let query = q
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    Ok((path.to_string(), query, body))
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

fn run(cmd: &str, args: Vec<String>) -> Result<String, String> {
    let guard = QUEUE.lock().map_err(|_| "busy".to_string())?;
    let tx = guard.as_ref().ok_or("not ready")?;
    let (reply, rx) = mpsc::channel();
    tx.send(Job { cmd: cmd.to_string(), args, reply }).map_err(|_| "the browser closed".to_string())?;
    crate::browser::post_to_window(HWND.load(Ordering::SeqCst), WM_APP);
    rx.recv_timeout(Duration::from_secs(300)).map_err(|_| "the browser did not answer".to_string())?
}

fn dispatch(path: &str, q: &[(String, String)], body: &str) -> Result<String, String> {
    let get = |k: &str, d: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or(d.to_string());
    match path {
        "/run" => {
            let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("bad request: {e}"))?;
            let cmd = v["cmd"].as_str().ok_or("bad request: cmd missing")?;
            let args = v["args"]
                .as_array()
                .map(|a| a.iter().map(|x| x.as_str().unwrap_or_default().to_string()).collect())
                .unwrap_or_default();
            run(cmd, args)
        }
        "/hwnd" => Ok(HWND.load(Ordering::SeqCst).to_string()),
        // Short query-string forms used by the focus test.
        "/open" => run("open", vec![get("url", "about:blank")]),
        "/eval" => run("eval", vec![get("js", "null")]),
        "/click" => run("click", vec![get("sel", "")]),
        "/fill" => run("fill", vec![get("sel", ""), get("text", "")]),
        "/dialogs" => run("dialogs", vec![]),
        "/quit" => {
            crate::browser::post_to_window(HWND.load(Ordering::SeqCst), WM_CLOSE);
            Ok("bye".into())
        }
        _ => Err(format!("unknown endpoint {path}")),
    }
}
