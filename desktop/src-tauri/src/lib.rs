//! Kotodama • Ai Prompt Builder — Tauri v2 desktop shell.
//!
//! Modules:
//! - `browser`  : Feature 1 — in-app AI provider (multi-webview).
//! - `clipboard`: Feature 2 — global clipboard monitor.
//! - `toast`    : notification window in the bottom-right corner.
//! - `settings` : user settings persistence.

mod browser;
mod clipboard;
mod debug;
mod kotodama;
mod settings;
mod toast;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use settings::Settings;
use tauri::menu::{CheckMenuItem, MenuItem};
use tauri::{AppHandle, Emitter, Manager, WindowEvent, Wry};
use tauri_plugin_clipboard::Clipboard;

/// Shared app state.
pub struct AppState {
    /// Last text written by the app itself (monitor ignore-self).
    pub last_self_copy: Mutex<Option<String>>,
    /// Last notified text (dedup).
    pub last_seen: Mutex<Option<String>>,
    /// Current settings.
    pub settings: Mutex<Settings>,
    /// Reference to the "Start on login" menu item (to sync its check).
    pub autostart_item: Mutex<Option<CheckMenuItem<Wry>>>,
    /// Reference to the "Always on top" menu item (to sync its check).
    pub always_on_top_item: Mutex<Option<CheckMenuItem<Wry>>>,
    /// Tray "Open" / "Quit" items, to localize their text at runtime (set_tray_labels).
    pub open_item: Mutex<Option<MenuItem<Wry>>>,
    pub quit_item: Mutex<Option<MenuItem<Wry>>>,
    /// Localized labels for the native provider context menu (set_provider_menu_labels).
    pub menu_labels: Mutex<browser::MenuLabels>,
    /// Inline transform in progress (one at a time).
    pub inline_busy: AtomicBool,
    /// While an inline transform runs, the clipboard monitor must NOT toast the
    /// simulated Ctrl+C copy (it is not a user copy).
    pub inline_suppress_toast: AtomicBool,
    /// Whether the CURRENT in-flight inline transform's recipe has toasts enabled
    /// (`Settings.recipe_notify`, resolved once at dispatch in `inline_transform`) --
    /// `inline_toast`/`inline_finish`/`inline_fail` check this before showing anything.
    pub inline_notify: AtomicBool,
    /// Original (x,y) of the main window when it was parked off-screen for an inline
    /// transform (only set when the window was hidden); restored afterwards.
    pub inline_saved_pos: Mutex<Option<(i32, i32)>>,
}

// ============================ COMMANDS ============================

/// Brings the app to the front with the copied text already in the Instructions field.
/// Shared by the hotkey, the toast click and the tray click.
/// Main hotkey / toast "Open" / tray: open the HOME (builder) with the clipboard text
/// in the description field. Nothing auto-opens or auto-sends: the user picks
/// recipe/provider from there. (Per-recipe hotkeys do the inline transform instead.)
#[tauri::command]
fn accept_clipboard(app: AppHandle) -> Result<(), String> {
    let clipboard = app.state::<Clipboard>();
    let text = clipboard.read_text().unwrap_or_default();

    if let Some(main) = app.get_window("main") {
        browser::park_provider(&main);
        bring_to_front(&main);
        let _ = main.emit("app://provider-closed", ());
        if !text.trim().is_empty() {
            let _ = main.emit("app://fill-clipboard", text);
        }
    }
    toast::hide(&app);
    Ok(())
}

/// Closes the app entirely (used by the "Oops" fallback page's Chiudi button, same effect as
/// tray "Esci"). Also registered as an invokable command (OOPS_PAGE_JS tries invoke() AND the
/// title-sentinel poller -- belt and suspenders, since which one actually reaches Rust from a
/// `chrome-error://` document hasn't been confirmed empirically yet).
#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Relaunches the app (used by the "Oops" fallback page's Riavvia button: a page reload can't fix
/// a broken webview environment, but a fresh process launch has been observed to clear it). Same
/// dual-path reasoning as `quit_app`.
#[tauri::command]
fn restart_app(app: AppHandle) {
    app.restart();
}

/// Hides the toast (✕ button / frontend-side auto-hide).
#[tauri::command]
fn hide_toast(app: AppHandle) {
    toast::hide(&app);
}

/// Writes text to the clipboard, recording the ignore-self marker.
#[tauri::command]
fn app_write_clipboard(app: AppHandle, text: String) -> Result<(), String> {
    *app.state::<AppState>().last_self_copy.lock().unwrap() = Some(text.clone());
    app.state::<Clipboard>().write_text(text)
}

/// Brings the window to the front on the CURRENT virtual desktop, in the foreground.
/// - Windows: we replicate the exact "close with the X, then reopen" flow the user confirmed lands
///   on the CURRENT desktop. `hide_to_tray` fully hides the window (SW_HIDE + skip taskbar); then,
///   after a short DELAY, we re-show it (SW_SHOW) on the current desktop. A *synchronous* hide->show
///   does NOT relocate: Windows/DWM needs a moment after the hide to unassign the window from its old
///   virtual desktop before the re-show can land it on the one we're summoning from. Version-
///   independent (works on Win10 and every Win11 build), unlike the virtual-desktop COM APIs.
/// - macOS/Linux: `set_visible_on_all_workspaces(true)` is left ON so the window follows the active space.
fn bring_to_front(w: &tauri::Window) {
    let _ = w.set_skip_taskbar(false);
    #[cfg(not(windows))]
    {
        let _ = w.set_visible_on_all_workspaces(true);
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
    #[cfg(windows)]
    {
        hide_to_tray(w); // exactly the X close-to-tray (set_skip_taskbar(true) + SW_HIDE)
        // Reopen after a real gap so the OS unassigns the old desktop; the delayed SW_SHOW then lands
        // the window on the CURRENT desktop. Same deferred main-thread pattern as browser.rs's fallback.
        let app = w.app_handle().clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || {
                if let Some(win) = app2.get_window("main") {
                    let _ = win.set_skip_taskbar(false);
                    if let Ok(h) = win.hwnd() {
                        win_show(h.0 as isize, true); // SW_SHOW -> current desktop
                    }
                    let _ = win.set_focus();
                }
            });
        });
    }
}

/// Windows only: hide (SW_HIDE) is what sends the window to the tray so that the next SW_SHOW puts it
/// on the CURRENT virtual desktop. Raw ShowWindow on the top-level HWND (Tauri's `hide()` does not hide
/// a multi-webview window; a minimized window stays tied to its desktop). Best-effort.
#[cfg(windows)]
fn win_show(hwnd_raw: isize, show: bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOW};
    unsafe {
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        let _ = ShowWindow(hwnd, if show { SW_SHOW } else { SW_HIDE });
    }
}

