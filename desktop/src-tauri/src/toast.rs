//! Notification window (toast) in the bottom-right corner.
//!
//! The "toast" window is pre-created hidden in `tauri.conf.json`
//! (frameless, always-on-top, transparent, skip-taskbar). Here we position it
//! on the current monitor and pass it the preview of the copied text.
//! Auto-hide (~6s) and the buttons are handled in `toast.html`.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition};

const MARGIN: f64 = 16.0;
const PREVIEW_CHARS: usize = 160;

#[derive(Clone, Serialize)]
struct ToastContent {
    preview: String,
    /// Current shortcut, so the toast always shows the up-to-date one
    /// (the toast window does not reload: it must be passed on every show).
    hotkey: String,
    /// Current theme, for visual consistency with the rest of the app.
    theme: String,
    /// Name of the default provider chosen in Settings (where "Open" will open).
    provider: String,
    /// Current UI language (code, e.g. "it"): the toast localizes its own texts.
    language: String,
}

/// Display name of the provider from the key saved in Settings.
fn provider_name(key: &str) -> &str {
    match key {
        "openai" => "ChatGPT",
        "anthropic" => "Claude",
        "grok" => "Grok",
        "gemini" => "Gemini",
        "perplexity" => "Perplexity",
        "qwen" => "Qwen",
        "deepseek" => "DeepSeek",
        "zai" => "Z.ai",
        _ => "",
    }
}

/// Show the toast with a preview of the copied text.
pub fn show(app: &AppHandle, text: &str) {
    let Some(window) = app.get_webview_window("toast") else {
        return;
    };

    let (hotkey, theme, provider, language) = {
        let s = app.state::<crate::AppState>();
        let g = s.settings.lock().unwrap();
        (
            g.hotkey.clone(),
            g.theme.clone(),
            provider_name(&g.default_provider).to_string(),
            g.language.clone(),
        )
    };

    let mut preview: String = text.chars().take(PREVIEW_CHARS).collect();
    if text.chars().count() > PREVIEW_CHARS {
        preview.push('…');
    }
    let _ = window.emit(
        "toast://content",
        ToastContent {
            preview,
            hotkey,
            theme,
            provider,
            language,
        },
    );

    // Bottom-right of the monitor WORK AREA (excludes the taskbar), so the toast
    // sits right next to the taskbar like a native notification.
    if let Ok(Some(monitor)) = window.current_monitor() {
        let scale = monitor.scale_factor();
        let wa = monitor.work_area(); // physical px, taskbar already excluded
        if let Ok(win_size) = window.outer_size() {
            let margin = (MARGIN * scale) as i32;
            let x = wa.position.x + wa.size.width as i32 - win_size.width as i32 - margin;
            let y = wa.position.y + wa.size.height as i32 - win_size.height as i32 - margin;
            let _ = window.set_position(PhysicalPosition::new(x, y));
        }
    }

    let _ = window.show();
}

/// Hide the toast.
pub fn hide(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("toast") {
        let _ = window.hide();
    }
}
