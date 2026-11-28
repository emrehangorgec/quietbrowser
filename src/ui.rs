// Browser-based controls for one local browser instance. All input is routed to an owned tab.
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::engine::{Engine, R};

pub const HTML: &str = include_str!("../ui/index.html");
pub const JS: &str = include_str!("../ui/app.js");
pub const CSS: &str = include_str!("../ui/app.css");

static INSTANCE: LazyLock<String> = LazyLock::new(|| {
    let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("{}-{time}", std::process::id())
});
static TABS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

pub fn publish(tabs: Vec<Value>) {
    *TABS.lock().unwrap() = tabs;
}

pub fn state() -> String {
    json!({ "instance": &*INSTANCE, "tabs": *TABS.lock().unwrap() }).to_string()
}

pub fn is_current(request: &Value) -> bool {
    request["instance"].as_str() == Some(INSTANCE.as_str())
}

fn call(b: &dyn Engine, id: u32, method: &str, params: Value) -> R<Value> {
    b.cdp_call_for(id, method, &params, Duration::from_secs(5))
}

pub fn preview(b: &dyn Engine, id: u32) -> R<Value> {
    let shot = call(b, id, "Page.captureScreenshot", json!({ "format": "jpeg", "quality": 60 }))?;
    let data = shot["data"].as_str().ok_or("The tab did not produce a preview.")?;
    Ok(json!({ "image": data }))
}

fn point(b: &dyn Engine, id: u32, request: &Value) -> R<(f64, f64)> {
    let fraction = |key: &str| request[key].as_f64().filter(|v| (0.0..1.0).contains(v));
    let x = fraction("x").ok_or("Click inside the preview.")?;
    let y = fraction("y").ok_or("Click inside the preview.")?;
    let metrics = call(b, id, "Page.getLayoutMetrics", json!({}))?;
    let viewport = &metrics["cssVisualViewport"];
    let w = viewport["clientWidth"].as_f64().filter(|v| *v > 0.0).ok_or("The tab has no viewport.")?;
    let h = viewport["clientHeight"].as_f64().filter(|v| *v > 0.0).ok_or("The tab has no viewport.")?;
    Ok((x * w, y * h))
}

pub fn input(b: &dyn Engine, id: u32, action: &str, request: &Value) -> R<()> {
    match action {
        "click" => {
            let (x, y) = point(b, id, request)?;
            for kind in ["mousePressed", "mouseReleased"] {
                call(b, id, "Input.dispatchMouseEvent", json!({
                    "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1
                }))?;
            }
        }
        "scroll" => {
            let (x, y) = point(b, id, request)?;
            let delta = |key: &str| request[key].as_f64().unwrap_or_default().clamp(-1500.0, 1500.0);
            call(b, id, "Input.dispatchMouseEvent", json!({
                "type": "mouseWheel", "x": x, "y": y, "deltaX": delta("dx"), "deltaY": delta("dy")
            }))?;
        }
        "text" => {
            let text = request["text"].as_str().filter(|s| s.len() <= 16384).ok_or("Invalid text.")?;
            call(b, id, "Input.insertText", json!({ "text": text }))?;
        }
        "key" => {
            let key = request["key"].as_str().ok_or("Missing key.")?;
            let vk = match key {
                "Backspace" => 8, "Tab" => 9, "Enter" => 13, "Escape" => 27,
                "PageUp" => 33, "PageDown" => 34, "End" => 35, "Home" => 36,
                "ArrowLeft" => 37, "ArrowUp" => 38, "ArrowRight" => 39, "ArrowDown" => 40,
                "Delete" => 46, "a" => 65,
                _ => return Err("Unsupported key.".into()),
            };
            let modifiers = if key == "a" { if request["meta"] == true { 4 } else { 2 } } else if request["shift"] == true { 8 } else { 0 };
            let kind = if key == "Enter" { "keyDown" } else { "rawKeyDown" };
            let mut down = json!({ "type": kind, "key": key, "windowsVirtualKeyCode": vk, "modifiers": modifiers });
            if key == "Enter" {
                down["text"] = json!("\r");
            }
            call(b, id, "Input.dispatchKeyEvent", down)?;
            call(b, id, "Input.dispatchKeyEvent", json!({
                "type": "keyUp", "key": key, "windowsVirtualKeyCode": vk, "modifiers": modifiers
            }))?;
        }
        _ => return Err("Unknown action.".into()),
    }
    Ok(())
}
