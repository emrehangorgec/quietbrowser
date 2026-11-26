// The browser window: a WebView2 control in a top-level window that never takes focus.
// Everything here runs on the UI thread. Waits pump the message loop, so WebView2 keeps working.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{take_pwstr, CallDevToolsProtocolMethodCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler, CreateCoreWebView2ControllerCompletedHandler, NavigationStartingEventHandler, NavigationCompletedEventHandler, ScriptDialogOpeningEventHandler, NewWindowRequestedEventHandler, DownloadStartingEventHandler, DevToolsProtocolEventReceivedEventHandler};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

use crate::engine::{Engine, Nav, Shared};
use crate::log;

/// A tab is a WebView2 control in the window. Only the selected one is visible.
#[derive(Clone)]
struct Tab {
    id: u32,
    controller: ICoreWebView2Controller,
    wv: ICoreWebView2,
    s: Arc<Shared>,
}

pub struct Browser {
    pub hwnd: HWND,
    env: ICoreWebView2Environment,
    normal: bool,
    tabs: RefCell<Vec<Tab>>,
    current: Cell<u32>,
    next_tab: Cell<u32>,
}

impl Browser {
    fn tab(&self) -> std::result::Result<Tab, String> {
        let id = self.current.get();
        self.tabs.borrow().iter().find(|t| t.id == id).cloned().ok_or_else(|| "the tab was closed".to_string())
    }

    fn fit(&self, tab: &Tab) {
        let mut r = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut r);
            let _ = tab.controller.SetBounds(r);
        }
    }
}

impl Engine for Browser {
    fn shared(&self) -> Arc<Shared> {
        self.tab().map(|t| t.s).unwrap_or_default()
    }

    fn cdp_call(&self, method: &str, params: &serde_json::Value, timeout: Duration) -> std::result::Result<serde_json::Value, String> {
        let wv = self.tab()?.wv;
        let (tx, rx) = mpsc::channel();
        let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |hr, json| {
            let _ = tx.send(match hr {
                Ok(()) => Ok(json),
                Err(e) => Err(if json.is_empty() { e.message().to_string() } else { json }),
            });
            Ok(())
        }));
        unsafe {
            wv
                .CallDevToolsProtocolMethod(&HSTRING::from(method), &HSTRING::from(params.to_string()), &handler)
                .map_err(|e| e.to_string())?;
        }
        let Some(result) = wait(&rx, timeout)? else {
            return Err(format!("the page did not answer {method} within {} s", timeout.as_secs()));
        };
        let text = result.map_err(|e| format!("{method} failed: {e}"))?;
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    fn pump(&self, timeout: Duration, done: &mut dyn FnMut() -> bool) -> std::result::Result<bool, String> {
        pump(timeout, done)
    }

    fn go(&self, nav: &Nav) -> std::result::Result<(), String> {
        let wv = self.tab()?.wv;
        unsafe {
            match nav {
                Nav::Url(url) => wv.Navigate(&HSTRING::from(url.as_str())),
                Nav::Back => wv.GoBack(),
                Nav::Reload => wv.Reload(),
            }
        }
        .map_err(|e| e.to_string())
    }

    fn title_url(&self) -> (String, String) {
        self.tab_info(self.current.get())
    }

    fn tabs(&self) -> Vec<u32> {
        self.tabs.borrow().iter().map(|t| t.id).collect()
    }

    fn new_tab(&self) -> std::result::Result<u32, String> {
        let controller = {
            let (tx, rx) = mpsc::channel();
            let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |hr, c| {
                let _ = tx.send(hr.and_then(|_| c.ok_or_else(|| Error::from(E_POINTER))));
                Ok(())
            }));
            unsafe { self.env.CreateCoreWebView2Controller(self.hwnd, &handler) }.map_err(|e| e.to_string())?;
            wait(&rx, Duration::from_secs(60))?.ok_or("WebView2 did not start")?.map_err(|e| e.to_string())?
        };
        let wv = unsafe { controller.CoreWebView2() }.map_err(|e| e.to_string())?;
        unsafe {
            controller.SetIsVisible(false).map_err(|e| e.to_string())?;
            // otherwise WebView2 shows its own modal dialogs
            let settings = wv.Settings().map_err(|e| e.to_string())?;
            settings.SetAreDefaultScriptDialogsEnabled(false).map_err(|e| e.to_string())?;
        }
        let s = Arc::new(Shared::default());
        add_handlers(&wv, &s, self.hwnd, self.normal).map_err(|e| e.to_string())?;
        let id = self.next_tab.get();
        self.next_tab.set(id + 1);
        self.tabs.borrow_mut().push(Tab { id, controller, wv, s });
        self.select_tab(id)?;
        Ok(id)
    }

    fn select_tab(&self, id: u32) -> std::result::Result<(), String> {
        let tabs = self.tabs.borrow().clone();
        let tab = tabs.iter().find(|t| t.id == id).ok_or("the tab was closed")?;
        self.current.set(id);
        self.fit(tab);
        unsafe {
            tab.controller.SetIsVisible(true).map_err(|e| e.to_string())?;
            for other in tabs.iter().filter(|t| t.id != id) {
                let _ = other.controller.SetIsVisible(false);
            }
        }
        Ok(())
    }

    fn close_tab(&self, id: u32) -> std::result::Result<(), String> {
        let tab = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(i) = tabs.iter().position(|t| t.id == id) else { return Ok(()) };
            tabs.remove(i)
        };
        unsafe { tab.controller.Close() }.map_err(|e| e.to_string())
    }

    fn tab_info(&self, id: u32) -> (String, String) {
        let Some(t) = self.tabs.borrow().iter().find(|t| t.id == id).cloned() else { return Default::default() };
        (get_string(|p| unsafe { t.wv.DocumentTitle(p) }), get_string(|p| unsafe { t.wv.Source(p) }))
    }

    /// Flashes the taskbar button. Unlike bringing the window up, this leaves focus where it is.
    fn attention(&self) {
        let info = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd: self.hwnd,
            dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        unsafe {
            let _ = FlashWindowEx(&info);
        }
    }
}