/// Inline transform: WebView2 won't load a webview added to a hidden (SW_HIDE) window.
/// So we show the host window FAR OFF-SCREEN and NOT ACTIVATED (focus stays on the app
/// the user is typing in) just long enough to run the hidden provider call, then hide it
/// back. Returns the original (x,y) so we can restore it (the tray-reopen relies on it).
#[cfg(windows)]
fn win_park_offscreen(hwnd_raw: isize) -> (i32, i32) {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };
    unsafe {
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        // move to (32000,32000): beyond any monitor, shown without stealing focus
        let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), 32000, 32000, 0, 0,
            SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        (r.left, r.top)
    }
}

/// Hide the off-screen host window again and restore its real position (while hidden), so a
/// later tray-reopen shows it where it was.
#[cfg(windows)]
fn win_unpark_hide(hwnd_raw: isize, x: i32, y: i32) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, ShowWindow, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE,
    };
    unsafe {
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        let _ = ShowWindow(hwnd, SW_HIDE);
        let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), x, y, 0, 0,
            SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER);
    }
}

/// macOS: move the window FAR off-screen and order it front WITHOUT activating. WKWebView throttles
/// (pauses timers / streaming) when its window is miniaturized or occluded, so the provider answer
/// never appears while the window is hidden. Kept off-screen it stays invisible and never steals
/// focus (the paste still lands in the user's app), but the webview runs at full speed. Main-thread
/// only (AppKit). Returns the old origin.
#[cfg(target_os = "macos")]
unsafe fn mac_park(ns_window: *mut core::ffi::c_void) -> (f64, f64) {
    use objc2_app_kit::NSWindow;
    use objc2_foundation::NSPoint;
    let win: &NSWindow = &*(ns_window as *const NSWindow);
    let f = win.frame();
    let orig = (f.origin.x, f.origin.y);
    if win.isMiniaturized() {
        win.deminiaturize(None);
    }
    win.setFrameOrigin(NSPoint { x: -30000.0, y: -30000.0 });
    win.orderFrontRegardless();
    orig
}

/// macOS: stop a WKWebView from throttling/pausing when its window is occluded or off-screen.
/// The provider webviews are parked off-screen and the host window is often minimized during the
/// inline transform, so WebKit's occlusion detection would freeze their JS timers AND the streaming
/// render of the provider's answer (it never reaches `done`). `_setWindowOcclusionDetectionEnabled:`
/// is a long-standing private WKWebView selector; best-effort (ignored if it ever goes away).
#[cfg(target_os = "macos")]
pub(crate) fn mac_disable_occlusion<R: tauri::Runtime>(webview: &tauri::Webview<R>) {
    let _ = webview.with_webview(|pw| unsafe {
        use objc2::msg_send;
        let wk: *mut objc2::runtime::AnyObject = pw.inner().cast();
        if !wk.is_null() {
            let _: () = msg_send![wk, _setWindowOcclusionDetectionEnabled: false];
        }
    });
}

/// macOS: restore the window origin and hide it again (order out).
#[cfg(target_os = "macos")]
unsafe fn mac_unpark(ns_window: *mut core::ffi::c_void, x: f64, y: f64) {
    use objc2_app_kit::NSWindow;
    use objc2_foundation::NSPoint;
    let win: &NSWindow = &*(ns_window as *const NSWindow);
    win.setFrameOrigin(NSPoint { x, y });
    win.orderOut(None);
}

