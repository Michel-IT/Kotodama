//! Feature 1 — AI provider inside the app as a full-window webview.
//!
//! The "provider" webview is a child of the main window (multi-webview,
//! `unstable` feature). It covers the whole window except the top strip
//! `TOPBAR_H` where the in-app browser bar (← / ⟳ / url) stays visible,
//! which lives in the main webview.
//!
//! Documented fallback: if multi-webview turns out to be unstable on an OS, one
//! can replace `add_child` with a full-window child `WebviewWindow`; the four
//! commands below remain the only public surface.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use tauri::webview::{PageLoadEvent, WebviewBuilder};
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, PhysicalPosition, PhysicalSize,
    Runtime, WebviewUrl, WebviewWindow, Window,
};
use tauri_plugin_opener::OpenerExt;   // open external links in the system browser

use crate::debug;

/// Persistent-tabs model: each provider gets its own child webview labelled `provider:<key>`, kept
/// alive and parked off-screen when not in front, so switching keeps each page. Only a recipe
/// request re-navigates a tab.
///
/// Key of the tab currently in the foreground, or `None` when the builder is shown.
fn active_provider() -> &'static Mutex<Option<String>> {
    static A: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(None))
}

/// Which tab (by key) must be brought on screen once its page finishes loading (anti-flash on a
/// navigate / first-open); consumed in `on_page_load` and the fallback thread.
fn pending_show_key() -> &'static Mutex<Option<String>> {
    static P: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// The active provider is temporarily "suppressed" (parked out of view) because an app modal
/// (the Download manager) is showing on top; stays "active" so its bounds restore afterwards.
static PROVIDER_SUPPRESSED: AtomicBool = AtomicBool::new(false);

/// Park Y: we move a provider webview well outside the window. It is a `set_position`
/// (NON-blocking, unlike `close()`/`hide()` on Windows once the page has navigated).
const PARK_Y: f64 = 100_000.0;

/// (Logical) height of the in-app browser bar in the main webview.
pub const TOPBAR_H: f64 = 46.0;

/// Per-provider child-webview label: `provider:<key>` (e.g. `provider:openai`).
pub fn provider_label(key: &str) -> String {
    format!("provider:{key}")
}
/// Key of the foreground tab, if any.
fn active_key() -> Option<String> {
    active_provider().lock().unwrap().clone()
}
/// The webview of the foreground tab, if any.
pub(crate) fn active_webview<R: Runtime, M: Manager<R>>(manager: &M) -> Option<tauri::Webview<R>> {
    active_key().and_then(|k| manager.get_webview(&provider_label(&k)))
}

/// WebView2 flags for the provider child-webview. `--disable-quic` must apply
/// HERE too (not only on the main): it is the provider webview that loads the
/// remote sites, and without this flag some domains fail with
/// ERR_QUIC_PROTOCOL_ERROR and stay blank. Aligned with `additionalBrowserArgs`
/// in tauri.conf.json.
/// `accept-lang` follows the OS language so provider sites (ChatGPT, Claude, …)
/// open in the user's language instead of a hardcoded one.
#[cfg(windows)]
pub fn provider_browser_args() -> String {
    let loc = sys_locale::get_locale().unwrap_or_else(|| "en-US".into()); // e.g. "fr-FR"
    let primary = loc.split('-').next().unwrap_or("en").to_string();      // e.g. "fr"
    format!("--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --disable-quic --accept-lang={loc},{primary},en-US,en")
}

/// "Clean Chrome desktop" User-Agent for the provider webview. Several sites
/// (in particular the Google/Gemini login) block or degrade embedded browsers
/// by recognizing the `Edg/`/WebView2 markers; a standard Chrome UA maximizes
/// compatibility and login success.
// User-Agent del webview provider. DEVE essere coerente col motore reale, altrimenti
// Google ("browser non sicuro") e Cloudflare ("verifica anti-bot") bloccano il login:
//   - Windows: WebView2 = Chromium  -> UA Chrome (coerente).
//   - macOS:   WKWebView = WebKit/Safari -> UA Safari macOS (coerente; un UA "Windows Chrome"
//              su WebKit crea l'incongruenza che fa scattare i blocchi).
#[cfg(target_os = "macos")]
pub const PROVIDER_UA: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Safari/605.1.15";
#[cfg(not(target_os = "macos"))]
pub const PROVIDER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Script injected into EVERY provider page (external sites):
/// - right-click -> OUR native context menu: preventDefault the page menu and navigate to the
///   `kotodama.menu` sentinel, which Rust `on_navigation` turns into a native `popup_menu`
///   (Copy/Paste/... + Switch provider), drawn OVER this child webview. Devtools shortcuts are
///   NOT blocked here: available in a dev build, off in release (Tauri gates them).
/// - sends links meant for a new tab (target="_blank") to the system default
///   browser, via a sentinel URL that the Rust `on_navigation` handler intercepts.
pub const NO_MENU_JS: &str = r#"
(function(){
  // Kill the "Leave site?" (beforeunload) dialog: our sentinel signalling assigns
  // location.href (blocked by Rust, the page never actually leaves), but a page with
  // composer text would pop its unload confirm on every sentinel. This runs BEFORE any
  // page script, so stopImmediatePropagation() silences their later-registered handlers.
  window.addEventListener('beforeunload', function(e){ e.stopImmediatePropagation(); }, true);
  setInterval(function(){ try { window.onbeforeunload = null; } catch(e){} }, 2000);
  document.addEventListener('contextmenu', function(e){
    e.preventDefault(); e.stopPropagation();
    window.location.href = 'https://kotodama.menu/';
  }, {capture:true});
  // External links (open-in-new-tab) -> system browser. Same-tab navigations
  // (SPA, login redirects) are left untouched so provider login keeps working.
  document.addEventListener('click', function(e){
    var a = e.target && e.target.closest && e.target.closest('a[href]');
    if(!a) return;
    // Download link (attributo download): NON dirottare al browser di sistema,
    // lascialo scaricare qui dentro (lo cattura on_download lato Rust).
    if(a.hasAttribute('download')) return;
    var href = a.href || '';
    if(!/^https?:\/\//i.test(href)) return;
    if(a.target !== '_blank') return;
    e.preventDefault(); e.stopPropagation();
    window.location.href = 'https://kotodama.external/open?u=' + encodeURIComponent(href);
  }, {capture:true});
})();
"#;

/// EXTRA vertical offset (logical px) below the topbar, to make room for a
/// native banner of the main webview (e.g. Claude login notice). 0 = no
/// banner. The banner lives in the main UI, so the provider must be pushed down.
fn provider_top_extra() -> &'static Mutex<f64> {
    static E: OnceLock<Mutex<f64>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(0.0))
}