thread_local! {
    static BROWSER: RefCell<Option<Rc<Browser>>> = const { RefCell::new(None) };
    static NORMAL: Cell<bool> = const { Cell::new(false) };
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

pub fn current() -> Option<Rc<Browser>> {
    BROWSER.with(|b| b.borrow().clone())
}

/// Runs the message loop until `done` returns true or the timeout passes. Returns whether `done` was reached.
/// Fails when the window is closing.
pub fn pump(timeout: Duration, mut done: impl FnMut() -> bool) -> std::result::Result<bool, String> {
    let start = Instant::now();
    let mut msg = MSG::default();
    loop {
        if done() {
            return Ok(true);
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Ok(false);
        }
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    PostQuitMessage(msg.wParam.0 as i32);
                    return Err("the browser is closing".into());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
                if done() {
                    return Ok(true);
                }
            }
            let left = (timeout - elapsed).as_millis().min(10) as u32;
            MsgWaitForMultipleObjects(None, false, left.max(1), QS_ALLINPUT);
        }
    }
}

/// Waits for one value from a completion handler.
pub fn wait<T>(rx: &mpsc::Receiver<T>, timeout: Duration) -> std::result::Result<Option<T>, String> {
    let mut out = None;
    pump(timeout, || match rx.try_recv() {
        Ok(v) => {
            out = Some(v);
            true
        }
        Err(_) => false,
    })?;
    Ok(out)
}

fn ex_style(hwnd: HWND) -> u32 {
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 }
}