/// If the main window was hidden, park it off-screen so the inline provider webview loads,
/// remembering the position to restore on `inline_restore_window`.
fn inline_park_window(app: &AppHandle) {
    #[cfg(windows)]
    if let Some(main) = app.get_window("main") {
        let visible = main.is_visible().unwrap_or(true);
        debug::log(format!("inline_park_window: main visible={visible}"));
        if !visible {
            if let Ok(h) = main.hwnd() {
                let pos = win_park_offscreen(h.0 as isize);
                *app.state::<AppState>().inline_saved_pos.lock().unwrap() = Some(pos);
            }
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(main) = app.get_window("main") {
        let visible = main.is_visible().unwrap_or(true);
        let minimized = main.is_minimized().unwrap_or(false);
        debug::log(format!("inline_park_window(mac): visible={visible} minimized={minimized}"));
        if !visible || minimized {
            // AppKit must run on the main thread; block briefly so the webview is un-throttled
            // BEFORE we dispatch the provider request.
            let app2 = app.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = app.run_on_main_thread(move || {
                if let Some(w) = app2.get_window("main") {
                    if let Ok(ptr) = w.ns_window() {
                        let (x, y) = unsafe { mac_park(ptr) };
                        *app2.state::<AppState>().inline_saved_pos.lock().unwrap() =
                            Some((x as i32, y as i32));
                    }
                }
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(std::time::Duration::from_millis(600));
        }
    }
}

/// Undo `inline_park_window` (hide + restore position). No-op if the window was visible.
fn inline_restore_window(app: &AppHandle) {
    #[cfg(windows)]
    {
        let pos = app.state::<AppState>().inline_saved_pos.lock().unwrap().take();
        if let (Some((x, y)), Some(main)) = (pos, app.get_window("main")) {
            if let Ok(h) = main.hwnd() {
                win_unpark_hide(h.0 as isize, x, y);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let pos = app.state::<AppState>().inline_saved_pos.lock().unwrap().take();
        if let Some((x, y)) = pos {
            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || {
                if let Some(w) = app2.get_window("main") {
                    if let Ok(ptr) = w.ns_window() {
                        unsafe { mac_unpark(ptr, x as f64, y as f64) };
                    }
                }
            });
        }
    }
}

/// Hide the window to the tray. Windows: SW_HIDE (so the next SW_SHOW lands on the CURRENT desktop);
/// macOS/Linux: minimize. Both skip the taskbar. (Tauri `hide()` can't hide a multi-webview window.)
fn hide_to_tray(w: &tauri::Window) {
    let _ = w.set_skip_taskbar(true);
    #[cfg(windows)]
    if let Ok(h) = w.hwnd() {
        win_show(h.0 as isize, false); // SW_HIDE (not minimize: keeps it off any desktop)
    }
    #[cfg(not(windows))]
    let _ = w.minimize();
}

/// Open from the tray: brings the builder to the front and inserts the copied
/// text into it, WITHOUT opening the provider (unlike toast/shortcut, which
/// process and open immediately). If the clipboard is empty, it only shows the
/// window without clearing any in-progress description.
fn open_from_tray(app: &AppHandle) {
    let text = app.state::<Clipboard>().read_text().unwrap_or_default();
    if let Some(main) = app.get_window("main") {
        browser::park_provider(&main);
        bring_to_front(&main);
        let _ = main.emit("app://provider-closed", ()); // back to the builder
        if !text.trim().is_empty() {
            let _ = main.emit("app://fill-clipboard", text); // fill the description (no auto-open)
        }
    }
}

/// Shows the main window and focuses it.
#[tauri::command]
fn show_main(app: AppHandle) {
    // get_window (not get_webview_window): with the provider child-webview the
    // "main" window has 2 webviews and get_webview_window("main") returns None.
    if let Some(w) = app.get_window("main") {
        bring_to_front(&w);
    }
}

/// Hides the main window to the tray (✕ of the in-app custom titlebar).
/// Hides the provider's child-webview FIRST: with multi-webview `window.hide()`
/// alone does not hide it and the window would stay visible.
#[tauri::command]
fn hide_main(app: AppHandle) {
    // get_window: with the provider child-webview, get_webview_window("main") = None.
    if let Some(w) = app.get_window("main") {
        // Park the provider out of view (set_position, non-blocking) so its
        // "presence" doesn't block the minimize and on reopen it doesn't cover the builder.
        browser::park_provider(&w);
        let _ = w.emit("app://provider-closed", ()); // bring the UI back to the builder
        hide_to_tray(&w);
    }
}

/// Returns the current settings.
#[tauri::command]
fn get_settings(app: AppHandle) -> Settings {
    app.state::<AppState>().settings.lock().unwrap().clone()
}

/// Operating-system language (e.g. "it-IT" → the frontend maps it to the supported code).
/// Reliable OS-language source for the UI (the webview's navigator.language can vary).
#[tauri::command]
fn get_system_locale() -> String {
    sys_locale::get_locale().unwrap_or_else(|| "en".into())
}

/// Open a URL in the system default browser (used by the About links in Settings).
#[tauri::command]
fn open_url(app: AppHandle, url: String) {
    use tauri_plugin_opener::OpenerExt;
    let _ = app.opener().open_url(url, None::<&str>);
}

/// Opens a downloaded file with the OS default app (Download manager: click the name).
#[tauri::command]
fn open_download_path(app: AppHandle, path: String) {
    use tauri_plugin_opener::OpenerExt;
    let _ = app.opener().open_path(path, None::<&str>);
}

/// Reveals a downloaded file in the system file manager (Download manager: folder icon).
/// Falls back to opening the parent directory if reveal is unavailable.
#[tauri::command]
fn reveal_download_path(app: AppHandle, path: String) {
    use tauri_plugin_opener::OpenerExt;
    if app.opener().reveal_item_in_dir(&path).is_err() {
        if let Some(parent) = std::path::Path::new(&path).parent() {
            let _ = app
                .opener()
                .open_path(parent.to_string_lossy().to_string(), None::<&str>);
        }
    }
}

/// Saves and applies new settings (hotkey, autostart, monitor, language…).
#[tauri::command]
fn set_settings(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    apply_autostart(&app, settings.autostart);
    if let Some(w) = app.get_window("main") {
        let _ = w.set_always_on_top(settings.always_on_top);
    }

    // Register the whole shortcut set (main hotkey with fallback + per-recipe ones);
    // the effective state (fallback applied, conflicting entries dropped) is what
    // gets stored and returned, so disk and UI stay faithful.
    let mut final_settings = settings;
    register_hotkeys(&app, &mut final_settings);

    // In-memory state = source of truth; a SINGLE save to disk, in the
    // background, so the UI thread doesn't block on I/O.
    *app.state::<AppState>().settings.lock().unwrap() = final_settings.clone();
    save_settings_bg(&app, final_settings.clone());
    // Return the EFFECTIVE one (with any hotkey fallback) so the frontend shows
    // what is actually registered → no discrepancy with the toast.
    Ok(final_settings)
}

/// Saves ONLY the UI state (provider/recipe/length/tone) without re-registering
/// the hotkey or touching autostart: called often (tile click, recipe change…),
/// it must be lightweight. The in-memory state remains the source of truth.
#[tauri::command]
fn save_ui_state(app: AppHandle, provider: String, recipe: String, length: u32, tone: u32, resp_fmt: u32) {
    let snapshot = {
        let state = app.state::<AppState>();
        let mut g = state.settings.lock().unwrap();
        g.default_provider = provider;
        g.recipe = recipe;
        g.length = length;
        g.tone = tone;
        g.resp_fmt = resp_fmt;
        g.clone()
    };
    save_settings_bg(&app, snapshot);
}

/// Adds (`known:true`) or removes (`known:false`) a provider from `known_providers` -- the
/// frontend uses this set to pre-select AND show-by-default only providers with an active login
/// for "chiedi a tutti" (instead of all of them regardless of login state). A no-op (no save, no
/// event) if the value doesn't actually change, so a normal successful chat doesn't hit disk on
/// every turn. On a real change, also emits `app://provider-known-changed` so the frontend can
/// update the chip's visibility immediately instead of waiting for the next app restart.
pub(crate) fn set_provider_known(app: &AppHandle, key: &str, known: bool) {
    let snapshot = {
        let state = app.state::<AppState>();
        let mut g = state.settings.lock().unwrap();
        let changed = if known {
            g.known_providers.insert(key.to_string())
        } else {
            g.known_providers.remove(key)
        };
        if !changed {
            return;
        }
        g.clone()
    };
    save_settings_bg(app, snapshot);
    let _ = app.emit("app://provider-known-changed", serde_json::json!({ "key": key, "known": known }));
}

/// Marks a provider as "known" (a Kotodama broadcast to it succeeded at least once, i.e. the user
/// has an active login there). The inverse (a provider found logged-out) is `set_provider_known`
/// called directly with `known:false` from `kotodama.rs` (login-wall signals).
#[tauri::command]
fn mark_provider_known(app: AppHandle, key: String) {
    set_provider_known(&app, &key, true);
}

/// Localizes the tray labels (Open / Start-on-login / Always-on-top / Quit) in the app language.
/// Called by the frontend at startup and on every language change.
#[tauri::command]
fn set_tray_labels(app: AppHandle, open: String, autostart: String, always_on_top: String, quit: String) {
    if let Some(i) = app.state::<AppState>().open_item.lock().unwrap().as_ref() {
        let _ = i.set_text(open);
    }
    if let Some(i) = app.state::<AppState>().autostart_item.lock().unwrap().as_ref() {
        let _ = i.set_text(autostart);
    }
    if let Some(i) = app.state::<AppState>().always_on_top_item.lock().unwrap().as_ref() {
        let _ = i.set_text(always_on_top);
    }
    if let Some(i) = app.state::<AppState>().quit_item.lock().unwrap().as_ref() {
        let _ = i.set_text(quit);
    }
}

/// Returns the custom recipes.
#[tauri::command]
fn get_recipes(app: AppHandle) -> Vec<settings::Recipe> {
    settings::load_recipes(&app)
}

/// Saves the custom recipes.
#[tauri::command]
fn save_recipes(app: AppHandle, recipes: Vec<settings::Recipe>) -> Result<(), String> {
    settings::save_recipes(&app, &recipes)
}

/// Returns the saved Kotodama meta-chat sessions (opaque JSON, owned by the frontend).
#[tauri::command]
fn get_kotodama_sessions(app: AppHandle) -> serde_json::Value {
    settings::load_kt_sessions(&app)
}

/// Saves the Kotodama meta-chat sessions.
#[tauri::command]
fn save_kotodama_sessions(app: AppHandle, sessions: serde_json::Value) -> Result<(), String> {
    settings::save_kt_sessions(&app, &sessions)
}

/// Returns the custom fields.
#[tauri::command]
fn get_fields(app: AppHandle) -> Vec<settings::Field> {
    settings::load_fields(&app)
}

/// Saves the custom fields.
#[tauri::command]
fn save_fields(app: AppHandle, fields: Vec<settings::Field>) -> Result<(), String> {
    settings::save_fields(&app, &fields)
}

// ============================ HELPER ============================

/// Persists the settings off the UI thread; the in-memory state remains the
/// source of truth, so any write error does not block the app.
fn save_settings_bg(app: &AppHandle, settings: Settings) {
    let app = app.clone();
    std::thread::spawn(move || {
        if let Err(e) = settings::save(&app, &settings) {
            eprintln!("[settings] salvataggio in background fallito: {e}");
        }
    });
}

fn apply_autostart(app: &AppHandle, enabled: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let mgr = app.autolaunch();
    let now = mgr.is_enabled().unwrap_or(false);
    if enabled != now {
        let _ = if enabled { mgr.enable() } else { mgr.disable() };
    }
    if let Some(item) = app.state::<AppState>().autostart_item.lock().unwrap().as_ref() {
        let _ = item.set_checked(enabled);
    }
}

/// Fallback combinations supported on Windows (the `global-hotkey` backend
/// does not map `IntlBackslash` on Windows → "Unknown VKCode"). On Linux X11
/// the requested hotkey (e.g. Ctrl+<) works and these are not used.
const HOTKEY_FALLBACKS: &[&str] = &["Control+Backslash", "Control+Backquote", "Control+Shift+Space"];

/// Simulated Ctrl+C / Ctrl+V for the INLINE transform. Before injecting the combo we
/// RELEASE the physical modifiers still held from the user's hotkey (Alt/Shift/Win),
/// or the target app would see e.g. Ctrl+Alt+C instead of Ctrl+C.
#[cfg(windows)]
fn send_combo(vk_letter: u16) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_SHIFT,
    };
    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
    let vk = VIRTUAL_KEY(vk_letter);
    let inputs = [
        // release any held modifiers from the user's shortcut
        key(VK_MENU, true),
        key(VK_SHIFT, true),
        key(VK_LWIN, true),
        // Ctrl+<letter>
        key(VK_CONTROL, false),
        key(vk, false),
        key(vk, true),
        key(VK_CONTROL, true),
    ];
    unsafe {
        let sent = SendInput(&inputs, std::mem::size_of::<windows::Win32::UI::Input::KeyboardAndMouse::INPUT>() as i32);
        if sent != inputs.len() as u32 {
            // SendInput returns the number of events it actually queued; less than we asked for
            // means the OS refused/blocked some of them (e.g. UIPI, a secure/locked desktop, or the
            // input desktop not being ours yet right after a resume) — GetLastError says why.
            let err = windows::Win32::Foundation::GetLastError();
            debug::log(format!("send_combo: SendInput only queued {sent}/{} events, GetLastError={:?}", inputs.len(), err));
        }
    }
}
/// macOS: synthesize Cmd+C / Cmd+V via CGEvent. macOS uses the COMMAND modifier (not Control) for
/// copy/paste. Requires the app to hold Accessibility permission (see `ax_is_trusted`), else the
/// posted events are silently dropped by the OS. We also release any stray Control/Option/Shift
/// still held from the user's hotkey so the target app sees a clean Cmd+<letter>.
#[cfg(target_os = "macos")]
fn send_combo(vk_letter: u16) {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    // Windows VK -> macOS virtual keycode (kVK_ANSI_C = 8, kVK_ANSI_V = 9, kVK_Command = 0x37).
    let keycode: CGKeyCode = match vk_letter {
        VK_C => 8,
        VK_V => 9,
        _ => return,
    };
    const K_CMD: CGKeyCode = 0x37;
    let src = match CGEventSource::new(CGEventSourceStateID::CombinedSessionState) {
        Ok(s) => s,
        Err(_) => return,
    };
    let post = |kc: CGKeyCode, down: bool, cmd: bool| {
        if let Ok(ev) = CGEvent::new_keyboard_event(src.clone(), kc, down) {
            if cmd {
                ev.set_flags(CGEventFlags::CGEventFlagCommand);
            }
            ev.post(CGEventTapLocation::HID);
        }
    };
    // 1) release the hotkey's own modifiers (control=0x3B, option=0x3A, shift=0x38) still held, so
    //    the app doesn't see e.g. Ctrl+Opt+Cmd+C instead of a clean Cmd+C.
    post(0x3B, false, false);
    post(0x3A, false, false);
    post(0x38, false, false);
    // 2) an EXPLICIT Command key press around the letter (real modifier events, not just the flag)
    //    — more robust than the flag alone across apps.
    post(K_CMD, true, true);
    post(keycode, true, true);
    post(keycode, false, true);
    post(K_CMD, false, false);
}
#[cfg(all(not(windows), not(target_os = "macos")))]
fn send_combo(_vk_letter: u16) {}

/// macOS Accessibility trust check. With `prompt=true` the system shows the "allow Accessibility"
/// dialog and adds the app to the list the FIRST time; once granted it just returns true. Synthetic
/// key events (send_combo) do nothing until this is granted — that was a hidden cause of the inline
/// transform failing on macOS.
#[cfg(target_os = "macos")]
fn ax_is_trusted(prompt: bool) -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::{CFString, CFStringRef};
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }
    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let val = CFBoolean::from(prompt);
        let dict = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), val.as_CFType())]);
        AXIsProcessTrustedWithOptions(dict.as_concrete_TypeRef())
    }
}

