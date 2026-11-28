// The browser window: a WebView2 control in a top-level window that never takes focus.
// Everything here runs on the UI thread. Waits pump the message loop, so WebView2 keeps working.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{take_pwstr, CallDevToolsProtocolMethodCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler, CreateCoreWebView2ControllerCompletedHandler, NavigationStartingEventHandler, NavigationCompletedEventHandler, ScriptDialogOpeningEventHandler, NewWindowRequestedEventHandler, DownloadStartingEventHandler, DevToolsProtocolEventReceivedEventHandler};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::GdiPlus as gp;
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
    /// Which agent each tab belongs to, shown in the tab strip.
    labels: RefCell<std::collections::HashMap<u32, String>>,
    /// The tab a command is running in (0 when idle).
    busy: Cell<u32>,
    /// When the busy tab goes back to idle; it stays marked a moment so a short command is seen.
    idle_at: Cell<Option<Instant>>,
    /// Spinner step, advanced by a timer while a tab is busy.
    spin: Cell<u32>,
    /// Agents in the order they first showed up, for their colors.
    agents: RefCell<Vec<String>>,
}

/// One color per agent, in the order they show up.
const AGENT_COLORS: [(u8, u8, u8); 6] =
    [(96, 165, 250), (251, 146, 60), (167, 139, 250), (45, 212, 191), (244, 114, 182), (163, 230, 53)];
const STRIP_BG: (u8, u8, u8) = (24, 25, 29);
const TAB_BG: (u8, u8, u8) = (34, 35, 41);
const TAB_SELECTED: (u8, u8, u8) = (50, 52, 61);
const TAB_BORDER: (u8, u8, u8) = (72, 75, 86);
const WORKING: (u8, u8, u8) = (74, 222, 128);

/// Height of the tab strip above the page, in pixels at 96 DPI.
const STRIP: i32 = 40;
const TAB_WIDTH: i32 = 250;
/// Timer that turns the spinner while a tab is busy.
const SPIN_TIMER: usize = 1;

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

fn rgb3((r, g, b): (u8, u8, u8)) -> COLORREF {
    rgb(r, g, b)
}