/// Logical Y at which the provider webview starts: topbar + optional banner.
fn provider_top() -> f64 {
    TOPBAR_H + *provider_top_extra().lock().unwrap()
}

/// Width (logical px) reserved ON THE RIGHT for the docked Download panel. The
/// provider webview is narrowed by this much so the HTML side panel (main webview)
/// is no longer covered by the opaque provider. 0 = no dock (provider full width).
fn provider_dock_px() -> &'static Mutex<f64> {
    static D: OnceLock<Mutex<f64>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(0.0))
}

/// Logical size available below the top bar (+ banner), minus the docked panel width.
pub(crate) fn provider_bounds(window: &Window) -> Result<(f64, f64), String> {
    let size = window.inner_size().map_err(|e| e.to_string())?;
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let logical = size.to_logical::<f64>(scale);
    let w = (logical.width - *provider_dock_px().lock().unwrap()).max(0.0);
    Ok((w, (logical.height - provider_top()).max(0.0)))
}

/// Parks the currently-foreground tab off-screen (does NOT clear `active_provider`). Used before
/// bringing another tab in front, and when returning to the builder (the tab stays alive).
fn park_active<R: Runtime, M: Manager<R>>(manager: &M) {
    if let Some(wv) = active_webview(manager) {
        let _ = wv.set_position(LogicalPosition::new(0.0, PARK_Y));
    }
}

/// Brings the tab `key` on screen (below the topbar, at full bounds), marks it the active tab and
/// notifies the frontend. The caller parks the previously-active tab first (`park_active`).
fn show_tab(window: &Window, key: &str) {
    // Showing a tab SUPERSEDES any queued show: while another tab was still loading the user may
    // have switched away, and letting that older pending show land later would put the wrong
    // provider in front of the one now selected (fast provider switching).
    *pending_show_key().lock().unwrap() = None;
    if let Some(wv) = window.get_webview(&provider_label(key)) {
        if let Ok((w, h)) = provider_bounds(window) {
            let _ = wv.set_position(LogicalPosition::new(0.0, provider_top()));
            let _ = wv.set_size(LogicalSize::new(w, h));
        }
        let _ = wv.show();
    }
    *active_provider().lock().unwrap() = Some(key.to_string());
    *last_provider_bounds().lock().unwrap() = None; // force next reposition
    let _ = window.emit("app://provider-loaded", ());
}

/// Localized labels for the native provider menu, pushed by the frontend in the active app language
/// (field names match the i18n keys). Stored in AppState; read when the menu is built. Defaults are
/// empty -> `show_provider_menu` falls back to English until the frontend sets them.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct MenuLabels {
    #[serde(default)]
    pub copy: String,
    #[serde(default)]
    pub cut: String,
    #[serde(default)]
    pub paste: String,
    #[serde(default, rename = "selectAll")]
    pub select_all: String,
    #[serde(default, rename = "switchProvider")]
    pub switch: String,
    #[serde(default, rename = "openOnly")]
    pub open_only: String,
    #[serde(default, rename = "openAndSend")]
    pub open_send: String,
    #[serde(default, rename = "openAndPaste")]
    pub open_paste: String,
    #[serde(default, rename = "copyUrl")]
    pub copy_url: String,
    #[serde(default)]
    pub downloads: String,
}

/// Frontend pushes the menu labels (all app languages) at startup and on every language change,
/// mirroring `set_tray_labels`. Stored for `show_provider_menu` to use.
#[tauri::command]
pub fn set_provider_menu_labels(window: Window, labels: MenuLabels) {
    if let Some(state) = window.try_state::<crate::AppState>() {
        *state.menu_labels.lock().unwrap() = labels;
    }
}

/// Providers offered in the right-click "Switch provider" submenu (key -> display name).
/// Keys match the frontend PROVIDERS registry; "other"/manual is intentionally excluded.
const SWITCH_PROVIDERS: &[(&str, &str)] = &[
    ("openai", "ChatGPT"),
    ("anthropic", "Claude"),
    ("grok", "Grok"),
    ("gemini", "Gemini"),
    ("perplexity", "Perplexity"),
    ("qwen", "Qwen"),
    ("deepseek", "DeepSeek"),
    ("zai", "Z.ai"),
];