const VK_C: u16 = 0x43;
const VK_V: u16 = 0x56;

/// Per-recipe hotkey -> INLINE transform: copy the selection from the focused app,
/// show the "processing" toast, and hand text+recipe to the frontend (hidden webview
/// gateway). The answer comes back via `inline_finish`/`inline_fail`, which paste it
/// where the user was typing. Nothing is brought to the foreground.
fn inline_transform(app: AppHandle, recipe: String) {
    let state = app.state::<AppState>();
    if state.inline_busy.swap(true, Ordering::SeqCst) {
        return; // one transform at a time
    }
    state.inline_suppress_toast.store(true, Ordering::SeqCst);
    let notify = state
        .settings
        .lock()
        .unwrap()
        .recipe_notify
        .get(&recipe)
        .copied()
        .unwrap_or(true);
    state.inline_notify.store(notify, Ordering::SeqCst);
    std::thread::spawn(move || {
        // let the user release the hotkey keys before we inject the copy combo (on macOS a still-held
        // Ctrl/Opt would pollute the synthetic Cmd+C), then copy.
        std::thread::sleep(std::time::Duration::from_millis(350));
        // macOS: synthetic key events do nothing without Accessibility permission -> the copy would
        // silently yield an empty clipboard ("Niente da copiare"). Gate on it and give a clear
        // message (prompting the system dialog the first time) instead of the misleading error.
        #[cfg(target_os = "macos")]
        {
            if !ax_is_trusted(true) {
                debug::log("inline_transform: macOS Accessibility not granted");
                let st = app.state::<AppState>();
                if st.inline_notify.load(Ordering::SeqCst) { toast::show_state(&app, "error", "accessibility"); }
                st.inline_busy.store(false, Ordering::SeqCst);
                st.inline_suppress_toast.store(false, Ordering::SeqCst);
                return;
            }
        }
        #[cfg(windows)]
        {
            // Diagnostic: WHICH window is focused right before we synthesize Ctrl+C. If this is
            // NULL, our own app, or an unexpected window, the copy has nothing useful to act on —
            // independent of whether SendInput itself succeeds.
            use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId};
            unsafe {
                let hwnd = GetForegroundWindow();
                let mut buf = [0u16; 128];
                let len = GetWindowTextW(hwnd, &mut buf);
                let title = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                debug::log(format!("inline_transform: foreground hwnd={:?} pid={pid} title={title:?}", hwnd.0));
            }
        }
        let before = app.state::<Clipboard>().read_text().unwrap_or_default();
        // Write a SENTINEL before copying, and check the clipboard against IT (not against
        // `before`): comparing "did it change" is wrong when the user copies text that happens to
        // be IDENTICAL to what's already in the clipboard (e.g. copy -> paste -> select-all ->
        // re-copy the very text they just pasted) — the real Ctrl+C fires and the OS clipboard does
        // get overwritten, but with the SAME bytes, so a before/after diff sees "no change" and
        // wrongly reports "nothing to copy". A sentinel only the app itself could have written
        // makes ANY real copy (even of unchanged content) detectable.
        const SENTINEL: &str = "\u{200B}__kotodama_inline_empty__\u{200B}";
        let _ = app.state::<Clipboard>().write_text(SENTINEL.to_string());
        send_combo(VK_C);
        // wait for the OS copy to land (retry: some apps are slow).
        // RE-SEND the combo a few times over a longer window (not just re-check the clipboard):
        // right after a system resume from sleep, the OS can take a couple of seconds to settle
        // where synthetic input actually lands, and a SendInput/CGEvent posted in that window can be
        // silently dropped entirely (not delayed) — polling alone would never see a copy that never
        // happened. A few resends over ~3.6s recovers this without changing the fast, normal case
        // (the loop still returns as soon as a fresh copy is seen).
        let mut text = String::new();
        let mut fresh = false;
        for i in 0..24 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let now = app.state::<Clipboard>().read_text().unwrap_or_default();
            if !now.trim().is_empty() && now != SENTINEL {
                text = now;
                fresh = true;
                break;
            }
            if i == 7 || i == 15 {
                send_combo(VK_C); // the first synthetic copy may have been dropped -> retry it
            }
        }
        if !fresh {
            // nothing was copied: put back whatever the clipboard had before, don't leave our
            // sentinel behind.
            let _ = app.state::<Clipboard>().write_text(before.clone());
        }
        debug::log(format!(
            "inline copy: before_len={} fresh={} text_len={} text_preview={:?}",
            before.chars().count(), fresh, text.chars().count(),
            text.chars().take(120).collect::<String>()
        ));
        // INLINE must transform the CURRENT selection, never a stale clipboard: if the copy produced
        // nothing new, tell the user to select text (immediate, clear) instead of silently reusing
        // the old clipboard or hanging on a processing spinner.
        if !fresh {
            let st = app.state::<AppState>();
            if st.inline_notify.load(Ordering::SeqCst) { toast::show_state(&app, "error", "empty"); }
            st.inline_busy.store(false, Ordering::SeqCst);
            st.inline_suppress_toast.store(false, Ordering::SeqCst);
            return;
        }
        debug::log(format!(
            "inline_transform recipe={recipe} len={} preview={:?}",
            text.len(), text.chars().take(120).collect::<String>()
        ));
        // WebView2 won't load a webview on a hidden window: park the host off-screen first.
        inline_park_window(&app);
        // the frontend shows the localized "processing <recipe>" toast and dispatches
        match app.get_window("main") {
            Some(main) => {
                let _ = main.emit(
                    "app://inline-transform",
                    serde_json::json!({ "text": text, "recipe": recipe }),
                );
            }
            None => {
                // Without the emit reaching the frontend, neither inline_finish nor inline_fail
                // will EVER be called from JS: without this, inline_busy would stay stuck (silent,
                // no toast, no log) until the 240s safety net below — every hotkey press in that
                // whole window would then silently no-op on the busy check at the top. Fail fast
                // and visibly instead.
                debug::log("inline_transform: main window not found, aborting");
                let st = app.state::<AppState>();
                if st.inline_notify.load(Ordering::SeqCst) { toast::show_state(&app, "error", "sendfail"); }
                inline_restore_window(&app);
                st.inline_busy.store(false, Ordering::SeqCst);
                st.inline_suppress_toast.store(false, Ordering::SeqCst);
                return;
            }
        }
        // safety: if no answer ever comes back, release the flags after 240s
        let app2 = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(240));
            let st = app2.state::<AppState>();
            if st.inline_busy.load(Ordering::SeqCst) {
                st.inline_busy.store(false, Ordering::SeqCst);
                st.inline_suppress_toast.store(false, Ordering::SeqCst);
                inline_restore_window(&app2);
                toast::hide(&app2);
            }
        });
    });
}