/// A GDI+ color.
fn argb(a: u8, (r, g, b): (u8, u8, u8)) -> u32 {
    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// The color mixed with white, for text on the dark strip.
fn lighten((r, g, b): (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let m = |c: u8| (c as f32 + (255.0 - c as f32) * t) as u8;
    (m(r), m(g), m(b))
}

/// Anti-aliased shapes for the strip, drawn with GDI+ on a memory DC.
struct Canvas(*mut gp::GpGraphics);

impl Canvas {
    unsafe fn new(hdc: HDC) -> Canvas {
        let mut g = std::ptr::null_mut();
        unsafe {
            gp::GdipCreateFromHDC(hdc, &mut g);
            gp::GdipSetSmoothingMode(g, gp::SmoothingModeAntiAlias);
            gp::GdipSetPixelOffsetMode(g, gp::PixelOffsetModeHalf);
        }
        Canvas(g)
    }

    fn round_rect(&self, x: f32, y: f32, w: f32, h: f32, r: f32, fill: u32, border: Option<u32>) {
        unsafe {
            let mut path = std::ptr::null_mut();
            gp::GdipCreatePath(gp::FillModeAlternate, &mut path);
            let d = r * 2.0;
            gp::GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
            gp::GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
            gp::GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
            gp::GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
            gp::GdipClosePathFigure(path);
            let mut brush = std::ptr::null_mut();
            gp::GdipCreateSolidFill(fill, &mut brush);
            gp::GdipFillPath(self.0, brush as *mut gp::GpBrush, path);
            gp::GdipDeleteBrush(brush as *mut gp::GpBrush);
            if let Some(color) = border {
                let mut pen = std::ptr::null_mut();
                gp::GdipCreatePen1(color, 1.0, gp::UnitPixel, &mut pen);
                gp::GdipDrawPath(self.0, pen, path);
                gp::GdipDeletePen(pen);
            }
            gp::GdipDeletePath(path);
        }
    }

    fn dot(&self, cx: f32, cy: f32, d: f32, color: u32) {
        unsafe {
            let mut brush = std::ptr::null_mut();
            gp::GdipCreateSolidFill(color, &mut brush);
            gp::GdipFillEllipse(self.0, brush as *mut gp::GpBrush, cx - d / 2.0, cy - d / 2.0, d, d);
            gp::GdipDeleteBrush(brush as *mut gp::GpBrush);
        }
    }

    fn arc(&self, cx: f32, cy: f32, d: f32, width: f32, start: f32, sweep: f32, color: u32) {
        unsafe {
            let mut pen = std::ptr::null_mut();
            gp::GdipCreatePen1(color, width, gp::UnitPixel, &mut pen);
            gp::GdipSetPenStartCap(pen, gp::LineCapRound);
            gp::GdipSetPenEndCap(pen, gp::LineCapRound);
            gp::GdipDrawArc(self.0, pen, cx - d / 2.0, cy - d / 2.0, d, d, start, sweep);
            gp::GdipDeletePen(pen);
        }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            gp::GdipDeleteGraphics(self.0);
        }
    }
}

/// The app icon, drawn here so the exe needs no resource file: a dark rounded window with
/// three dots in the first agent colors over a page, for several agents sharing one browser.
fn app_icon(size: i32) -> Option<HICON> {
    const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x26200A;
    let f = size as f32;
    unsafe {
        let mut bmp = std::ptr::null_mut();
        if gp::GdipCreateBitmapFromScan0(size, size, 0, PIXEL_FORMAT_32BPP_ARGB, None, &mut bmp) != gp::Ok {
            return None;
        }
        let mut g = std::ptr::null_mut();
        gp::GdipGetImageGraphicsContext(bmp as *mut gp::GpImage, &mut g);
        gp::GdipSetSmoothingMode(g, gp::SmoothingModeAntiAlias);
        gp::GdipSetPixelOffsetMode(g, gp::PixelOffsetModeHalf);
        gp::GdipGraphicsClear(g, 0);
        {
            let c = Canvas(g);
            c.round_rect(0.5, 0.5, f - 1.0, f - 1.0, f * 0.24, argb(255, STRIP_BG), Some(argb(255, TAB_BORDER)));
            c.round_rect(f * 0.16, f * 0.58, f * 0.68, f * 0.24, f * 0.07, argb(255, TAB_SELECTED), None);
            for (i, &color) in AGENT_COLORS[..3].iter().enumerate() {
                c.dot(f * (0.27 + 0.23 * i as f32), f * 0.35, f * 0.21, argb(255, color));
            }
        }
        let mut icon = HICON::default();
        let ok = gp::GdipCreateHICONFromBitmap(bmp, &mut icon) == gp::Ok;
        gp::GdipDisposeImage(bmp as *mut gp::GpImage);
        ok.then_some(icon)
    }
}

fn set_icons(hwnd: HWND) {
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    for (kind, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
        let size = unsafe { GetSystemMetricsForDpi(metric, dpi) };
        if let Some(icon) = app_icon(size) {
            unsafe {
                SendMessageW(hwnd, WM_SETICON, Some(WPARAM(kind as usize)), Some(LPARAM(icon.0 as isize)));
            }
        }
    }
}

/// Starts GDI+ once for the process.
fn start_gdiplus() {
    let input = gp::GdiplusStartupInput { GdiplusVersion: 1, ..Default::default() };
    let mut token = 0usize;
    unsafe {
        gp::GdiplusStartup(&mut token, &input, std::ptr::null_mut());
    }
}

/// A dark title bar that matches the strip (Windows 10 20H1 and later; the caption color needs Windows 11).
fn dark_title_bar(hwnd: HWND) {
    let on: BOOL = true.into();
    let caption = rgb3(STRIP_BG);
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as _, 4);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &caption as *const _ as _, 4);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &caption as *const _ as _, 4);
    }
}