/// Builds + pops OUR native context menu over the provider child webview (triggered by the
/// right-click sentinel intercepted in `on_navigation`). A native menu draws ABOVE the opaque
/// child webview (an HTML menu cannot). It carries just two entries: "Download manager" (id
/// `downloads` → opens the in-app panel) and the "Switch provider" submenu (custom ids
/// `sw:<key>:send` / `sw:<key>:paste`), both handled by the app-level `on_menu_event`. Must run
/// on the main thread (caller uses `run_on_main_thread`).
pub fn show_provider_menu(window: &Window) {
    use tauri::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
    let app = window.app_handle().clone();
    // Labels come from the frontend in the ACTIVE app language (set_provider_menu_labels, all langs).
    // English is only a fallback for the brief window before labels arrive.
    let l = app.state::<crate::AppState>().menu_labels.lock().unwrap().clone();
    let pick = |s: &str, fb: &str| if s.trim().is_empty() { fb.to_string() } else { s.to_string() };
    let switch_t = pick(&l.switch, "Switch provider");
    let open_only_t = pick(&l.open_only, "Open");
    let open_paste_t = pick(&l.open_paste, "Open + paste");
    let send_t = pick(&l.open_send, "Open + paste + send");
    let downloads_t = pick(&l.downloads, "Download manager");

    let build = || -> tauri::Result<Menu<tauri::Wry>> {
        // Gestore download (apre il pannello in-app) + separatore + "Cambia provider".
        let downloads = MenuItem::with_id(&app, "downloads", &downloads_t, true, None::<&str>)?;
        let sep = PredefinedMenuItem::separator(&app)?;

        // "Switch provider" -> one submenu per provider -> [Open, Open+paste, Open+paste+send].
        let mut subs: Vec<Submenu<tauri::Wry>> = Vec::new();
        for (key, name) in SWITCH_PROVIDERS {
            let open_i =
                MenuItem::with_id(&app, format!("sw:{key}:open"), &open_only_t, true, None::<&str>)?;
            let paste_i =
                MenuItem::with_id(&app, format!("sw:{key}:paste"), &open_paste_t, true, None::<&str>)?;
            let send_i =
                MenuItem::with_id(&app, format!("sw:{key}:send"), &send_t, true, None::<&str>)?;
            subs.push(Submenu::with_items(&app, *name, true, &[&open_i, &paste_i, &send_i])?);
        }
        let sub_refs: Vec<&dyn IsMenuItem<tauri::Wry>> =
            subs.iter().map(|s| s as &dyn IsMenuItem<tauri::Wry>).collect();
        let switch = Submenu::with_items(&app, &switch_t, true, &sub_refs)?;

        Menu::with_items(&app, &[&downloads, &sep, &switch])
    };

    match build() {
        Ok(menu) => {
            if let Err(e) = window.popup_menu(&menu) {
                debug::log(format!("popup_menu error: {e}"));
            }
        }
        Err(e) => debug::log(format!("build provider menu error: {e}")),
    }
}

/// Builds + pops the EDITING context menu (Cut / Copy / Paste / Select all) over the provider
/// child webview, triggered by right-click. These are PredefinedMenuItems: they act on the
/// focused webview by themselves (no `on_menu_event`). Localized via `MenuLabels`, English
/// fallback. A native menu is required to draw ABOVE the opaque child webview. Main thread only.
pub fn show_context_menu(window: &Window) {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    let app = window.app_handle().clone();
    let l = app.state::<crate::AppState>().menu_labels.lock().unwrap().clone();
    let pick = |s: &str, fb: &str| if s.trim().is_empty() { fb.to_string() } else { s.to_string() };
    let cut_t = pick(&l.cut, "Cut");
    let copy_t = pick(&l.copy, "Copy");
    let paste_t = pick(&l.paste, "Paste");
    let select_all_t = pick(&l.select_all, "Select All");
    let copy_url_t = pick(&l.copy_url, "Copy URL");

    let build = || -> tauri::Result<Menu<tauri::Wry>> {
        let cut = PredefinedMenuItem::cut(&app, Some(&cut_t))?;
        let copy = PredefinedMenuItem::copy(&app, Some(&copy_t))?;
        let paste = PredefinedMenuItem::paste(&app, Some(&paste_t))?;
        let sep = PredefinedMenuItem::separator(&app)?;
        let select_all = PredefinedMenuItem::select_all(&app, Some(&select_all_t))?;
        let sep2 = PredefinedMenuItem::separator(&app)?;
        // "Copy URL": custom id, handled in the app-level on_menu_event (copies the provider's URL).
        let copy_url = MenuItem::with_id(&app, "copy_url", &copy_url_t, true, None::<&str>)?;
        Menu::with_items(&app, &[&cut, &copy, &paste, &sep, &select_all, &sep2, &copy_url])
    };
    match build() {
        Ok(menu) => {
            if let Err(e) = window.popup_menu(&menu) {
                debug::log(format!("context popup_menu error: {e}"));
            }
        }
        Err(e) => debug::log(format!("build context menu error: {e}")),
    }
}

/// Pops OUR native provider menu on demand (the in-app bar's ⇄ button). Same menu as the
/// right-click; pops at the cursor (which is on the button). Runs on the main thread.
#[tauri::command]
pub fn provider_menu(window: Window) -> Result<(), String> {
    let w = window.clone();
    window
        .app_handle()
        .run_on_main_thread(move || show_provider_menu(&w))
        .map_err(|e| e.to_string())
}