fn set_no_activate(hwnd: HWND, on: bool) {
    if NORMAL.with(|n| n.get()) {
        return;
    }
    let s = ex_style(hwnd);
    let s = if on { s | WS_EX_NOACTIVATE.0 } else { s & !WS_EX_NOACTIVATE.0 };
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, s as isize) };
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let normal = NORMAL.with(|n| n.get());
    match msg {
        // A real click by the user may activate the window; once it loses activation it goes quiet again.
        WM_MOUSEACTIVATE if !normal => {
            set_no_activate(hwnd, false);
            return LRESULT(MA_ACTIVATE as isize);
        }
        WM_ACTIVATE if !normal && (wparam.0 & 0xFFFF) == 0 => set_no_activate(hwnd, true),
        WM_SETFOCUS => {
            if let Some(t) = current().and_then(|b| b.tab().ok()) {
                unsafe { let _ = t.controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC); }
            }
        }
        WM_SIZE => {
            if let Some(b) = current() {
                for t in b.tabs.borrow().iter() {
                    b.fit(t);
                }
            }
        }
        WM_MOVE => {
            if let Some(b) = current() {
                for t in b.tabs.borrow().iter() {
                    unsafe { let _ = t.controller.NotifyParentWindowPositionChanged(); }
                }
            }
        }
        WM_DPICHANGED => {
            let r = unsafe { &*(lparam.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE);
            }
            return LRESULT(0);
        }
        WM_APP => {
            run_queued();
            return LRESULT(0);
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Runs the commands the HTTP threads queued. A command waits by pumping messages, so a nested
/// WM_APP can arrive while one runs; it is picked up by the outer loop instead.
fn run_queued() {
    if BUSY.with(|b| b.replace(true)) {
        return;
    }
    while let Some(job) = crate::server::next_job() {
        let result = match current() {
            Some(_) if job.cmd == crate::server::QUIT => {
                crate::agent::exit_requested();
                Ok("bye".into())
            }
            Some(b) => crate::agent::dispatch(&*b, &job.session, &job.cmd, &job.args),
            None => Err("the browser is not ready".into()),
        };
        if let Some(b) = current().filter(|_| crate::agent::should_exit()) {
            unsafe { let _ = PostMessageW(Some(b.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)); }
        }
        let _ = job.reply.send(result);
    }
    BUSY.with(|b| b.set(false));
}

fn profile_dir() -> String {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    format!("{base}\\quietbrowser\\profile")
}

pub fn get_string(f: impl FnOnce(*mut PWSTR) -> Result<()>) -> String {
    let mut p = PWSTR::null();
    if f(&mut p).is_err() || p.is_null() {
        return String::new();
    }
    take_pwstr(p)
}

/// Creates the window and the WebView2 control. Returns the window handle.
pub fn create(normal: bool) -> std::result::Result<Rc<Browser>, String> {
    NORMAL.with(|n| n.set(normal));
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let hwnd = unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None).map_err(|e| e.to_string())?.into();
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: w!("quietbrowser"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
        let scale = GetDpiForSystem() as i32;
        let ex = if normal { WINDOW_EX_STYLE(0) } else { WS_EX_NOACTIVATE | WS_EX_APPWINDOW };
        let title = if normal { w!("quietbrowser (normal mode)") } else { w!("quietbrowser") };
        CreateWindowExW(ex, w!("quietbrowser"), title, WS_OVERLAPPEDWINDOW,
            60 * scale / 96, 60 * scale / 96, 1100 * scale / 96, 760 * scale / 96,
            None, None, Some(instance), None)
            .map_err(|e| e.to_string())?
    };
    unsafe {
        let _ = ShowWindow(hwnd, if normal { SW_SHOW } else { SW_SHOWNOACTIVATE });
        let _ = UpdateWindow(hwnd);
    }

    let long = Duration::from_secs(60);
    let env = {
        let (tx, rx) = mpsc::channel();
        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |hr, env| {
            let _ = tx.send(hr.and_then(|_| env.ok_or_else(|| Error::from(E_POINTER))));
            Ok(())
        }));
        let dir = HSTRING::from(profile_dir());
        unsafe { CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &dir, None, &handler) }
            .map_err(|e| format!("WebView2 runtime not found: {e}"))?;
        wait(&rx, long)?.ok_or("WebView2 did not start")?.map_err(|e| e.to_string())?
    };
    let b = Rc::new(Browser {
        hwnd,
        env,
        normal,
        tabs: RefCell::new(Vec::new()),
        current: Cell::new(0),
        next_tab: Cell::new(1),
    });
    BROWSER.with(|x| *x.borrow_mut() = Some(b.clone()));
    b.new_tab()?;
    Ok(b)
}