fn scale(hwnd: HWND, v: i32) -> i32 {
    v * unsafe { GetDpiForWindow(hwnd) }.max(96) as i32 / 96
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
        }
        r.top = (r.top + scale(self.hwnd, STRIP)).min(r.bottom);
        unsafe {
            let _ = tab.controller.SetBounds(r);
        }
    }

    fn redraw_strip(&self) {
        let mut r = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut r);
            r.bottom = scale(self.hwnd, STRIP);
            let _ = InvalidateRect(Some(self.hwnd), Some(&r), false);
        }
    }

    /// The tab rectangles of the strip, left to right, with each tab's id.
    fn strip_layout(&self) -> Vec<(u32, RECT)> {
        let mut client = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut client);
        }
        let ids: Vec<u32> = self.tabs.borrow().iter().map(|t| t.id).collect();
        if ids.is_empty() {
            return Vec::new();
        }
        let s = |v| scale(self.hwnd, v);
        let (pad, gap) = (s(8), s(6));
        let avail = (client.right - pad * 2).max(1);
        let w = s(TAB_WIDTH).min(avail / ids.len() as i32);
        ids.iter()
            .enumerate()
            .map(|(i, &id)| {
                let left = pad + w * i as i32;
                (id, RECT { left, top: s(6), right: left + w - gap, bottom: s(STRIP) - s(6) })
            })
            .collect()
    }

    /// Draws the tab strip: one rounded tab per page, named after the agent that owns it in the
    /// agent's color, with a spinner and "working" while a command runs in it. Painted off screen
    /// first so it does not flicker.
    fn paint_strip(&self, hdc: HDC) {
        let hwnd = self.hwnd;
        let s = |v| scale(hwnd, v);
        let mut client = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut client);
        }
        let (w, h) = (client.right.max(1), s(STRIP));
        let current = self.current.get();
        let busy = self.busy.get();
        unsafe {
            let mem = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, w, h);
            let old_bmp = SelectObject(mem, bmp.into());
            let brush = CreateSolidBrush(rgb3(STRIP_BG));
            FillRect(mem, &RECT { left: 0, top: 0, right: w, bottom: h }, brush);
            let _ = DeleteObject(brush.into());
            let font = |size: i32, weight: i32| {
                CreateFontW(-s(size), 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
                    CLEARTYPE_QUALITY, (DEFAULT_PITCH.0 | FF_SWISS.0) as u32, w!("Segoe UI"))
            };
            let (bold, normal, small) = (font(13, 600), font(13, 400), font(12, 600));
            let old_font = SelectObject(mem, bold.into());
            SetBkMode(mem, TRANSPARENT);
            let labels = self.labels.borrow();
            let agents = self.agents.borrow();
            let flags = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX;
            for (id, r) in self.strip_layout() {
                let selected = id == current;
                let label = labels.get(&id).cloned().unwrap_or_default();
                let color = match agents.iter().position(|a| *a == label) {
                    Some(i) if !label.is_empty() => AGENT_COLORS[i % AGENT_COLORS.len()],
                    _ => (148, 152, 163),
                };
                let (cx, cy) = ((r.left + s(16)) as f32, (r.top + r.bottom) as f32 / 2.0);
                {
                    let c = Canvas::new(mem);
                    let (x, y) = (r.left as f32, r.top as f32);
                    let (rw, rh) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
                    if selected {
                        c.round_rect(x, y, rw, rh, s(8) as f32, argb(255, TAB_SELECTED), Some(argb(255, TAB_BORDER)));
                    } else {
                        c.round_rect(x, y, rw, rh, s(8) as f32, argb(255, TAB_BG), None);
                    }
                    if id == busy {
                        let d = s(11) as f32;
                        c.arc(cx, cy, d, s(2) as f32, 0.0, 360.0, argb(60, color));
                        c.arc(cx, cy, d, s(2) as f32, (self.spin.get() * 36 % 360) as f32, 100.0, argb(255, color));
                    } else {
                        c.dot(cx, cy, s(8) as f32, argb(255, color));
                    }
                }

                let mut text_rect = RECT { left: r.left + s(30), top: r.top, right: r.right - s(10), bottom: r.bottom };
                if id == busy {
                    // "working" at the right end while a command runs in this tab
                    SelectObject(mem, small.into());
                    SetTextColor(mem, rgb3(WORKING));
                    let mut text: Vec<u16> = "working".encode_utf16().collect();
                    let mut busy_rect = text_rect;
                    DrawTextW(mem, &mut text, &mut busy_rect, flags | DT_RIGHT);
                    let mut measure = text_rect;
                    DrawTextW(mem, &mut text, &mut measure, flags | DT_CALCRECT);
                    text_rect.right -= (measure.right - measure.left) + s(10);
                }
                if !label.is_empty() {
                    SelectObject(mem, bold.into());
                    SetTextColor(mem, rgb3(lighten(color, 0.25)));
                    let mut text: Vec<u16> = label.encode_utf16().collect();
                    let mut measure = text_rect;
                    DrawTextW(mem, &mut text, &mut measure, flags | DT_CALCRECT);
                    let width = (measure.right - measure.left).min((text_rect.right - text_rect.left) * 2 / 3);
                    let mut label_rect = RECT { right: text_rect.left + width, ..text_rect };
                    DrawTextW(mem, &mut text, &mut label_rect, flags);
                    text_rect.left += width + s(8);
                }
                SelectObject(mem, normal.into());
                SetTextColor(mem, if selected { rgb(226, 228, 233) } else { rgb(138, 142, 153) });
                let (title, _) = self.tab_info(id);
                let mut text: Vec<u16> = (if title.is_empty() { "New tab".to_string() } else { title }).encode_utf16().collect();
                if text_rect.left < text_rect.right {
                    DrawTextW(mem, &mut text, &mut text_rect, flags);
                }
            }
            let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old_font);
            for f in [bold, normal, small] {
                let _ = DeleteObject(f.into());
            }
            SelectObject(mem, old_bmp);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
        }
    }

    /// Turns the spinner; once the busy tab has been idle a moment, stops the timer.
    fn tick(&self) {
        self.spin.set(self.spin.get().wrapping_add(1));
        if self.idle_at.get().is_some_and(|t| Instant::now() >= t) {
            self.idle_at.set(None);
            self.busy.set(0);
            unsafe {
                let _ = KillTimer(Some(self.hwnd), SPIN_TIMER);
            }
        }
        self.redraw_strip();
    }

    /// A click on a tab in the strip shows that tab. Agents keep working in their own tabs.
    fn click_strip(&self, x: i32, y: i32) {
        if y >= scale(self.hwnd, STRIP) {
            return;
        }
        let hit = self.strip_layout().into_iter().find(|(_, r)| x >= r.left && x < r.right).map(|(id, _)| id);
        log(&format!("strip click at {x},{y}: tab {hit:?}"));
        if let Some(id) = hit {
            let _ = self.select_tab(id);
        }
    }
}

