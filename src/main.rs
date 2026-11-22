// quietbrowser: a browser for coding agents on Windows that never steals focus.
// One binary. `quietbrowser serve` runs the window and its local HTTP API; every other command is a
// client that sends one command to it and prints the answer, starting the window first if needed.
mod agent;
mod browser;
mod mcp;
mod server;

use std::io::{Read, Write};
use std::net::TcpStream;
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
Port: QUIETBROWSER_PORT (default 9555).";

const SKILL: &str = include_str!("../skill/SKILL.md");

pub fn log(line: &str) {
    if std::env::var("QUIETBROWSER_LOG").as_deref() != Ok("1") {
        return;
    }
    let dir = format!("{}\\quietbrowser", std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into()));
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(format!("{dir}\\log.txt")) {
        let _ = writeln!(f, "{} {line}", timestamp());
    }
}

/// Local time as yyyyMMdd-HHmmss.
pub fn timestamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
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

/// `serve [--port N] [--normal]`. --normal turns the focus protections off and activates on every
/// navigation (the control run of the focus test).
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
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
    }
    let b = match browser::create(normal) {
        Ok(b) => b,
        Err(e) => {
            log(&format!("start failed: {e}"));
            eprintln!("error: {e}");
            return 1;
        }
    };
    if let Err(e) = agent::init(&b) {
        log(&format!("init failed: {e}"));
    }
    if let Err(e) = server::start(port, b.hwnd.0 as isize) {
        log(&format!("port {port}: {e}"));
        eprintln!("error: port {port}: {e}");
        return 1;
    }
    log(&format!("serving on {port}"));
    browser::run_loop();
    0
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
fn spawn_server() -> Result<(), String> {
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
        let _ = windows::Win32::Foundation::CloseHandle(pi.hProcess);
    }
    Ok(())
}

fn ensure_running() -> Result<(), String> {
    if running() {
        return Ok(());
    }
    spawn_server()?;
    for _ in 0..300 {
        std::thread::sleep(Duration::from_millis(100));
        if running() {
            return Ok(());
        }
    }
    Err("the browser did not start within 30 s".into())
}

/// Runs one command in the browser window, starting it if needed. Used by the CLI and the MCP server.
pub fn call(cmd: &str, args: &[String]) -> Result<String, String> {
    if cmd == "quit" {
        if running() {
            let _ = request("GET", "/quit", "", Duration::from_secs(5));
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
    let on_path = std::env::var("PATH")
        .unwrap_or_default()
        .split(';')
        .any(|d| !d.trim().is_empty() && std::path::Path::new(d.trim()).join("quietbrowser.exe").is_file());
    let mut text = SKILL.replace("\r\n", "\n");
    if !on_path {
        // Not on PATH: tell the agent the full path on this machine.
        if let Some(i) = text[3..].find("---") {
            let end = i + 6;
            let note = format!(
                "\n\nOn this machine the CLI is not on PATH. Run it as `& \"{0}\"` in PowerShell or `\"{0}\"` in bash.\n",
                exe.display()
            );
            text.insert_str(end, &note);
        }
    }
    let home = std::env::var("USERPROFILE").map_err(|e| e.to_string())?;
    let dir = std::path::Path::new(&home).join(".claude").join("skills").join("quietbrowser");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join("SKILL.md");
    std::fs::write(&file, text).map_err(|e| e.to_string())?;
    println!("installed {}", file.display());
    Ok(())
}