/// Payload for the floating provider-menu window: provider rows (each with inline SVG icon + color) +
/// localized labels + resolved theme colors (so the menu window matches the app theme without
/// duplicating the theme definitions). Passed as-is from the main webview.
#[derive(serde::Deserialize, serde::Serialize, Clone)]
pub struct ProviderMenuData {
    providers: serde_json::Value,
    labels: serde_json::Value,
    theme: serde_json::Value,
}

/// Shows the floating provider menu (window `menu`) anchored under the ⇄ button, drawn ABOVE the
/// provider webview WITHOUT hiding it (a separate always-on-top window, unlike the old HTML overlay
/// that had to park the provider). `ax`/`ay` = the button's bottom-right in the main webview's
/// LOGICAL viewport coords; converted to screen physical px via the main window's client origin.
#[tauri::command]
pub fn show_provider_menu_window(
    app: AppHandle,
    data: ProviderMenuData,
    ax: f64,
    ay: f64,
) -> Result<(), String> {
    debug::log(format!("show_provider_menu_window: entry ax={ax} ay={ay}"));
    let Some(menu) = app.get_webview_window("menu") else {
        debug::log("show_provider_menu_window: MENU WINDOW MISSING");
        return Err("menu window missing".into());
    };
    // get_window (NOT get_webview_window): with the provider child-webview the "main" window has 2
    // webviews and get_webview_window("main") returns None -> the menu would never show.
    let Some(main) = app.get_window("main") else {
        debug::log("show_provider_menu_window: MAIN WINDOW MISSING");
        return Err("main window missing".into());
    };
    let scale = main.scale_factor().map_err(|e| e.to_string())?;
    let inner = main.inner_position().map_err(|e| e.to_string())?;
    // Cover the monitor the app is on with a transparent full-screen backdrop, so a click ANYWHERE
    // outside the menu box dismisses it (real menu behaviour, multi-monitor safe). menu.html then
    // places the menu box inside it at the ⇄ button anchor.
    let Some(mon) = main.current_monitor().map_err(|e| e.to_string())? else {
        return Err("no current monitor".into());
    };
    let mpos = mon.position();
    let msz = mon.size();
    // Button anchor (bottom-right) relative to the monitor, in LOGICAL px for the CSS in menu.html.
    let anchor_x = (inner.x + (ax * scale) as i32 - mpos.x) as f64 / scale;
    let anchor_y = (inner.y + (ay * scale) as i32 - mpos.y) as f64 / scale;
    let _ = menu.emit(
        "menu://data",
        serde_json::json!({
            "providers": data.providers,
            "labels": data.labels,
            "theme": data.theme,
            "anchorX": anchor_x,
            "anchorY": anchor_y,
        }),
    );
    debug::log(format!(
        "show_provider_menu_window: anchor=({anchor_x:.0},{anchor_y:.0}) monitor=({},{}) {}x{} scale={scale}",
        mpos.x, mpos.y, msz.width, msz.height
    ));
    *menu_shown_at().lock().unwrap() = Some(std::time::Instant::now()); // arm the anti-flash guard
    let _ = menu.set_position(PhysicalPosition::new(mpos.x, mpos.y));
    let _ = menu.set_size(PhysicalSize::new(msz.width, msz.height));
    let _ = menu.show();
    let _ = menu.set_focus();
    debug::log(format!("show_provider_menu_window: shown, visible={:?}", menu.is_visible()));
    Ok(())
}

/// Diagnostic hook: the menu window (menu.html) calls this so its lifecycle shows up in the
/// gated `[KDBG]` log (the menu window has no console we can read).
#[tauri::command]
pub fn menu_log(msg: String) {
    debug::log(format!("[menu.html] {msg}"));
}

/// Menu window → a provider was chosen: tell the main webview (reuses `app://switch-provider`) and hide the menu.
#[tauri::command]
pub fn menu_pick(app: AppHandle, key: String, mode: String) -> Result<(), String> {
    // get_window: the "main" window has 2 webviews while a provider is open (get_webview_window = None).
    if let Some(w) = app.get_window("main") {
        let _ = w.emit("app://switch-provider", serde_json::json!({ "key": key, "mode": mode }));
    }
    if let Some(m) = app.get_webview_window("menu") {
        let _ = m.hide();
    }
    Ok(())
}

/// Menu window → "Download manager" chosen.
#[tauri::command]
pub fn menu_downloads(app: AppHandle) -> Result<(), String> {
    // get_window: the "main" window has 2 webviews while a provider is open (get_webview_window = None).
    if let Some(w) = app.get_window("main") {
        let _ = w.emit("app://open-downloads", ());
    }
    if let Some(m) = app.get_webview_window("menu") {
        let _ = m.hide();
    }
    Ok(())
}

/// Menu window → dismissed (focus lost / click-outside / Esc).
#[tauri::command]
pub fn menu_dismiss(app: AppHandle) -> Result<(), String> {
    if let Some(m) = app.get_webview_window("menu") {
        let _ = m.hide();
    }
    Ok(())
}

/// When the floating menu was last shown — anti-flash guard for the focus-lost dismiss below.
fn menu_shown_at() -> &'static Mutex<Option<std::time::Instant>> {
    static M: OnceLock<Mutex<Option<std::time::Instant>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(None))
}

/// Hides the floating provider-menu window (single-webview window: get_webview_window works).
pub fn hide_menu_window<R: Runtime, M: Manager<R>>(manager: &M) {
    if let Some(m) = manager.get_webview_window("menu") {
        let _ = m.hide();
    }
}