/// Frontend -> show a localized inline-transform toast state (gated by the current recipe's
/// `recipe_notify`, resolved in `inline_transform`).
#[tauri::command]
fn inline_toast(app: AppHandle, mode: String, label: String) {
    if app.state::<AppState>().inline_notify.load(Ordering::SeqCst) {
        toast::show_state(&app, &mode, &label);
    }
}

/// Inline transform answer arrived: put it in the clipboard (self-marked) and paste it
/// into the input the user copied from.
#[tauri::command]
fn inline_finish(app: AppHandle, text: String) -> Result<(), String> {
    *app.state::<AppState>().last_self_copy.lock().unwrap() = Some(text.clone());
    app.state::<Clipboard>()
        .write_text(text)
        .map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        inline_restore_window(&app); // hide the off-screen host again before pasting
        std::thread::sleep(std::time::Duration::from_millis(150));
        send_combo(VK_V);
        let st = app.state::<AppState>();
        if st.inline_notify.load(Ordering::SeqCst) { toast::show_state(&app, "done", ""); }
        st.inline_busy.store(false, Ordering::SeqCst);
        st.inline_suppress_toast.store(false, Ordering::SeqCst);
    });
    Ok(())
}

/// Inline transform failed: error toast + release the flags.
#[tauri::command]
fn inline_fail(app: AppHandle, reason: String) {
    let st = app.state::<AppState>();
    if st.inline_notify.load(Ordering::SeqCst) { toast::show_state(&app, "error", &reason); }
    inline_restore_window(&app);
    st.inline_busy.store(false, Ordering::SeqCst);
    st.inline_suppress_toast.store(false, Ordering::SeqCst);
}

