// quietbrowser: a browser for coding agents that never steals focus.
// One binary. `quietbrowser serve` runs the browser and its local HTTP API; every other command is a
// client that sends one command to it and prints the answer, starting the browser first if needed.
// Windows uses its own WebView2 window; macOS and Linux use the installed Chrome over CDP.
mod agent;
#[cfg(windows)]
mod browser;
mod chrome;
mod engine;
mod mcp;
mod server;
mod ws;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "\
quietbrowser: a browser for coding agents that never steals focus

Usage: quietbrowser <command> [args]

  open <url>             Open a page, print a snapshot
  snapshot               Print the page as an accessibility tree with [ref=eN] handles
  click <ref|selector>   Click an element, print what changed
  fill <ref|selector> <text>
                         Set an input's value, print what changed
  type <text>            Type into the focused element
  press <key>            Press a key (Enter, Tab, Escape, ArrowDown, a, ...)
  hover <ref|selector>   Move the mouse over an element
  select <ref|selector> <value>...
                         Choose options in a <select> by value or label
  text                   Print the page's visible text
  eval <js>              Run JavaScript, print the JSON result
  back | reload          Navigate back or reload
  dialogs                List alert/confirm/prompt dialogs answered so far
  screenshot [file.png]  Save a screenshot of the page, print the file path
  media dark|light|reset Emulate the page's color scheme
  start | quit           Start or close the browser window
  install-skill          Install the agent skill for Claude Code (~/.claude/skills/quietbrowser)
  mcp                    Run as an MCP server over stdio (Playwright MCP tool names)

The window shows up without taking focus. Click it to use it yourself.
Port: QUIETBROWSER_PORT (default 9555).
Engine: QUIETBROWSER_ENGINE=webview2 (Windows default) or chrome (macOS and Linux).
Browser for the chrome engine: QUIETBROWSER_CHROME (found automatically if unset).";

const SKILL: &str = include_str!("../skill/SKILL.md");

fn home() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    PathBuf::from(std::env::var(var).unwrap_or_else(|_| ".".into()))
}

/// Where quietbrowser keeps its profile and log.
pub fn data_dir() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("quietbrowser")
    } else if cfg!(target_os = "macos") {
        home().join("Library/Application Support/quietbrowser")
    } else {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home().join(".local/share"))
            .join("quietbrowser")
    }
}

pub fn log(line: &str) {
    if std::env::var("QUIETBROWSER_LOG").as_deref() != Ok("1") {
        return;
    }
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("log.txt")) {
        let _ = writeln!(f, "{} {line}", timestamp());
    }
}

/// Local time on Windows, UTC elsewhere, as yyyyMMdd-HHmmss.
pub fn timestamp() -> String {
    #[cfg(windows)]
    {
        let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        format!("{:04}{:02}{:02}-{:02}{:02}{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
    }
    #[cfg(not(windows))]
    {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let (days, rem) = ((secs / 86400) as i64, secs % 86400);
        // civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
        let z = days + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
    }
}

pub fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' => continue,
            _ => return Err("bad base64".into()),
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn port() -> u16 {
    std::env::var("QUIETBROWSER_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(9555)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            0
        }
        Some("serve") => serve(&args[1..]),
        Some("mcp") => mcp::serve(),
        Some("-V" | "--version" | "version") => {
            println!("quietbrowser {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some(cmd) => match client(cmd, &args[1..]) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        },
    };
    std::process::exit(code);
}

fn use_chrome(args: &[String]) -> bool {
    if args.iter().any(|a| a == "--chrome") {
        return true;
    }
    match std::env::var("QUIETBROWSER_ENGINE").as_deref() {
        Ok("chrome") => true,
        Ok("webview2") => false,
        _ => !cfg!(windows),
    }
}

/// `serve [--port N] [--normal] [--chrome]`. --normal turns the focus protections off and activates
/// on every navigation (the control run of the focus test, Windows only).
fn serve(args: &[String]) -> i32 {
    let mut port = port();
    let mut normal = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = it.next().and_then(|p| p.parse().ok()).unwrap_or(port),
            "--normal" => normal = true,
            _ => {}
        }
    }
    let result = if use_chrome(args) { serve_chrome(port) } else { serve_webview2(port, normal) };
    match result {
        Ok(()) => 0,
        Err(e) => {
            log(&format!("serve failed: {e}"));
            // read by the client that started us, which has no other way to see the reason
            let _ = std::fs::create_dir_all(data_dir());
            let _ = std::fs::write(data_dir().join("serve-error.txt"), &e);
            eprintln!("error: {e}");
            1
        }
    }
}

fn serve_chrome(port: u16) -> Result<(), String> {
    let c = chrome::Chrome::start()?;
    if let Err(e) = agent::init(&c) {
        log(&format!("init failed: {e}"));
    }
    server::start(port, "0".into(), Box::new(|| {})).map_err(|e| format!("port {port}: {e}"))?;
    log(&format!("serving on {port} (chrome)"));
    loop {
        let Some(job) = server::wait_job(Duration::from_secs(3600)) else { continue };
        if job.cmd == server::QUIT {
            // close first, so the next command never reaches this server while it shuts down
            c.close();
            let _ = job.reply.send(Ok("bye".into()));
            std::thread::sleep(Duration::from_millis(200));
            return Ok(());
        }
        let _ = job.reply.send(agent::run(&c, &job.cmd, &job.args));
    }
}