/// The menu window lost OS focus -> dismiss it. Ignores the transient toggle right after show
/// (~250ms) so opening the menu does not immediately close it.
pub fn on_menu_focus_lost<R: Runtime, M: Manager<R>>(manager: &M) {
    if let Some(t) = *menu_shown_at().lock().unwrap() {
        if t.elapsed().as_millis() < 250 {
            return;
        }
    }
    hide_menu_window(manager);
}

/// Builds + adds (PARKED off-screen) the persistent child webview for provider `key`, navigated to
/// `url`. Its `on_page_load` brings it on screen once loading finishes IF it is still the pending
/// tab (anti-flash). Shared by `open_provider_view` (recipe) and `show_provider_tab` (first switch).
pub(crate) fn create_tab(window: &Window, key: &str, url: tauri::Url, w: f64, h: f64) -> Result<(), String> {
    let win_for_nav = window.clone();
    let key_load = key.to_string();
    let builder = WebviewBuilder::new(provider_label(key), WebviewUrl::External(url))
        .user_agent(PROVIDER_UA)
        .initialization_script(NO_MENU_JS)
        .on_navigation(move |u| {
            debug::log(format!("on_navigation -> {u}"));
            // Right-click sentinel -> our native EDITING menu (on the main thread). Nav is blocked.
            if u.host_str() == Some("kotodama.menu") {
                let w = win_for_nav.clone();
                let _ = win_for_nav.app_handle().run_on_main_thread(move || show_context_menu(&w));
                return false;
            }
            // External links funneled here open in the system browser; in-app nav blocked.
            if u.host_str() == Some("kotodama.external") {
                if let Some((_, ext)) = u.query_pairs().find(|(k, _)| k == "u") {
                    let _ = win_for_nav.app_handle().opener().open_url(ext.into_owned(), None::<&str>);
                }
                return false;
            }
            // Kotodama broadcast: harvested answer chunks / progress heartbeats. Nav blocked.
            if u.host_str() == Some("kotodama.result") {
                crate::kotodama::on_result_url(&win_for_nav, &u);
                return false;
            }
            let _ = win_for_nav.emit("app://provider-url", u.to_string());
            true
        })
        .on_page_load(move |webview, payload| {
            debug::log(format!("on_page_load {:?} url={}", payload.event(), payload.url()));
            // When THIS tab is the pending one and finished loading, bring it on screen.
            if payload.event() == PageLoadEvent::Finished {
                let show = {
                    let mut p = pending_show_key().lock().unwrap();
                    if p.as_deref() == Some(key_load.as_str()) { *p = None; true } else { false }
                };
                if show {
                    show_tab(&webview.window(), &key_load);
                }
                // Kotodama broadcast: run any fill+harvest injection queued for this tab.
                crate::kotodama::on_page_finished(&webview, &key_load);
            }
        })
        // Abilita i DOWNLOAD nella webview del provider (senza handler wry/WebView2 li scarta):
        // salviamo in Download/Kotodama e notifichiamo il frontend.
        .on_download(|webview, event| {
            use tauri::webview::DownloadEvent;
            match event {
                DownloadEvent::Requested { url, destination } => {
                    debug::log(format!("on_download REQUESTED url={url}"));
                    if let Ok(dir) = webview.app_handle().path().download_dir() {
                        let name = destination
                            .file_name()
                            .map(|n| n.to_os_string())
                            .unwrap_or_else(|| std::ffi::OsString::from("download"));
                        let kdir = dir.join("Kotodama");
                        let _ = std::fs::create_dir_all(&kdir);
                        *destination = kdir.join(name);
                    }
                    debug::log(format!("on_download DEST={}", destination.display()));
                    let name = destination
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "download".to_string());
                    let _ = webview.app_handle().emit(
                        "app://provider-download-start",
                        serde_json::json!({ "name": name, "url": url.to_string() }),
                    );
                    true
                }
                DownloadEvent::Finished { url, path, success } => {
                    let p = path.map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                    debug::log(format!("on_download FINISHED ok={success} url={url} path={p}"));
                    if success {
                        use tauri_plugin_notification::NotificationExt;
                        let name = std::path::Path::new(&p)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| p.clone());
                        let _ = webview
                            .app_handle()
                            .notification()
                            .builder()
                            .title("Kotodama")
                            .body(format!("Download/Kotodama: {name}"))
                            .show();
                    }
                    let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                    let _ = webview.app_handle().emit(
                        "app://provider-download",
                        serde_json::json!({ "ok": success, "path": p, "url": url.to_string(), "size": size }),
                    );
                    true
                }
                _ => true,
            }
        });
    // WebView2 args are set process-wide in lib.rs::run() (WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS),
    // so every webview shares identical arguments; setting them per-webview would blank it.
    let _child = window
        .add_child(builder, LogicalPosition::new(0.0, PARK_Y), LogicalSize::new(w, h))
        .map_err(|e| e.to_string())?;
    // macOS: keep this provider webview running at full speed even while parked off-screen / with
    // the host window minimized (the inline transform relies on it scraping in the background).
    #[cfg(target_os = "macos")]
    crate::mac_disable_occlusion(&_child);
    Ok(())
}

/// Fallback: if `on_page_load Finished` never fires (cached page/redirect), show the pending tab
/// anyway after a while so we don't stay stuck on the loading overlay.
/// ARM IT BEFORE the (possibly slow) `create_tab`: on a slower machine creating a WebView2 child can
/// take seconds, and arming afterwards would push the deadline that much further while the user
/// stares at the loading overlay.
fn spawn_show_fallback(window: &Window, key: String) {
    let app = window.app_handle().clone();
    let win = window.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(2200));
        let show = {
            let mut p = pending_show_key().lock().unwrap();
            if p.as_deref() == Some(key.as_str()) { *p = None; true } else { false }
        };
        if show {
            debug::log("show_tab via FALLBACK (on_page_load Finished never fired)");
            let _ = app.run_on_main_thread(move || show_tab(&win, &key));
        }
    });
}