fn add_handlers(wv: &ICoreWebView2, s: &Arc<Shared>, hwnd: HWND, normal: bool) -> Result<()> {
    let mut token = 0i64;
    unsafe {
        let st = s.clone();
        let hw = hwnd.0 as isize;
        wv.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(move |_, args| {
                st.nav_started();
                if let Some(a) = args {
                    let uri = get_string(|p| a.Uri(p));
                    log(&format!("nav starting {}", uri.chars().take(80).collect::<String>()));
                }
                if normal {
                    let _ = SetForegroundWindow(HWND(hw as *mut _));
                }
                Ok(())
            })),
            &mut token,
        )?;

        let st = s.clone();
        wv.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |_, args| {
                let mut ok = BOOL(1);
                if let Some(a) = args {
                    let _ = a.IsSuccess(&mut ok);
                }
                st.nav_finished(ok.as_bool());
                log(&format!("nav completed ok={}", ok.as_bool()));
                Ok(())
            })),
            &mut token,
        )?;

        let st = s.clone();
        wv.add_ScriptDialogOpening(
            &ScriptDialogOpeningEventHandler::create(Box::new(move |_, args| {
                let Some(a) = args else { return Ok(()) };
                let mut kind = COREWEBVIEW2_SCRIPT_DIALOG_KIND_ALERT;
                a.Kind(&mut kind)?;
                let message = get_string(|p| a.Message(p));
                let name = match kind {
                    COREWEBVIEW2_SCRIPT_DIALOG_KIND_CONFIRM => "confirm",
                    COREWEBVIEW2_SCRIPT_DIALOG_KIND_PROMPT => "prompt",
                    COREWEBVIEW2_SCRIPT_DIALOG_KIND_BEFOREUNLOAD => "beforeunload",
                    _ => "alert",
                };
                st.dialog(format!("{name}: {message}"));
                let answer = if kind == COREWEBVIEW2_SCRIPT_DIALOG_KIND_PROMPT {
                    get_string(|p| a.DefaultText(p))
                } else {
                    String::new()
                };
                a.SetResultText(&HSTRING::from(answer))?;
                a.Accept() // answered in code, no modal dialog
            })),
            &mut token,
        )?;

        // popups open in place
        let view = wv.clone();
        wv.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(a) = args else { return Ok(()) };
                a.SetHandled(true)?;
                let uri = get_string(|p| a.Uri(p));
                view.Navigate(&HSTRING::from(uri))
            })),
            &mut token,
        )?;

        // console messages, errors and requests, read by the console and network commands
        for name in crate::engine::EVENTS {
            let receiver = wv.GetDevToolsProtocolEventReceiver(&HSTRING::from(*name))?;
            let st = s.clone();
            receiver.add_DevToolsProtocolEventReceived(
                &DevToolsProtocolEventReceivedEventHandler::create(Box::new(move |_, args| {
                    if let Some(a) = args {
                        let json = get_string(|p| a.ParameterObjectAsJson(p));
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
                            st.cdp_event(name, &v);
                        }
                    }
                    Ok(())
                })),
                &mut token,
            )?;
        }

        if let Ok(wv4) = wv.cast::<ICoreWebView2_4>() {
            wv4.add_DownloadStarting(
                &DownloadStartingEventHandler::create(Box::new(move |_, args| {
                    if let Some(a) = args {
                        a.SetCancel(true)?;
                    }
                    Ok(())
                })),
                &mut token,
            )?;
        }
    }
    Ok(())
}

/// Runs the message loop until the window closes.
pub fn run_loop() {
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub fn post_to_window(hwnd: isize, msg: u32) {
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(0), LPARAM(0));
    }
}