#[cfg(windows)]
fn serve_webview2(port: u16, normal: bool) -> Result<(), String> {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
    }
    let b = browser::create(normal)?;
    if let Err(e) = agent::init(&*b) {
        log(&format!("init failed: {e}"));
    }
    let hwnd = b.hwnd.0 as isize;
    let wake = Box::new(move || browser::post_to_window(hwnd, windows::Win32::UI::WindowsAndMessaging::WM_APP));
    server::start(port, hwnd.to_string(), wake).map_err(|e| format!("port {port}: {e}"))?;
    log(&format!("serving on {port}"));
    browser::run_loop();
    Ok(())
}

#[cfg(not(windows))]
fn serve_webview2(_port: u16, _normal: bool) -> Result<(), String> {
    Err("the webview2 engine is only available on Windows".into())
}

/// One HTTP request to the window. Returns the status code and the body.
fn request(method: &str, path: &str, body: &str, timeout: Duration) -> std::io::Result<(u16, String)> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port()));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    s.set_read_timeout(Some(timeout))?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let code = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Ok((code, body.to_string()))
}

fn running() -> bool {
    request("GET", "/hwnd", "", Duration::from_secs(5)).map(|(c, _)| c == 200).unwrap_or(false)
}

/// Starts `quietbrowser serve` detached and without inherited handles. An inherited stdout handle
/// would keep the caller's pipe open for as long as the browser runs, and an agent waiting on our
/// output would hang.
/// Returns a check that tells whether the server process has exited.
#[cfg(windows)]
fn spawn_server() -> Result<Box<dyn FnMut() -> bool>, String> {
    use windows::Win32::System::Threading::*;
    use windows::core::{HSTRING, PWSTR};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let line = format!("\"{}\" serve --port {}", exe.display(), port());
    let mut wide: Vec<u16> = line.encode_utf16().chain(Some(0)).collect();
    let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            &HSTRING::from(exe.as_os_str()),
            Some(PWSTR(wide.as_mut_ptr())),
            None,
            None,
            false,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            None,
            None,
            &si,
            &mut pi,
        )
        .map_err(|e| format!("could not start the browser: {e}"))?;
        let _ = windows::Win32::Foundation::CloseHandle(pi.hThread);
    }
    let process = pi.hProcess;
    Ok(Box::new(move || unsafe { WaitForSingleObject(process, 0) == windows::Win32::Foundation::WAIT_OBJECT_0 }))
}

/// On Unix the standard streams are replaced with /dev/null (other descriptors are close-on-exec)
/// and the server gets its own process group, so it outlives the caller.
#[cfg(unix)]
fn spawn_server() -> Result<Box<dyn FnMut() -> bool>, String> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .args(["serve", "--port", &port().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("could not start the browser: {e}"))?;
    Ok(Box::new(move || !matches!(child.try_wait(), Ok(None))))
}

fn ensure_running() -> Result<(), String> {
    if running() {
        return Ok(());
    }
    let error_file = data_dir().join("serve-error.txt");
    let _ = std::fs::remove_file(&error_file);
    let mut exited = spawn_server()?;
    // Timed by the clock: on Windows a refused connection to localhost takes about 2 s.
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(100));
        if running() {
            return Ok(());
        }
        if exited() {
            // another client may have started the server at the same time
            if running() {
                return Ok(());
            }
            let reason = std::fs::read_to_string(&error_file).unwrap_or_else(|_| "unknown error".into());
            return Err(format!("the browser could not start: {reason}"));
        }
    }
    Err("the browser did not start within 60 s".into())
}

/// Runs one command in the browser window, starting it if needed. Used by the CLI and the MCP server.
pub fn call(cmd: &str, args: &[String]) -> Result<String, String> {
    if cmd == "quit" {
        if running() {
            let _ = request("GET", "/quit", "", Duration::from_secs(20));
        }
        return Ok("closed".into());
    }
    ensure_running()?;
    if cmd == "start" {
        return Ok("running".into());
    }
    let body = serde_json::json!({ "cmd": cmd, "args": args }).to_string();
    let (code, text) = request("POST", "/run", &body, Duration::from_secs(120)).map_err(|e| e.to_string())?;
    if code != 200 {
        return Err(text);
    }
    Ok(text)
}

fn client(cmd: &str, args: &[String]) -> Result<i32, String> {
    if cmd == "install-skill" {
        return install_skill().map(|_| 0);
    }
    match call(cmd, args) {
        Ok(text) => {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{text}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("error: {e}");
            Ok(1)
        }
    }
}

fn install_skill() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut text = SKILL.replace("\r\n", "\n");
    // How the agent should run it later: the npm wrapper says so, otherwise use the full path
    // unless quietbrowser.exe is on PATH.
    let how = match std::env::var("QUIETBROWSER_LAUNCHER") {
        Ok(l) if l.trim() == "quietbrowser" => None,
        Ok(l) if !l.trim().is_empty() => Some(format!("Run it as `{} <command>`.", l.trim())),
        _ => {
            let (sep, name) = if cfg!(windows) { (';', "quietbrowser.exe") } else { (':', "quietbrowser") };
            let on_path = std::env::var("PATH")
                .unwrap_or_default()
                .split(sep)
                .any(|d| !d.trim().is_empty() && std::path::Path::new(d.trim()).join(name).is_file());
            (!on_path).then(|| {
                let p = exe.display();
                if cfg!(windows) {
                    format!("Run it as `& \"{p}\"` in PowerShell or `\"{p}\"` in bash.")
                } else {
                    format!("Run it as `\"{p}\"`.")
                }
            })
        }
    };
    if let (Some(how), Some(i)) = (how, text[3..].find("---")) {
        text.insert_str(i + 6, &format!("\n\nOn this machine `quietbrowser` is not on PATH. {how}\n"));
    }
    let dir = home().join(".claude").join("skills").join("quietbrowser");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join("SKILL.md");
    std::fs::write(&file, text).map_err(|e| e.to_string())?;
    println!("installed {}", file.display());
    Ok(())
}
