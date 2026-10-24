//! Notification window (toast), right-aligned on the current monitor:
//! bottom-right on Windows/Linux (next to the taskbar), top-right on macOS
//! (next to the menu bar / Notification Center).
//!
//! The "toast" window is pre-created hidden in `tauri.conf.json`
//! (frameless, always-on-top, transparent, skip-taskbar). Here we position it
//! on the current monitor and pass it the preview of the copied text.
//! Auto-hide (~6s) and the buttons are handled in `toast.html`.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize};

const PREVIEW_CHARS: usize = 160;
const READY_PREVIEW_CHARS: usize = 240;

#[derive(Clone, Serialize)]
struct ToastContent {
    /// Toast state: "copy" (text copied -> Open button), "processing" (inline transform
    /// running: spinner + recipe name), "done" (pasted ok), "error" (message).
    mode: String,
    /// Free label for the non-copy modes (recipe name / error message).
    label: String,
    preview: String,
    /// Current shortcut, so the toast always shows the up-to-date one
    /// (the toast window does not reload: it must be passed on every show).
    hotkey: String,
    /// Current theme, for visual consistency with the rest of the app.
    theme: String,
    /// Current UI language (code, e.g. "it"): the toast localizes its own texts.
    language: String,
    /// "mac" | "other": tells the frontend which corner is flush against the screen edge
    /// (top on macOS, bottom elsewhere) so it can flip the card's rounded corners to match.
    os: String,
    /// Readable name of the window a pending result belongs to ("ready" / "closed" modes).
    target: String,
}

#[cfg(target_os = "macos")]
const OS_TAG: &str = "mac";
#[cfg(not(target_os = "macos"))]
const OS_TAG: &str = "other";

/// Internal: emit content + position right-aligned (bottom on Win/Linux, top on macOS) + show.
fn emit_and_show(app: &AppHandle, mode: &str, label: &str, preview: String) {
    emit_and_show_full(app, mode, label, preview, String::new());
}

fn emit_and_show_full(app: &AppHandle, mode: &str, label: &str, preview: String, target: String) {
    let Some(window) = app.get_webview_window("toast") else {
        return;
    };
    let (hotkey, theme, language) = {
        let s = app.state::<crate::AppState>();
        let g = s.settings.lock().unwrap();
        (g.hotkey.clone(), g.theme.clone(), g.language.clone())
    };
    let _ = window.emit(
        "toast://content",
        ToastContent {
            mode: mode.into(),
            label: label.into(),
            preview,
            hotkey,
            theme,
            language,
            os: OS_TAG.into(),
            target,
        },
    );

    // Height depends on the state (no dead space): compact for done/error, medium for
    // processing, full for the copy preview. Width stays as configured.
    let logical_h: f64 = match mode {
        "processing" => 108.0,
        "done" | "error" => 66.0,
        "ready" => 260.0,
        "closed" => 246.0,
        _ => 138.0,
    };
    place_with_height(&window, logical_h);
    show_without_stealing_focus(&window);
}

/// Safety limits (logical px) for the height the page asks for via `toast_fit`.
const FIT_MIN_H: f64 = 120.0;
const FIT_MAX_H: f64 = 480.0;

/// Pure: clamp a measured content height to the allowed range (NaN falls back to the minimum).
pub fn clamp_toast_height(h: f64) -> f64 {
    if h.is_nan() {
        return FIT_MIN_H;
    }
    h.clamp(FIT_MIN_H, FIT_MAX_H)
}

/// Resize the toast to the height measured by the page ("ready"/"closed" states), keeping the
/// width and the anchored edge. No-op when the size would not change; never touches the focus.
pub fn fit(app: &AppHandle, logical_h: f64) {
    let Some(window) = app.get_webview_window("toast") else {
        return;
    };
    let h = clamp_toast_height(logical_h);
    let scale = window.scale_factor().unwrap_or(1.0);
    let phys_h = (h * scale).round() as u32;
    if window.outer_size().map(|s| s.height == phys_h).unwrap_or(false) {
        return;
    }
    place_with_height(&window, h);
}