/// RECIPE request: navigate provider `key`'s tab to `url` (a fresh chat) and bring it in front.
/// Creates the tab if it doesn't exist. Async: creating a WebView2 webview in a sync command
/// deadlocks on Windows.
#[tauri::command]
pub async fn open_provider_view(window: Window, key: String, url: String) -> Result<(), String> {
    debug::log(format!("open_provider_view key={key} url={url}"));
    let label = provider_label(&key);
    let parsed = url.parse::<tauri::Url>().map_err(|e| e.to_string())?;
    let (w, h) = provider_bounds(&window)?;
    crate::kotodama::abort_key(&window, &key); // re-navigation kills any in-flight harvest on this tab
    park_active(&window); // park whatever tab is currently in front
    if let Some(webview) = window.get_webview(&label) {
        // Existing tab: park (hide the old chat) and re-navigate; only show it once the NEW chat has
        // loaded, otherwise the previous conversation would flash.
        *pending_show_key().lock().unwrap() = Some(key.clone());
        spawn_show_fallback(&window, key.clone()); // armed BEFORE the (slow) navigate
        let _ = webview.set_position(LogicalPosition::new(0.0, PARK_Y));
        let _ = webview.set_size(LogicalSize::new(w, h));
        webview.navigate(parsed).map_err(|e| e.to_string())?;
    } else {
        // Brand-new tab: no old page to hide -> show it right away (instant, page loads on screen).
        create_tab(&window, &key, parsed, w, h)?;
        show_tab(&window, &key);
    }
    window.emit("app://provider-opened", &url).map_err(|e| e.to_string())?;
    Ok(())
}

