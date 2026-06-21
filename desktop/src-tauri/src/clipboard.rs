//! Feature 2 — global clipboard monitor.
//!
//! The monitor is started on the Rust side (robust even when the window is
//! hidden) and the raw `clipboard-monitor/update` event is listened to here. On
//! every change we read the text, apply ignore-self (so we don't auto-trigger
//! when the app itself writes the prompt) and dedup, then show the toast.

use crate::{toast, AppState};
use tauri::{AppHandle, Listener, Manager};
use tauri_plugin_clipboard::Clipboard;

const UPDATE_EVENT: &str = "plugin:clipboard://clipboard-monitor/update";

/// Start the monitor and register the Rust listener.
pub fn start(app: &AppHandle) {
    let clipboard = app.state::<Clipboard>();
    if let Err(e) = clipboard.start_monitor(app.clone()) {
        eprintln!("[clipboard] start_monitor fallito: {e}");
    }

    let handle = app.clone();
    app.listen_any(UPDATE_EVENT, move |_event| {
        on_update(&handle);
    });
}

fn on_update(app: &AppHandle) {
    let state = app.state::<AppState>();

    // Monitor disabled in the settings?
    if !state.settings.lock().unwrap().monitor_enabled {
        return;
    }

    // Read the text; ignore non-textual content.
    let clipboard = app.state::<Clipboard>();
    let Ok(text) = clipboard.read_text() else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }

    // ignore-self: discard what the app itself wrote (and consume the marker).
    {
        let mut last_self = state.last_self_copy.lock().unwrap();
        if last_self.as_deref() == Some(text.as_str()) {
            *last_self = None;
            return;
        }
    }

    // dedup: same text already notified recently -> no duplicates.
    {
        let mut last_seen = state.last_seen.lock().unwrap();
        if last_seen.as_deref() == Some(text.as_str()) {
            return;
        }
        *last_seen = Some(text.clone());
    }

    toast::show(app, &text);
}