/// Tries to register a single accelerator. `true` on success.
/// `recipe = None` -> main hotkey (default-recipe flow); `Some(r)` -> forces recipe `r`.
fn try_register_hotkey(app: &AppHandle, accel: &str, recipe: Option<String>) -> bool {
    use std::str::FromStr;
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

    let Ok(shortcut) = Shortcut::from_str(accel) else {
        return false;
    };
    let handle = app.clone();
    let accel_owned = accel.to_string();
    let recipe_for_closure = recipe.clone();
    let ok = app
        .global_shortcut()
        .on_shortcut(shortcut, move |_app, _sc, event| {
            if event.state() == ShortcutState::Pressed {
                debug::log(format!("hotkey PRESSED accel={accel_owned} recipe={recipe_for_closure:?}"));
                match &recipe_for_closure {
                    // per-recipe hotkey: INLINE transform (copy -> process -> paste back)
                    Some(r) => inline_transform(handle.clone(), r.clone()),
                    None => {
                        let _ = accept_clipboard(handle.clone());
                    }
                }
            }
        })
        .is_ok();
    debug::log(format!("hotkey REGISTER accel={accel} recipe={recipe:?} ok={ok}"));
    ok
}

/// Registers the MAIN hotkey with per-platform fallbacks (no unregister here: the caller
/// `register_hotkeys` clears everything first). Returns the accelerator ACTUALLY
/// registered, or `None` if none is registrable (the toast/tray fallback remains).
fn register_hotkey(app: &AppHandle, accel: &str) -> Option<String> {
    let mut candidates: Vec<&str> = vec![accel];
    candidates.extend(HOTKEY_FALLBACKS.iter().copied().filter(|f| *f != accel));

    for cand in candidates {
        if try_register_hotkey(app, cand, None) {
            if cand != accel {
                eprintln!("[hotkey] '{accel}' non registrabile su questa piattaforma; uso '{cand}'");
            }
            return Some(cand.to_string());
        }
    }
    eprintln!("[hotkey] nessun hotkey registrabile: usa il toast o la tray");
    None
}

/// (Re)registers ALL global shortcuts: one `unregister_all`, then the main hotkey
/// (fallback chain) and one shortcut per recipe entry. Mutates `s` to the EFFECTIVE
/// state: main hotkey possibly replaced by a fallback, unregistrable/conflicting
/// recipe entries DROPPED (duplicate accelerators fail to register -> discarded).
fn register_hotkeys(app: &AppHandle, s: &mut Settings) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let _ = app.global_shortcut().unregister_all();
    if let Some(active) = register_hotkey(app, &s.hotkey) {
        s.hotkey = active;
    }
    s.recipe_hotkeys.retain(|recipe, accel| {
        let ok = !accel.is_empty() && try_register_hotkey(app, accel, Some(recipe.clone()));
        if !ok {
            eprintln!("[hotkey] scorciatoia ricetta scartata: {recipe} = '{accel}'");
        }
        ok
    });
}

/// Startup registration: applies the whole shortcut set and, if the effective state
/// differs from what was on disk (fallback main hotkey / dropped recipe entries),
/// persists the real one.
fn register_and_persist_hotkeys(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut snapshot = state.settings.lock().unwrap().clone();
    let before = (snapshot.hotkey.clone(), snapshot.recipe_hotkeys.clone());
    register_hotkeys(app, &mut snapshot);
    let changed = before != (snapshot.hotkey.clone(), snapshot.recipe_hotkeys.clone());
    *state.settings.lock().unwrap() = snapshot.clone();
    if changed {
        let _ = settings::save(app, &snapshot);
    }
}

fn build_tray(app: &AppHandle, autostart_on: bool) -> tauri::Result<()> {
    use tauri::menu::{Menu, PredefinedMenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    // Etichette MONOLINGUA nella lingua dell'app (it/en hardcoded; en di default). Le bilingue
    // ("Apri / Open"...) erano troppo lunghe. Il frontend poi le localizza per TUTTE le lingue
    // via set_tray_labels (all'avvio e ad ogni cambio lingua).
    let (it, always_on_top_on) = {
        let st = app.state::<AppState>();
        let g = st.settings.lock().unwrap();
        (g.language == "it", g.always_on_top)
    };
    let (open_lbl, auto_lbl, aot_lbl, quit_lbl) = if it {
        ("Apri", "Apri al login", "Sempre in primo piano", "Esci")
    } else {
        ("Open", "Start on login", "Always on top", "Quit")
    };
    let open_i = MenuItem::with_id(app, "open", open_lbl, true, None::<&str>)?;
    let login_i = CheckMenuItem::with_id(app, "autostart", auto_lbl, true, autostart_on, None::<&str>)?;
    let aot_i = CheckMenuItem::with_id(app, "alwaysontop", aot_lbl, true, always_on_top_on, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_i = MenuItem::with_id(app, "quit", quit_lbl, true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_i, &login_i, &aot_i, &sep, &quit_i])?;

    // Store items: sync check + localize text at runtime.
    {
        let st = app.state::<AppState>();
        st.autostart_item.lock().unwrap().replace(login_i.clone());
        st.always_on_top_item.lock().unwrap().replace(aot_i.clone());
        st.open_item.lock().unwrap().replace(open_i.clone());
        st.quit_item.lock().unwrap().replace(quit_i.clone());
    }

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("Kotodama • Ai Prompt Builder")
        .menu(&menu)
        .show_menu_on_left_click(false);

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    builder
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => open_from_tray(app),
            "autostart" => {
                use tauri_plugin_autostart::ManagerExt;
                let now = app.autolaunch().is_enabled().unwrap_or(false);
                let want = !now;
                apply_autostart(app, want);
                // persist
                let state = app.state::<AppState>();
                let snapshot = {
                    let mut g = state.settings.lock().unwrap();
                    g.autostart = want;
                    g.clone()
                };
                save_settings_bg(app, snapshot);
                // Tieni il frontend in sync: altrimenti currentSettings.autostart resta vecchio e
                // un successivo set_settings lo sovrascriverebbe (ri-disabilitando l'autostart).
                if let Some(main) = app.get_window("main") {
                    let _ = main.emit("app://autostart-changed", want);
                }
            }
            "alwaysontop" => {
                let state = app.state::<AppState>();
                let want = {
                    let mut g = state.settings.lock().unwrap();
                    g.always_on_top = !g.always_on_top;
                    g.always_on_top
                };
                if let Some(main) = app.get_window("main") {
                    let _ = main.set_always_on_top(want);
                }
                if let Some(item) = state.always_on_top_item.lock().unwrap().as_ref() {
                    let _ = item.set_checked(want);
                }
                let snapshot = state.settings.lock().unwrap().clone();
                save_settings_bg(app, snapshot);
                // Same sync need as autostart: keep the Settings modal's own toggle from going
                // stale (a later Save from there would otherwise silently revert this).
                if let Some(main) = app.get_window("main") {
                    let _ = main.emit("app://always-on-top-changed", want);
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                // Tray click = always recall the window onto the current desktop
                // (a toggle based on is_visible fails with virtual desktops).
                let app = tray.app_handle();
                if let Some(w) = app.get_window("main") {
                    bring_to_front(&w);
                }
            }
        })
        .build(app)?;

    Ok(())
}

// ============================ AUTO-UPDATE ============================

/// Update info returned to the frontend.
#[derive(serde::Serialize)]
struct UpdateInfo {
    version: String,
    notes: String,
}