/// Set the window height (logical px), keep its width and anchor it to the right edge of the work
/// area (bottom on Windows/Linux, top on macOS).
fn place_with_height(window: &tauri::WebviewWindow, logical_h: f64) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let cur_w = window
        .outer_size()
        .map(|s| s.width)
        .unwrap_or((372.0 * scale) as u32);
    let phys_h = (logical_h * scale).round() as u32;
    let _ = window.set_size(PhysicalSize::new(cur_w, phys_h));

    // Right edge of the monitor WORK AREA (excludes taskbar/menu bar) on every platform --
    // the vertical edge flips per-OS to match where each system's own notifications/tray
    // live: bottom-right on Windows (next to the taskbar), TOP-right on macOS (the menu
    // bar -- and thus Notification Center -- sits at the top, not the bottom, there).
    if let Ok(Some(monitor)) = window.current_monitor() {
        let wa = monitor.work_area(); // physical px, taskbar/menu bar already excluded
        let x = wa.position.x + wa.size.width as i32 - cur_w as i32;
        #[cfg(target_os = "macos")]
        let y = wa.position.y;
        #[cfg(not(target_os = "macos"))]
        let y = wa.position.y + wa.size.height as i32 - phys_h as i32;
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }
}

/// Show the toast WITHOUT letting it take the foreground.
///
/// `focus: false` in tauri.conf only applies when the window is CREATED; every later show goes
/// through ShowWindow(SW_SHOW), which by definition activates the window. For a notification that
/// is fatal: the inline transform fires while the user is typing in another app, and stealing the
/// foreground there means the result gets pasted into the wrong window -- measured with Notepad,
/// where the rewritten text never came back.
///
/// The fix is WS_EX_NOACTIVATE on the window itself, NOT a raw SW_SHOWNOACTIVATE in place of
/// Tauri's `show()`. Tauri's show does more than ShowWindow: it also tells the WebView2 controller
/// it is visible. Bypassing it left the webview considered hidden, so its JavaScript timers never
/// ran -- and the timer is exactly what closes the toast, which then stayed on screen for good
/// (reported, and reproduced). With the extended style set, `show()` displays the window and leaves
/// the foreground where it was. Mouse input still reaches it, so the Open/close buttons keep
/// working; only keyboard activation is refused.
fn show_without_stealing_focus(window: &tauri::WebviewWindow) {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
        };
        if let Ok(h) = window.hwnd() {
            unsafe {
                let hwnd = HWND(h.0 as *mut core::ffi::c_void);
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                let want = ex | (WS_EX_NOACTIVATE.0 as isize);
                if ex != want {
                    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
                }
            }
        }
    }
    let _ = window.show();
}

/// Show the "text copied" toast with a preview (Open button -> home).
pub fn show(app: &AppHandle, text: &str) {
    // A "ready"/"closed" toast still waiting for the user must not be replaced by the copy toast
    // (its buttons would vanish while the result lives only in memory). Pending exists on Windows only.
    if app.state::<crate::AppState>().pending_paste.lock().unwrap().is_some() {
        return;
    }
    let mut preview: String = text.chars().take(PREVIEW_CHARS).collect();
    if text.chars().count() > PREVIEW_CHARS {
        preview.push('…');
    }
    emit_and_show(app, "copy", "", preview);
}

/// Show a STATE toast (inline transform): mode = "processing" | "done" | "error",
/// label = recipe name or error message key resolved by the frontend.
pub fn show_state(app: &AppHandle, mode: &str, label: &str) {
    emit_and_show(app, mode, label, String::new());
}

/// Show the "result ready" toast: the answer was NOT pasted and waits for the user.
/// `mode` is "ready" or "closed"; it stays on screen until the user acts or closes it.
/// `label` is "" or "refused" (the last attempt to switch window was refused).
pub fn show_ready(app: &AppHandle, mode: &str, label: &str, result: &str, target: &str) {
    let mut preview: String = result.chars().take(READY_PREVIEW_CHARS).collect();
    if result.chars().count() > READY_PREVIEW_CHARS {
        preview.push('…');
    }
    emit_and_show_full(app, mode, label, preview, target.to_string());
}

/// Hide the toast.
pub fn hide(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("toast") {
        let _ = window.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::clamp_toast_height;

    #[test]
    fn clamps_height() {
        assert_eq!(clamp_toast_height(50.0), 120.0);
        assert_eq!(clamp_toast_height(200.0), 200.0);
        assert_eq!(clamp_toast_height(900.0), 480.0);
        assert_eq!(clamp_toast_height(f64::NAN), 120.0);
    }
}