/// SWITCH to provider `key` (header icon strip / double-click tile / ⇄ "Open"): if its tab already
/// exists, bring it on screen AS-IS (keeps its page, NO reload); otherwise create it navigated to
/// `base_url` and show it once loaded. Async for the create path (WebView2 deadlock on Windows).
#[tauri::command]
pub async fn show_provider_tab(window: Window, key: String, base_url: String) -> Result<(), String> {
    debug::log(format!("show_provider_tab key={key}"));
    if window.get_webview(&provider_label(&key)).is_some() {
        park_active(&window);
        show_tab(&window, &key); // keep the existing page
        Ok(())
    } else {
        let parsed = base_url.parse::<tauri::Url>().map_err(|e| e.to_string())?;
        let (w, h) = provider_bounds(&window)?;
        park_active(&window);
        create_tab(&window, &key, parsed, w, h)?;
        // Brand-new tab: nothing to flash (no old page), so bring it on screen IMMEDIATELY and let
        // the user watch the provider's own page load, instead of staring at our spinner. Waiting
        // for PageLoadEvent::Finished here is what made a first open feel slow.
        show_tab(&window, &key);
        window.emit("app://provider-opened", &base_url).map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Returns to the builder by "parking" the active tab out of view (all tabs stay ALIVE). (← Builder / Esc)
/// NB: we use `set_position` (non-blocking), NOT `hide()`/`close()`, which on Windows freeze once the
/// page has navigated.
#[tauri::command]
pub fn close_provider_view(window: Window) -> Result<(), String> {
    *pending_show_key().lock().unwrap() = None; // cancel any pending show
    park_active(&window); // park the front tab (kept alive, resumed on next switch)
    *active_provider().lock().unwrap() = None;
    hide_menu_window(&window); // closing the provider also dismisses the floating ⇄ menu
    window
        .emit("app://provider-closed", ())
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Reloads the embedded page.
#[tauri::command]
pub fn provider_reload(window: Window) -> Result<(), String> {
    if let Some(webview) = active_webview(&window) {
        webview
            .eval("location.reload()")
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Navigates the embedded page back in its history (← button in the in-app bar).
#[tauri::command]
pub fn provider_back(window: Window) -> Result<(), String> {
    if let Some(webview) = active_webview(&window) {
        webview
            .eval("history.back()")
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Auto-fill: inserts the prompt into the provider page's input and (optionally) submits.
/// RESILIENT LOOP (~60s, 400ms ticks), designed for cold pages hydrating late:
/// - re-picks the composer every tick (hydration may REPLACE the node);
/// - re-fills whenever the field is empty again (hydration can WIPE a too-early fill);
/// - retries submit (Enter + send-button click) throttled, and stops ONLY when the
///   message actually APPEARS in the page thread (the one reliable success signal);
/// - `?q=`-prefilled composers just get submitted.
/// `fill_js` builds the script (also reused by the Kotodama broadcast gateway);
/// `fill_impl` runs it on the ACTIVE tab.
pub(crate) fn fill_js(text: &str, send: bool) -> Result<String, String> {
    let json = serde_json::to_string(text).map_err(|e| e.to_string())?;
    Ok(format!("var __apb_text = {json}; var __apb_send = {send};")
        + r#"
(function(){
  var text = __apb_text, send = __apb_send;
  var HEAD = text.trim().replace(/\s+/g,' ').slice(0, 60);
  function getVal(el){ return (el.value !== undefined ? el.value : el.innerText) || ''; }
  // Only VISIBLE inputs: ChatGPT keeps a hidden legacy <textarea> in the DOM that would
  // otherwise be picked over the real (contenteditable) composer and swallow the fill.
  function pickComposer(){
    var sels = ['textarea:not([readonly]):not([aria-hidden="true"])', '[contenteditable="true"]', 'div[role="textbox"]'];
    for (var i=0;i<sels.length;i++){
      var els = document.querySelectorAll(sels[i]);
      for (var j=0;j<els.length;j++){ if (els[j].offsetParent !== null) return els[j]; }
    }
    return null;
  }
  function vis(b){ return b && !b.disabled && b.getAttribute('aria-disabled')!=='true' && b.offsetParent !== null; }
  // Send button, fully LANGUAGE-INDEPENDENT (no per-language strings). Primary send is Enter
  // (universal); this is only the fallback. Uses stable non-linguistic signals:
  //   1) data-testid / type=submit  (language-neutral attributes)
  //   2) GEOMETRY: the rightmost small icon button on the composer's bottom row = the send
  //      button in EVERY language (bottom-right is where every chat UI puts it).
  function findSendBtn(){
    var sels = ['button[data-testid="send-button"]','button[data-testid*="send" i]','button[type="submit"]:not([disabled])'];
    for (var i=0;i<sels.length;i++){ var b=document.querySelector(sels[i]); if (vis(b)) return b; }
    var el = pickComposer(); if (!el || !getVal(el).length) return null;
    var cr = el.getBoundingClientRect();
    var all = document.querySelectorAll('button'), best=null, bestX=-1e9;
    for (var k=0;k<all.length;k++){
      var c=all[k]; if (!vis(c) || !c.querySelector('svg')) continue;
      var r=c.getBoundingClientRect();
      if (r.width<14 || r.width>90) continue;                 // send buttons are small square icons
      if (r.top >= cr.top-10 && r.top <= cr.bottom+72 && r.left >= cr.right-140){
        if (r.left > bestX){ bestX=r.left; best=c; }          // rightmost wins
      }
    }
    return best;
  }
  // TRUE once the sent message is visible in the page OUTSIDE the composer: the only
  // reliable "accepted" signal (composer emptying alone can also mean hydration wiped it).
  function delivered(){
    try {
      var el = pickComposer();
      if (el && getVal(el).replace(/\s+/g,' ').indexOf(HEAD) !== -1) return false; // still (only) in composer
      return ((document.body && document.body.innerText) || '').replace(/\s+/g,' ').indexOf(HEAD) !== -1;
    } catch(e){ return false; }
  }
  function fill(el){
    var isText = (el.tagName === 'TEXTAREA' || el.value !== undefined);
    try { el.focus(); } catch(e){}
    if (isText) {
      try {
        var set = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
        set.call(el, text);
      } catch (e) { el.value = text; }
      el.dispatchEvent(new Event('input', { bubbles: true }));
    } else {
      // contenteditable (ProseMirror/Quill, e.g. Claude): a synthetic PASTE is the most
      // reliable insertion - the editor's OWN paste handler updates its internal state, so
      // the send button ENABLES and Enter sends. execCommand no longer registers on some
      // React editors. Fall back to execCommand if the paste didn't take.
      try { document.execCommand('selectAll', false, null); } catch(e){}
      try {
        var dt = new DataTransfer(); dt.setData('text/plain', text);
        el.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
      } catch (e) {}
      // verify + fallback
      if ((el.innerText||'').replace(/\s+/g,' ').indexOf(text.trim().replace(/\s+/g,' ').slice(0,20)) === -1) {
        try { document.execCommand('selectAll', false, null); document.execCommand('insertText', false, text); }
        catch (e) { el.textContent = text; }
        try { el.dispatchEvent(new InputEvent('input', { bubbles: true, inputType:'insertText', data: text })); } catch(e){}
      }
    }
  }
  function submitOnce(el){
    try { el.focus(); } catch(e){}                          // Claude/ProseMirror ignora l'Invio senza focus
    if (el.value === undefined) {
      // rich editor precompilato: caret in fondo + input, così React abilita l'invio
      try { var rng = document.createRange(); rng.selectNodeContents(el); rng.collapse(false); var sel = window.getSelection(); sel.removeAllRanges(); sel.addRange(rng); } catch (e) {}
      el.dispatchEvent(new InputEvent('input', { bubbles: true }));
    }
    ['keydown','keypress','keyup'].forEach(function(t){
      el.dispatchEvent(new KeyboardEvent(t, {key:'Enter', code:'Enter', keyCode:13, which:13, bubbles:true, cancelable:true}));
    });
    setTimeout(function(){
      if (delivered()) return;
      var el2 = pickComposer();
      if (el2 && getVal(el2).trim().length) { var b = findSendBtn(); if (b) b.click(); }
    }, 300);
  }
  var ticks = 0, lastSubmit = -10;
  var iv = setInterval(function(){
    if (window.__ktHoldFill) return;                         // temp-chat toggle in progress: wait
    ticks++;
    if (delivered()) { clearInterval(iv); return; }          // accepted and visible -> done
    var el = pickComposer();
    if (!el) { if (ticks > 150) clearInterval(iv); return; }
    var val = getVal(el).trim();
    if (val.length === 0) {
      fill(el);                                              // (re)fill: heals hydration wipes
    } else if (send && ticks - lastSubmit >= 3) {
      lastSubmit = ticks;                                    // throttle submit attempts (~1.2s)
      submitOnce(el);
    } else if (!send) {
      clearInterval(iv); return;                             // paste-only: text in place, stop
    }
    if (ticks > 150) clearInterval(iv);                      // ~60s budget
  }, 400);
})();
"#)
}

fn fill_impl(window: &Window, text: String, send: bool) -> Result<(), String> {
    if let Some(webview) = active_webview(window) {
        let js = fill_js(&text, send)?;
        webview.eval(&js).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Fill the provider AND submit (normal open + autosend). Original signature -> autosend unchanged.
#[tauri::command]
pub fn provider_fill(window: Window, text: String) -> Result<(), String> {
    fill_impl(&window, text, true)
}

/// Fill the provider WITHOUT submitting ("switch provider -> open and paste").
#[tauri::command]
pub fn provider_paste(window: Window, text: String) -> Result<(), String> {
    fill_impl(&window, text, false)
}

/// Sets the extra offset (logical px) below the topbar and immediately
/// repositions the provider webview if it is on screen. Used by the native
/// banner (Claude login notice): the banner lives in the main webview, so the
/// provider must be pushed down by `px` to avoid ending up under it.
/// `px = 0` → no banner (provider goes back up).
#[tauri::command]
pub fn set_provider_top_extra(window: Window, px: f64) -> Result<(), String> {
    *provider_top_extra().lock().unwrap() = px.max(0.0);
    *last_provider_bounds().lock().unwrap() = None; // invalidate cache → force reposition
    if active_key().is_some() {
        if let Some(webview) = active_webview(&window) {
            let (w, h) = provider_bounds(&window)?;
            let _ = webview.set_position(LogicalPosition::new(0.0, provider_top()));
            let _ = webview.set_size(LogicalSize::new(w, h));
        }
    }
    Ok(())
}

/// Temporarily hides (`on=true`) or restores (`on=false`) the provider webview so an
/// app modal (Download manager) can show on top of it — the provider webview is opaque
/// and would otherwise cover any HTML overlay. Restore repositions to the current bounds.
#[tauri::command]
pub fn provider_suppress(window: Window, on: bool) -> Result<(), String> {
    PROVIDER_SUPPRESSED.store(on, Ordering::Relaxed);
    if let Some(webview) = active_webview(&window) {
        if on {
            let _ = webview.set_position(LogicalPosition::new(0.0, PARK_Y));
        } else if active_key().is_some() {
            *last_provider_bounds().lock().unwrap() = None; // force reposition
            let (w, h) = provider_bounds(&window)?;
            let _ = webview.set_position(LogicalPosition::new(0.0, provider_top()));
            let _ = webview.set_size(LogicalSize::new(w, h));
        }
    }
    Ok(())
}

/// Docks (`px>0`) or undocks (`px=0`) the Download side panel: narrows the provider
/// webview on the right by `px` logical pixels so the HTML panel (main webview) shows
/// beside it instead of behind it. Repositions the provider immediately if on screen.
#[tauri::command]
pub fn provider_dock(window: Window, px: f64) -> Result<(), String> {
    *provider_dock_px().lock().unwrap() = px.max(0.0);
    *last_provider_bounds().lock().unwrap() = None; // force reposition
    if active_key().is_some() {
        if let Some(webview) = active_webview(&window) {
            let (w, h) = provider_bounds(&window)?;
            let _ = webview.set_position(LogicalPosition::new(0.0, provider_top()));
            let _ = webview.set_size(LogicalSize::new(w, h));
        }
    }
    Ok(())
}

/// Last bounds applied to the provider (to skip redundant set_size/set_position:
/// Windows emits `Resized` even duplicated / with unchanged measurements).
fn last_provider_bounds() -> &'static Mutex<Option<(f64, f64)>> {
    static B: OnceLock<Mutex<Option<(f64, f64)>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(None))
}

/// Keeps the child webview's bounds aligned with the window resize.
pub fn resize_provider(window: &WebviewWindow) {
    // If the provider is parked (builder on screen) or suppressed (a modal is on
    // top), do NOT bring it back up on a resize: it would cover the builder/modal.
    if active_key().is_none() || PROVIDER_SUPPRESSED.load(Ordering::Relaxed) {
        return;
    }
    if let Some(webview) = active_webview(window) {
        if let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) {
            let logical = size.to_logical::<f64>(scale);
            let w = (logical.width - *provider_dock_px().lock().unwrap()).max(0.0);
            let top = provider_top();
            let h = (logical.height - top).max(0.0);

            // Skip if unchanged: avoids useless WebView2 IPC (without delaying the
            // tracking during a real resize, where the measurements change).
            {
                let mut last = last_provider_bounds().lock().unwrap();
                if *last == Some((w, h)) {
                    return;
                }
                *last = Some((w, h));
            }

            let _ = webview.set_position(LogicalPosition::new(0.0, top));
            let _ = webview.set_size(LogicalSize::new(w, h));
        }
    }
}

/// "Parks" the provider out of view (returns to the builder) on the Rust side.
/// Uses `set_position` (NON-blocking) instead of `hide()`/`close()`, which on
/// Windows freeze once the provider's page has navigated → the ✕/← Builder used
/// to get stuck. The webview stays alive (fast resume), just moved outside the
/// window; `resize_provider` does not bring it back up thanks to the `None` active key.
/// All tabs stay ALIVE (only the front one is parked), so switching back is instant.
pub fn park_provider<R: Runtime, M: Manager<R>>(manager: &M) {
    *pending_show_key().lock().unwrap() = None; // cancel any pending show
    park_active(manager); // park the front tab out of view (kept alive)
    *active_provider().lock().unwrap() = None;
    hide_menu_window(manager); // returning to the builder also dismisses the floating ⇄ menu
}