/// Checks whether there is an update in the public repo's Releases.
/// Returns `None` if the app is already up to date.
#[tauri::command]
async fn check_for_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await.map_err(|e| e.to_string())? {
        Some(update) => Ok(Some(UpdateInfo {
            version: update.version.clone(),
            notes: update.body.clone().unwrap_or_default(),
        })),
        None => Ok(None),
    }
}

/// Downloads and installs the update, then restarts the app.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Ok(()); // no update: nothing to do
    };
    update
        .download_and_install(|_chunk, _total| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart() // does not return (restarts the process) → coerce to Result
}

// ============================ ENTRY ============================

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // WebView2 (Windows): ALL webviews of the process must create their environment
    // with the SAME additional browser arguments, otherwise the 2nd webview (the
    // provider child) fails to initialize and stays BLANK. We therefore set the
    // arguments once, process-wide, BEFORE any webview is created — instead of
    // per-window (which previously diverged: main/toast without `--accept-lang`,
    // provider with it). This keeps the dynamic OS-language accept-lang AND keeps
    // every webview consistent. We append to any pre-existing value (e.g. debug flags).
    #[cfg(windows)]
    {
        let extra = browser::provider_browser_args();
        let value = match std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS") {
            Ok(existing) if !existing.trim().is_empty() => format!("{existing} {extra}"),
            _ => extra,
        };
        std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", value);
    }

    tauri::Builder::default()
        // MUST be the first plugin: a 2nd launch (e.g. from the Start menu) does
        // not create a new process but recalls the window of the already-running
        // instance → no double tray icon and no duplicate process.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_window("main") {
                bring_to_front(&w);
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--silent"]),
        ))
        // Register the state ON THE BUILDER: it must be available before any
        // window (e.g. toast) or IPC command accesses it.
        .manage(AppState {
            last_self_copy: Mutex::new(None),
            last_seen: Mutex::new(None),
            settings: Mutex::new(Settings::default()),
            autostart_item: Mutex::new(None),
            always_on_top_item: Mutex::new(None),
            open_item: Mutex::new(None),
            quit_item: Mutex::new(None),
            menu_labels: Mutex::new(browser::MenuLabels::default()),
            inline_busy: AtomicBool::new(false),
            inline_suppress_toast: AtomicBool::new(false),
            inline_notify: AtomicBool::new(true),
            inline_saved_pos: Mutex::new(None),
        })
        // App-level menu events: the provider's native EDITING context menu (browser.rs,
        // `show_context_menu`) puts a "Copy URL" item with id "copy_url". Copy/Cut/Paste/
        // Select-all are predefined items and act on the focused webview by themselves (no
        // event here).
        .on_menu_event(|app, event| {
            let id = event.id.as_ref();
            if id == "copy_url" {
                // Right-click "Copy URL": copy the provider webview's current URL to the clipboard
                // (ignore-self marker set so the clipboard monitor doesn't pop a toast for it).
                if let Some(pv) = browser::active_webview(app) {
                    if let Ok(url) = pv.url() {
                        let s = url.to_string();
                        *app.state::<AppState>().last_self_copy.lock().unwrap() = Some(s.clone());
                        let _ = app.state::<Clipboard>().write_text(s);
                    }
                }
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();
            // Checked BEFORE load() (which returns defaults on a missing file too, so it alone
            // can't tell "fresh install" from "existing user, corrupted file"): gates the
            // fresh-install autostart activation below without ever touching an existing user's
            // explicit choice.
            let fresh_install = !settings::exists(&handle);
            let loaded = settings::load(&handle);

            // Update the values loaded from disk into the already-registered state.
            *app.state::<AppState>().settings.lock().unwrap() = loaded.clone();

            // Main window lifecycle + show IMMEDIATELY: the first paint arrives
            // before the non-visual init (tray/hotkey/monitor) below.
            if let Some(main) = app.get_webview_window("main") {
                let win = main.clone();
                main.on_window_event(move |event| match event {
                    WindowEvent::CloseRequested { api, .. } => {
                        // Close = hide to the tray (the monitor keeps running; exit via tray "Quit").
                        api.prevent_close();
                        browser::park_provider(&win);
                        let _ = win.emit("app://provider-closed", ());
                        // hide_to_tray wants a Window; `win` is the WebviewWindow -> fetch the Window.
                        if let Some(w) = win.get_window("main") {
                            hide_to_tray(&w);
                        }
                    }
                    WindowEvent::Resized(_) => browser::resize_provider(&win),
                    _ => {}
                });

                // Start in the tray if launched with --silent (autostart), otherwise show.
                let silent = std::env::args().any(|a| a == "--silent");
                if !silent {
                    let _ = main.show();
                    let _ = main.set_focus();
                }
                // Debug-only, gated on KOTODAMA_TEST_OOPS: forces a real chrome-error on the main
                // window shortly after startup, so the Oops-page watchdog/buttons can be exercised
                // on demand instead of waiting for the real (intermittent, environment-specific)
                // asset-load race this was built to catch.
                if debug::enabled() && std::env::var("KOTODAMA_TEST_OOPS").is_ok() {
                    let main_test = main.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(2500));
                        debug::log("KOTODAMA_TEST_OOPS: forcing a real navigation failure on main");
                        // Port 1 is Chromium-restricted (ERR_UNSAFE_PORT, blocked before any real
                        // navigation/document even starts -- doesn't reproduce the target failure).
                        // A closed high port gives a genuine ERR_CONNECTION_REFUSED, matching what
                        // this watchdog actually needs to catch.
                        let _ = main_test.eval("location.href = 'http://127.0.0.1:58193/';");
                    });
                }
                // Watchdog for a transient asset-load failure at cold start (WebView2 shows its own
                // `chrome-error://chromewebdata/` page if the very first navigation loses a race with
                // Tauri's own local asset server -- seen intermittently on this machine). One silent
                // reload attempt first; if that still didn't clear it, fall back to the branded page
                // (same OOPS_PAGE_JS the provider tabs use) instead of leaving Chromium's raw error up.
                {
                    let main_watch = main.clone();
                    let app_handle = handle.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(1200));
                        let _ = main_watch
                            .eval("if (location.protocol === 'chrome-error:') { location.reload(); }");
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        let _ = main_watch.eval(&browser::oops_page_js(&app_handle, true));
                        // Poll the window title for the Oops page's button sentinels -- see
                        // OOPS_PAGE_JS's doc comment for why title (not invoke): a chrome-error://
                        // document isn't the app's own origin, so a direct invoke() from it may be
                        // silently denied by the capability ACL, while title get/set has none.
                        // 100ms (not 300ms): a user clicking Riavvia/Segnala/Chiudi in quick
                        // succession can overwrite the title faster than a slower poll would ever
                        // observe an intermediate value (only the LAST click before a poll tick
                        // survives) -- seen for real: three fast clicks and only "close" landed.
                        for i in 0..5400 {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                            // Self-healing: re-inject every ~2s. Both scripts are idempotent no-ops
                            // on a normal page (protocol check) or an already-shown Oops page (id
                            // check) -- this only matters when the page navigated to a NEW failure
                            // after the one-shot injection above already ran (confirmed real: the
                            // Oops page's own "Segnala" fallback once navigated to a fake domain,
                            // landing on a fresh chrome-error the original one-shot never re-saw).
                            if i % 20 == 0 {
                                let _ = main_watch.eval(
                                    "if (location.protocol === 'chrome-error:') { location.reload(); }",
                                );
                                let _ = main_watch.eval(&browser::oops_page_js(&app_handle, true));
                            }
                            let Ok(title) = main_watch.title() else { continue };
                            if title.starts_with("__kt_oops") {
                                debug::log(format!("oops watchdog: title={title:?}"));
                            }
                            match title.strip_prefix("__kt_oops:") {
                                Some("restart") => {
                                    debug::log("oops watchdog: restart");
                                    restart_app(app_handle.clone());
                                }
                                Some("close") => {
                                    debug::log("oops watchdog: close");
                                    quit_app(app_handle.clone());
                                }
                                Some("issue") => {
                                    debug::log("oops watchdog: issue");
                                    use tauri_plugin_opener::OpenerExt;
                                    let _ = app_handle.opener().open_url(
                                        "https://github.com/Michel-IT/Kotodama/issues/new",
                                        None::<&str>,
                                    );
                                    let _ = main_watch
                                        .set_title("Kotodama • Ai Prompt Builder");
                                }
                                _ => {}
                            }
                        }
                    });
                }
                let _ = main.set_always_on_top(loaded.always_on_top);
                // macOS/Linux: join ALL Spaces/virtual desktops from creation (NSWindow
                // canJoinAllSpaces), so summoning always finds the window on the CURRENT one with no
                // relocation and no dragging to another Space. Must be set here (not only in
                // bring_to_front, where the window is already assigned to a Space). Windows can't do
                // this (no-op) -> there the hide/show recall in bring_to_front handles it.
                #[cfg(not(windows))]
                let _ = main.set_visible_on_all_workspaces(true);
            }

            // Non-visual init (after the show): tray + autostart sync.
            let autostart_on = {
                use tauri_plugin_autostart::ManagerExt;
                handle.autolaunch().is_enabled().unwrap_or(false)
            };
            // Fresh install: nothing was ever persisted, so there is no explicit user choice to
            // respect yet -- actually create the OS login item now, so autostart is really ON
            // from the very first launch (Settings::default().autostart is true, but the flag
            // alone does not register anything with the OS; without this an install would sit
            // "wants ON" until the user happened to open Settings and hit Save).
            let autostart_on = if fresh_install && !autostart_on {
                apply_autostart(&handle, true);
                true
            } else {
                autostart_on
            };
            // Il login-item/registro (gestito dal plugin autostart) e' la FONTE DI VERITA':
            // la spunta della tray e settings.json riflettono lo stato REALE. NON forziamo piu'
            // il registro al valore di settings.json: poteva essere "stale" (es. il frontend lo
            // sovrascriveva) e all'avvio DISABILITAVA l'autostart appena abilitato, togliendo la spunta.
            build_tray(&handle, autostart_on)?;
            if loaded.autostart != autostart_on {
                let snapshot = {
                    let st = app.state::<AppState>();
                    let mut g = st.settings.lock().unwrap();
                    g.autostart = autostart_on;
                    g.clone()
                };
                save_settings_bg(&handle, snapshot);
            }

            // Clipboard monitor + hotkeys (main with per-platform fallback + per-recipe set).
            clipboard::start(&handle);
            register_and_persist_hotkeys(&handle);

            // DEBUG-only: auto-open a provider a few seconds after start, via the
            // real JS path (like a double-click), so the debug tool can reproduce
            // issues without manual interaction. Set KOTO_AUTOOPEN=<provider key>
            // (e.g. openai, anthropic). No effect unless the env var is set.
            if let Ok(which) = std::env::var("KOTO_AUTOOPEN") {
                let which = if which.is_empty() { "openai".to_string() } else { which };
                if let Some(main) = handle.get_webview_window("main") {
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(6000));
                        debug::log(format!("auto-open provider: {which}"));
                        let _ = main.eval(format!(
                            "window.openProviderDirect && openProviderDirect('{which}')"
                        ));
                    });
                }
            }

            // DEBUG-only: auto-open each KOTO_AUTOPROBE=<key[,key...]> provider's compose page in
            // turn (spaced out) so on_page_finished can inject the INCOG discovery probe on each.
            if let Ok(list) = std::env::var("KOTO_AUTOPROBE") {
                if let Some(main) = handle.get_webview_window("main") {
                    let keys: Vec<String> = list.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                    std::thread::spawn(move || {
                        for (i, k) in keys.iter().enumerate() {
                            std::thread::sleep(std::time::Duration::from_millis(7000 + (i as u64) * 14000));
                            debug::log(format!("auto-probe open: {k}"));
                            let _ = main.eval(format!("window.openProviderDirect && openProviderDirect('{k}')"));
                        }
                    });
                }
            }

            // DEBUG-only: auto-fire a Kotodama broadcast to one provider with the incognito path
            // engaged, so the dev loop can validate the temp toggle from logs without UI. Set
            // KOTO_AUTOKOTO=<provider key> (e.g. anthropic). Fires once ~8s after start.
            if let Ok(list) = std::env::var("KOTO_AUTOKOTO") {
                if let Some(main) = handle.get_webview_window("main") {
                    let keys: Vec<String> = if list.is_empty() { vec!["anthropic".to_string()] }
                        else { list.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect() };
                    std::thread::spawn(move || {
                        for (i, k) in keys.iter().enumerate() {
                            std::thread::sleep(std::time::Duration::from_millis(if i == 0 { 8000 } else { 24000 }));
                            debug::log(format!("auto-kotodama broadcast: {k}"));
                            let _ = main.eval(format!("window.__ktAutoTest && __ktAutoTest('{k}')"));
                        }
                    });
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            browser::show_provider_tab,
            kotodama::kotodama_broadcast,
            kotodama::kotodama_push,
            kotodama::provider_login_probe,
            mark_provider_known,
            kotodama::kotodama_cancel,
            get_kotodama_sessions,
            save_kotodama_sessions,
            inline_toast,
            inline_finish,
            inline_fail,
            browser::close_provider_view,
            browser::set_provider_top_extra,
            browser::provider_reload,
            browser::provider_back,
            browser::set_provider_menu_labels,
            browser::provider_suppress,
            browser::provider_dock,
            open_download_path,
            reveal_download_path,
            accept_clipboard,
            hide_toast,
            quit_app,
            restart_app,
            app_write_clipboard,
            show_main,
            hide_main,
            get_settings,
            get_system_locale,
            open_url,
            set_settings,
            save_ui_state,
            set_tray_labels,
            get_recipes,
            save_recipes,
            get_fields,
            save_fields,
            check_for_update,
            install_update,
        ])
        .run(tauri::generate_context!())
        .expect("errore nell'avvio dell'applicazione Tauri");
}
