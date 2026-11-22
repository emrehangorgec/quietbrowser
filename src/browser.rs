// The browser window: a WebView2 control in a top-level window that never takes focus.
// Everything here runs on the UI thread. Waits pump the message loop, so WebView2 keeps working.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{take_pwstr, CreateCoreWebView2EnvironmentCompletedHandler, CreateCoreWebView2ControllerCompletedHandler, NavigationStartingEventHandler, NavigationCompletedEventHandler, ScriptDialogOpeningEventHandler, NewWindowRequestedEventHandler, DownloadStartingEventHandler};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

use crate::log;

/// State shared by the event handlers and the commands. Borrowed only briefly, never across a wait.
#[derive(Default)]
pub struct Shared {
    pub navigating: Cell<bool>,
    pub navigations: Cell<u32>,
    pub nav_result: Cell<Option<bool>>,
    pub dialogs: RefCell<Vec<String>>,
    pub refs: RefCell<crate::agent::Refs>,
    pub last_snapshot: RefCell<Option<String>>,
}

pub struct Browser {
    pub hwnd: HWND,
    pub controller: ICoreWebView2Controller,
    pub wv: ICoreWebView2,
    pub s: Rc<Shared>,
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

pub fn sleep(ms: u64) -> std::result::Result<(), String> {
    pump(Duration::from_millis(ms), || false).map(|_| ())
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
            if let Some(b) = current() {
                unsafe { let _ = b.controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC); }
            }
        }
        WM_SIZE => {
            if let Some(b) = current() {
                let mut r = RECT::default();
                unsafe {
                    let _ = GetClientRect(hwnd, &mut r);
                    let _ = b.controller.SetBounds(r);
                }
            }
        }
        WM_MOVE => {
            if let Some(b) = current() {
                unsafe { let _ = b.controller.NotifyParentWindowPositionChanged(); }
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
            Some(b) => crate::agent::run(&b, &job.cmd, &job.args),
            None => Err("the browser is not ready".into()),
        };
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
    let controller = {
        let (tx, rx) = mpsc::channel();
        let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |hr, c| {
            let _ = tx.send(hr.and_then(|_| c.ok_or_else(|| Error::from(E_POINTER))));
            Ok(())
        }));
        unsafe { env.CreateCoreWebView2Controller(hwnd, &handler) }.map_err(|e| e.to_string())?;
        wait(&rx, long)?.ok_or("WebView2 did not start")?.map_err(|e| e.to_string())?
    };
    let wv = unsafe { controller.CoreWebView2() }.map_err(|e| e.to_string())?;
    unsafe {
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        controller.SetBounds(r).map_err(|e| e.to_string())?;
        controller.SetIsVisible(true).map_err(|e| e.to_string())?;
        // otherwise WebView2 shows its own modal dialogs
        let settings = wv.Settings().map_err(|e| e.to_string())?;
        settings.SetAreDefaultScriptDialogsEnabled(false).map_err(|e| e.to_string())?;
    }

    let s = Rc::new(Shared::default());
    add_handlers(&wv, &s, hwnd, normal).map_err(|e| e.to_string())?;
    let b = Rc::new(Browser { hwnd, controller, wv, s });
    BROWSER.with(|x| *x.borrow_mut() = Some(b.clone()));
    Ok(b)
}

fn add_handlers(wv: &ICoreWebView2, s: &Rc<Shared>, hwnd: HWND, normal: bool) -> Result<()> {
    let mut token = 0i64;
    unsafe {
        let st = s.clone();
        let hw = hwnd.0 as isize;
        wv.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(move |_, args| {
                st.navigating.set(true);
                st.navigations.set(st.navigations.get() + 1);
                st.refs.borrow_mut().clear();
                *st.last_snapshot.borrow_mut() = None;
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
                st.navigating.set(false);
                st.nav_result.set(Some(ok.as_bool()));
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
                st.dialogs.borrow_mut().push(format!("{name}: {message}"));
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