impl Engine for Browser {
    fn shared(&self) -> Arc<Shared> {
        self.tab().map(|t| t.s).unwrap_or_default()
    }

    fn cdp_call(&self, method: &str, params: &serde_json::Value, timeout: Duration) -> std::result::Result<serde_json::Value, String> {
        self.cdp_call_for(self.current.get(), method, params, timeout)
    }

    fn cdp_call_for(&self, id: u32, method: &str, params: &serde_json::Value, timeout: Duration) -> std::result::Result<serde_json::Value, String> {
        let wv = self.tabs.borrow().iter().find(|t| t.id == id).map(|t| t.wv.clone()).ok_or("the tab was closed")?;
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
        self.redraw_strip();
        Ok(())
    }

    fn close_tab(&self, id: u32) -> std::result::Result<(), String> {
        let tab = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(i) = tabs.iter().position(|t| t.id == id) else { return Ok(()) };
            tabs.remove(i)
        };
        self.labels.borrow_mut().remove(&id);
        self.redraw_strip();
        unsafe { tab.controller.Close() }.map_err(|e| e.to_string())
    }

    fn label_tab(&self, id: u32, label: &str) {
        let changed = self.labels.borrow_mut().insert(id, label.to_string()).as_deref() != Some(label);
        let mut agents = self.agents.borrow_mut();
        if !agents.iter().any(|a| a == label) {
            agents.push(label.to_string());
        }
        drop(agents);
        if changed {
            self.redraw_strip();
        }
    }

    fn set_busy(&self, id: Option<u32>) {
        match id {
            Some(id) => {
                self.busy.set(id);
                self.idle_at.set(None);
                unsafe {
                    SetTimer(Some(self.hwnd), SPIN_TIMER, 50, None);
                }
            }
            None => self.idle_at.set(Some(Instant::now() + Duration::from_millis(450))),
        }
        self.redraw_strip();
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
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                if let Some(b) = current() {
                    b.paint_strip(hdc);
                }
                let _ = EndPaint(hwnd, &ps);
            }
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1), // the strip and the pages cover the whole window
        WM_TIMER if wparam.0 == SPIN_TIMER => {
            if let Some(b) = current() {
                b.tick();
            }
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            if let Some(b) = current() {
                let (x, y) = ((lparam.0 & 0xFFFF) as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as i16 as i32);
                b.click_strip(x, y);
            }
            return LRESULT(0);
        }
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
    start_gdiplus();
    dark_title_bar(hwnd);
    set_icons(hwnd);
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
        labels: RefCell::new(Default::default()),
        busy: Cell::new(0),
        idle_at: Cell::new(None),
        spin: Cell::new(0),
        agents: RefCell::new(Vec::new()),
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
                // the strip shows the new title
                let _ = InvalidateRect(Some(HWND(hw as *mut _)), None, false);
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
