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
mod settings;
mod toast;

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
    /// Tray "Open" / "Quit" items, to localize their text at runtime (set_tray_labels).
    pub open_item: Mutex<Option<MenuItem<Wry>>>,
    pub quit_item: Mutex<Option<MenuItem<Wry>>>,
    /// Localized labels for the native provider context menu (set_provider_menu_labels).
    pub menu_labels: Mutex<browser::MenuLabels>,
}

// ============================ COMMANDS ============================

/// Brings the app to the front with the copied text already in the Instructions field.
/// Shared by the hotkey, the toast click and the tray click.
#[tauri::command]
fn accept_clipboard(app: AppHandle) -> Result<(), String> {
    let clipboard = app.state::<Clipboard>();
    let text = clipboard.read_text().unwrap_or_default();

    // Neutra con "Auto-invio" OFF: apri SOLO Kotodama (builder) per editare il testo;
    // niente apertura provider ne' invio. Le altre ricette / Neutra ON: processa e apri il provider.
    let edit_in_builder = {
        let st = app.state::<AppState>();
        let g = st.settings.lock().unwrap();
        g.recipe == "key:neutral" && !g.neutral_autosend
    };

    if let Some(main) = app.get_window("main") {
        browser::park_provider(&main);
        bring_to_front(&main);
        let _ = main.emit("app://provider-closed", ());
        if edit_in_builder {
            if !text.trim().is_empty() {
                let _ = main.emit("app://fill-clipboard", text); // solo builder, nessun provider
            }
        } else {
            let _ = main.emit("app://insert-clipboard", text);
        }
    }
    toast::hide(&app);
    Ok(())
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

    // Register the requested hotkey; if it is not registrable on this platform,
    // use the fallback that is actually active, so disk and UI stay faithful.
    let mut final_settings = settings;
    if let Some(active) = register_hotkey(&app, &final_settings.hotkey) {
        final_settings.hotkey = active;
    }

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
fn save_ui_state(app: AppHandle, provider: String, recipe: String, length: u32, tone: u32) {
    let snapshot = {
        let state = app.state::<AppState>();
        let mut g = state.settings.lock().unwrap();
        g.default_provider = provider;
        g.recipe = recipe;
        g.length = length;
        g.tone = tone;
        g.clone()
    };
    save_settings_bg(&app, snapshot);
}

/// Localizes the tray labels (Open / Start-on-login / Quit) in the app language.
/// Called by the frontend at startup and on every language change.
#[tauri::command]
fn set_tray_labels(app: AppHandle, open: String, autostart: String, quit: String) {
    if let Some(i) = app.state::<AppState>().open_item.lock().unwrap().as_ref() {
        let _ = i.set_text(open);
    }
    if let Some(i) = app.state::<AppState>().autostart_item.lock().unwrap().as_ref() {
        let _ = i.set_text(autostart);
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

/// Tries to register a single accelerator. `true` on success.
fn try_register_hotkey(app: &AppHandle, accel: &str) -> bool {
    use std::str::FromStr;
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

    let Ok(shortcut) = Shortcut::from_str(accel) else {
        return false;
    };
    let handle = app.clone();
    app.global_shortcut()
        .on_shortcut(shortcut, move |_app, _sc, event| {
            if event.state() == ShortcutState::Pressed {
                let _ = accept_clipboard(handle.clone());
            }
        })
        .is_ok()
}

/// (Re)registers the global hotkey with per-platform fallbacks.
/// Returns the accelerator ACTUALLY registered (so UI/toast stay faithful),
/// or `None` if none is registrable (the toast/tray fallback remains).
fn register_hotkey(app: &AppHandle, accel: &str) -> Option<String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let _ = app.global_shortcut().unregister_all();

    let mut candidates: Vec<&str> = vec![accel];
    candidates.extend(HOTKEY_FALLBACKS.iter().copied().filter(|f| *f != accel));

    for cand in candidates {
        if try_register_hotkey(app, cand) {
            if cand != accel {
                eprintln!("[hotkey] '{accel}' non registrabile su questa piattaforma; uso '{cand}'");
            }
            return Some(cand.to_string());
        }
    }
    eprintln!("[hotkey] nessun hotkey registrabile: usa il toast o la tray");
    None
}

/// Registers the hotkey and, if a fallback was used, persists the real one.
fn register_and_persist_hotkey(app: &AppHandle, requested: &str) {
    if let Some(active) = register_hotkey(app, requested) {
        if active != requested {
            let state = app.state::<AppState>();
            let snapshot = {
                let mut g = state.settings.lock().unwrap();
                g.hotkey = active;
                g.clone()
            };
            let _ = settings::save(app, &snapshot);
        }
    }
}

fn build_tray(app: &AppHandle, autostart_on: bool) -> tauri::Result<()> {
    use tauri::menu::{Menu, PredefinedMenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    // Etichette MONOLINGUA nella lingua dell'app (it/en hardcoded; en di default). Le bilingue
    // ("Apri / Open"...) erano troppo lunghe. Il frontend poi le localizza per TUTTE le lingue
    // via set_tray_labels (all'avvio e ad ogni cambio lingua).
    let it = {
        let st = app.state::<AppState>();
        let g = st.settings.lock().unwrap();
        g.language == "it"
    };
    let (open_lbl, auto_lbl, quit_lbl) = if it {
        ("Apri", "Apri al login", "Esci")
    } else {
        ("Open", "Start on login", "Quit")
    };
    let open_i = MenuItem::with_id(app, "open", open_lbl, true, None::<&str>)?;
    let login_i = CheckMenuItem::with_id(app, "autostart", auto_lbl, true, autostart_on, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_i = MenuItem::with_id(app, "quit", quit_lbl, true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_i, &login_i, &sep, &quit_i])?;

    // Store items: sync check + localize text at runtime.
    {
        let st = app.state::<AppState>();
        st.autostart_item.lock().unwrap().replace(login_i.clone());
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
            open_item: Mutex::new(None),
            quit_item: Mutex::new(None),
            menu_labels: Mutex::new(browser::MenuLabels::default()),
        })
        // App-level menu events: the provider's native context menu (browser.rs) puts
        // "Switch provider" items with ids "sw:<key>:<send|paste>". Copy/Cut/Paste/Select-all
        // are predefined items and act on the focused webview by themselves (no event here).
        .on_menu_event(|app, event| {
            let id = event.id.as_ref();
            if id == "downloads" {
                // Apre la modale "Gestore download" (in-app): il frontend sopprime la
                // webview provider e mostra la cronologia.
                if let Some(w) = app.get_window("main") {
                    let _ = w.emit("app://open-downloads", ());
                }
                return;
            }
            if let Some(rest) = id.strip_prefix("sw:") {
                // id = "sw:<key>:<open|paste|send>": switch provider with that mode.
                if let Some((key, mode)) = rest.rsplit_once(':') {
                    if let Some(w) = app.get_window("main") {
                        let _ = w.emit(
                            "app://switch-provider",
                            serde_json::json!({ "key": key, "mode": mode }),
                        );
                    }
                }
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();
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

            // Clipboard monitor + hotkey (with per-platform fallback).
            clipboard::start(&handle);
            register_and_persist_hotkey(&handle, &loaded.hotkey);

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

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            browser::open_provider_view,
            browser::close_provider_view,
            browser::set_provider_top_extra,
            browser::provider_reload,
            browser::provider_back,
            browser::provider_fill,
            browser::provider_paste,
            browser::provider_menu,
            browser::set_provider_menu_labels,
            browser::provider_suppress,
            browser::provider_dock,
            open_download_path,
            reveal_download_path,
            accept_clipboard,
            hide_toast,
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
