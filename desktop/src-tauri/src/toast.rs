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
}

#[cfg(target_os = "macos")]
const OS_TAG: &str = "mac";
#[cfg(not(target_os = "macos"))]
const OS_TAG: &str = "other";

/// Internal: emit content + position right-aligned (bottom on Win/Linux, top on macOS) + show.
fn emit_and_show(app: &AppHandle, mode: &str, label: &str, preview: String) {
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
        },
    );

    // Height depends on the state (no dead space): compact for done/error, medium for
    // processing, full for the copy preview. Width stays as configured.
    let logical_h: f64 = match mode {
        "processing" => 108.0,
        "done" | "error" => 66.0,
        _ => 138.0,
    };
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

    let _ = window.show();
}

/// Show the "text copied" toast with a preview (Open button -> home).
pub fn show(app: &AppHandle, text: &str) {
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

/// Hide the toast.
pub fn hide(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("toast") {
        let _ = window.hide();
    }
}
