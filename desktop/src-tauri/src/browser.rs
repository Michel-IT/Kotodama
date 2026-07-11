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

/// The provider is "active" (on screen) or "parked" out of view. Used by
/// `resize_provider` to NOT bring it back on screen on a resize when it is parked.
static PROVIDER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The provider is loading a new page while parked out of view: it must be
/// brought back on screen ONLY once loading is finished (on_page_load) — or via
/// the fallback. Avoids the "flash" of the previous provider's page during the switch.
static PROVIDER_PENDING_SHOW: AtomicBool = AtomicBool::new(false);

/// The provider is temporarily "suppressed" (parked out of view) because an app
/// modal (e.g. the Download manager) is showing on top. Unlike close/park, this
/// keeps PROVIDER_ACTIVE=true so we can restore the exact bounds afterwards;
/// `resize_provider` also bails out while suppressed so a resize does not pop it back.
static PROVIDER_SUPPRESSED: AtomicBool = AtomicBool::new(false);

/// Park Y: we move the provider webview well outside the window. It is a
/// `set_position` (NON-blocking, unlike `close()`/`hide()` on Windows once the
/// page has navigated), so the ✕ never freezes.
const PARK_Y: f64 = 100_000.0;

/// (Logical) height of the in-app browser bar in the main webview.
pub const TOPBAR_H: f64 = 46.0;
/// Label of the provider's child webview.
pub const PROVIDER_LABEL: &str = "provider";

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
fn provider_bounds(window: &Window) -> Result<(f64, f64), String> {
    let size = window.inner_size().map_err(|e| e.to_string())?;
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let logical = size.to_logical::<f64>(scale);
    let w = (logical.width - *provider_dock_px().lock().unwrap()).max(0.0);
    Ok((w, (logical.height - provider_top()).max(0.0)))
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

/// Opens (or re-navigates) the provider view to the given `url`.
///
/// Async: on Windows, creating a webview in a synchronous command causes a
/// deadlock (WebView2). The async command runs off the UI thread.
#[tauri::command]
pub async fn open_provider_view(window: Window, url: String) -> Result<(), String> {
    debug::log(format!("open_provider_view url={url}"));
    #[cfg(windows)]
    debug::log(format!(
        "sys_locale={:?} provider_args={}",
        sys_locale::get_locale(),
        provider_browser_args()
    ));
    debug::log(format!(
        "provider webview already exists = {}",
        window.get_webview(PROVIDER_LABEL).is_some()
    ));
    let parsed = url.parse::<tauri::Url>().map_err(|e| e.to_string())?;
    let (w, h) = provider_bounds(&window)?;

    // Anti-flash strategy: we load the new page with the webview PARKED out of
    // view, so the previous provider's page is never visible; we bring it back
    // on screen only once loading is finished (on_page_load) or via the fallback.
    PROVIDER_PENDING_SHOW.store(true, Ordering::Relaxed);

    if let Some(webview) = window.get_webview(PROVIDER_LABEL) {
        // Already created: park (hide the old page) and re-navigate.
        let _ = webview.set_position(LogicalPosition::new(0.0, PARK_Y));
        let _ = webview.set_size(LogicalSize::new(w, h));
        webview.navigate(parsed).map_err(|e| e.to_string())?;
        // no show() here: show_provider() does it once the page is ready.
    } else {
        let win_for_nav = window.clone();
        let builder = WebviewBuilder::new(PROVIDER_LABEL, WebviewUrl::External(parsed))
            .user_agent(PROVIDER_UA)
            .initialization_script(NO_MENU_JS)
            .on_navigation(move |u| {
                debug::log(format!("on_navigation -> {u}"));
                // Right-click sentinel: pop OUR native EDITING menu (Cut/Copy/Paste/Select all)
                // over this child webview, on the main thread (popup_menu requires it). Provider
                // switching now lives in the header ⇄ HTML menu, so it is NOT here. Nav is blocked.
                if u.host_str() == Some("kotodama.menu") {
                    let w = win_for_nav.clone();
                    let _ = win_for_nav.app_handle().run_on_main_thread(move || show_context_menu(&w));
                    return false;
                }
                // External links funneled here (sentinel host) open in the system
                // default browser; the in-app navigation is then blocked.
                if u.host_str() == Some("kotodama.external") {
                    if let Some((_, ext)) = u.query_pairs().find(|(k, _)| k == "u") {
                        let _ = win_for_nav.app_handle().opener().open_url(ext.into_owned(), None::<&str>);
                    }
                    return false;
                }
                // Keep the in-app URL bar in sync.
                let _ = win_for_nav.emit("app://provider-url", u.to_string());
                true
            })
            .on_page_load(|webview, payload| {
                debug::log(format!(
                    "on_page_load {:?} url={}",
                    payload.event(),
                    payload.url()
                ));
                // Once loading is finished, if we were waiting, bring the provider on screen.
                if payload.event() == PageLoadEvent::Finished
                    && PROVIDER_PENDING_SHOW.swap(false, Ordering::Relaxed)
                {
                    show_provider(&webview.window());
                }
            })
            // Abilita i DOWNLOAD nella webview del provider: senza un handler, wry/WebView2 li scarta
            // (es. il .docx generato da ChatGPT). Salviamo nella cartella Download e notifichiamo il frontend.
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
                            // Sottocartella dedicata: Download/Kotodama (ordinata + punto unico
                            // per condividere i file tra i vari provider). Creata se manca.
                            let kdir = dir.join("Kotodama");
                            let _ = std::fs::create_dir_all(&kdir);
                            *destination = kdir.join(name);
                        }
                        debug::log(format!("on_download DEST={}", destination.display()));
                        // Avvisa il frontend all'INIZIO: apre la modale "Gestore download" e mostra
                        // una riga "in corso" (poi finalizzata su Finished). Il download prosegue
                        // anche con la webview provider parcheggiata fuori vista dalla modale.
                        let name = destination
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "download".to_string());
                        let _ = webview.app_handle().emit(
                            "app://provider-download-start",
                            serde_json::json!({ "name": name, "url": url.to_string() }),
                        );
                        true // consenti il download
                    }
                    DownloadEvent::Finished { url, path, success } => {
                        let p = path.map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                        debug::log(format!("on_download FINISHED ok={success} url={url} path={p}"));
                        if success {
                            // Notifica NATIVA (si vede anche sopra la webview provider, a differenza del toast HTML).
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
        // NOTE: WebView2 browser arguments (incl. --disable-quic and the dynamic
        // --accept-lang) are set ONCE process-wide in lib.rs::run() via the
        // WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS env var, so EVERY webview (main,
        // toast, provider) shares identical arguments. Setting them per-webview
        // here would diverge from the main webview's environment and make this
        // child webview fail to initialize (blank page, no navigation).
        // Created PARKED (PARK_Y): the first page loads out of view.
        window
            .add_child(
                builder,
                LogicalPosition::new(0.0, PARK_Y),
                LogicalSize::new(w, h),
            )
            .map_err(|e| e.to_string())?;
    }

    // The in-app bar/overlay appear immediately (instant feedback); PROVIDER_ACTIVE
    // is set by show_provider() only when the page is ready.
    window
        .emit("app://provider-opened", &url)
        .map_err(|e| e.to_string())?;

    // Fallback: if on_page_load does not fire (cached page/redirect, etc.), show
    // the provider anyway after a while, so we don't stay stuck on the overlay.
    let app = window.app_handle().clone();
    let win_fallback = window.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(3500));
        if PROVIDER_PENDING_SHOW.swap(false, Ordering::Relaxed) {
            debug::log("show_provider via FALLBACK (on_page_load Finished never fired)");
            let _ = app.run_on_main_thread(move || show_provider(&win_fallback));
        }
    });
    Ok(())
}

/// Brings the provider back on screen (page ready): positions it below the
/// topbar, sizes it and shows it; marks ACTIVE and notifies the frontend
/// (removes the loading overlay). Idempotent: calling it multiple times is harmless.
fn show_provider(window: &Window) {
    debug::log("show_provider");
    if let Some(webview) = window.get_webview(PROVIDER_LABEL) {
        if let Ok((w, h)) = provider_bounds(window) {
            let _ = webview.set_position(LogicalPosition::new(0.0, provider_top()));
            let _ = webview.set_size(LogicalSize::new(w, h));
        }
        let _ = webview.show();
    }
    PROVIDER_ACTIVE.store(true, Ordering::Relaxed);
    *last_provider_bounds().lock().unwrap() = None; // force the next reposition
    let _ = window.emit("app://provider-loaded", ());
}

/// Returns to the builder by "parking" the provider out of view. (← Builder / Esc)
/// NB: we use `set_position` (non-blocking) and NOT `hide()`/`close()`, which on
/// Windows freeze once the page has navigated.
#[tauri::command]
pub fn close_provider_view(window: Window) -> Result<(), String> {
    PROVIDER_ACTIVE.store(false, Ordering::Relaxed);
    PROVIDER_PENDING_SHOW.store(false, Ordering::Relaxed); // cancel any pending show
    if let Some(webview) = window.get_webview(PROVIDER_LABEL) {
        let _ = webview.set_position(LogicalPosition::new(0.0, PARK_Y));
    }
    hide_menu_window(&window); // closing the provider also dismisses the floating ⇄ menu
    window
        .emit("app://provider-closed", ())
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Reloads the embedded page.
#[tauri::command]
pub fn provider_reload(window: Window) -> Result<(), String> {
    if let Some(webview) = window.get_webview(PROVIDER_LABEL) {
        webview
            .eval("location.reload()")
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Navigates the embedded page back in its history (← button in the in-app bar).
#[tauri::command]
pub fn provider_back(window: Window) -> Result<(), String> {
    if let Some(webview) = window.get_webview(PROVIDER_LABEL) {
        webview
            .eval("history.back()")
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Auto-fill: inserts the prompt into the provider page's input.
/// Polls for ~20s for an input (textarea / contenteditable / role=textbox) and
/// fills it ONLY if empty (so it doesn't disturb providers prefilled via ?q=).
/// Uses the native setter for textareas and `execCommand('insertText')` for
/// rich editors (ProseMirror/Quill), the most compatible. After filling it sends
/// the message by simulating Enter — used for providers without ?q=.
fn fill_impl(window: &Window, text: String, send: bool) -> Result<(), String> {
    if let Some(webview) = window.get_webview(PROVIDER_LABEL) {
        let json = serde_json::to_string(&text).map_err(|e| e.to_string())?;
        let js = format!("var __apb_text = {json}; var __apb_send = {send};")
            + r#"
(function(){
  var text = __apb_text;
  function getVal(el){ return (el.value !== undefined ? el.value : el.innerText) || ''; }
  function findSendBtn(){
    var sels = [
      'button[data-testid="send-button"]',
      'button[data-testid*="send" i]',
      'button[aria-label*="send" i]',
      'button[aria-label*="invia" i]',
      'button[aria-label*="invio" i]',
      'button[type="submit"]:not([disabled])'
    ];
    for (var i=0;i<sels.length;i++){
      var b = document.querySelector(sels[i]);
      if (b && !b.disabled && b.offsetParent !== null) return b;
    }
    return null;
  }
  function submit(el){
    // Ritenta finché il testo è ancora nel campo (si ferma appena inviato → niente
    // doppio invio). Serve per la 1ª apertura "a freddo" (es. ChatGPT) dove il
    // composer/idratazione non è pronto al primo colpo.
    var attempts = 0;
    function attempt(){
      if (getVal(el).trim().length === 0) return;            // inviato: stop
      try { el.focus(); } catch (e) {}                       // focus prima dell'Invio (Claude)
      // 1) Enter (invia su ChatGPT/Gemini/DeepSeek/Z.ai)
      ['keydown','keypress','keyup'].forEach(function(t){
        el.dispatchEvent(new KeyboardEvent(t, {key:'Enter', code:'Enter', keyCode:13, which:13, bubbles:true, cancelable:true}));
      });
      // 2) poco dopo, se c'è ancora testo, clicca il pulsante d'invio (es. Qwen)
      setTimeout(function(){
        if (getVal(el).trim().length === 0) return;
        var b = findSendBtn();
        if (b) b.click();
      }, 250);
      attempts++;
      if (attempts < 10) setTimeout(attempt, 900);
    }
    attempt();
  }
  var tries = 0;
  var iv = setInterval(function(){
    tries++;
    var el = document.querySelector('textarea:not([readonly]):not([aria-hidden="true"])')
          || document.querySelector('[contenteditable="true"]')
          || document.querySelector('div[role="textbox"]');
    if (el) {
      el.focus();   // SEMPRE: Claude/ProseMirror ignora l'Invio se l'editor non e' a fuoco
      var isText = (el.tagName === 'TEXTAREA' || el.value !== undefined);
      if (getVal(el).trim().length === 0) {
        // vuoto → riempi (provider clipboard, o ?q= che non ha precompilato)
        if (isText) {
          try {
            var set = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
            set.call(el, text);
          } catch (e) { el.value = text; }
          el.dispatchEvent(new Event('input', { bubbles: true }));
        } else {
          try { document.execCommand('selectAll', false, null); document.execCommand('insertText', false, text); }
          catch (e) { el.textContent = text; el.dispatchEvent(new InputEvent('input', { bubbles: true })); }
        }
      } else if (!isText) {
        // gia' precompilato (?q=) in un editor rich (Claude): porta il caret in fondo
        // e notifica un input, cosi' React abilita l'invio e accetta l'Enter.
        try { var rng = document.createRange(); rng.selectNodeContents(el); rng.collapse(false); var sel = window.getSelection(); sel.removeAllRanges(); sel.addRange(rng); } catch (e) {}
        el.dispatchEvent(new InputEvent('input', { bubbles: true }));
      }
      // invia se l'input ha testo (riempito ora o precompilato da ?q=)
      setTimeout(function(){ if (__apb_send && getVal(el).trim().length) submit(el); }, 500);
      clearInterval(iv);
    }
    if (tries > 66) clearInterval(iv);
  }, 300);
})();
"#;
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
    if PROVIDER_ACTIVE.load(Ordering::Relaxed) {
        if let Some(webview) = window.get_webview(PROVIDER_LABEL) {
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
    if let Some(webview) = window.get_webview(PROVIDER_LABEL) {
        if on {
            let _ = webview.set_position(LogicalPosition::new(0.0, PARK_Y));
        } else if PROVIDER_ACTIVE.load(Ordering::Relaxed) {
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
    if PROVIDER_ACTIVE.load(Ordering::Relaxed) {
        if let Some(webview) = window.get_webview(PROVIDER_LABEL) {
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
    if !PROVIDER_ACTIVE.load(Ordering::Relaxed) || PROVIDER_SUPPRESSED.load(Ordering::Relaxed) {
        return;
    }
    if let Some(webview) = window.get_webview(PROVIDER_LABEL) {
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
/// window; `resize_provider` does not bring it back up thanks to `PROVIDER_ACTIVE`.
pub fn park_provider<R: Runtime, M: Manager<R>>(manager: &M) {
    PROVIDER_ACTIVE.store(false, Ordering::Relaxed);
    PROVIDER_PENDING_SHOW.store(false, Ordering::Relaxed); // cancel any pending show
    if let Some(webview) = manager.get_webview(PROVIDER_LABEL) {
        let _ = webview.set_position(LogicalPosition::new(0.0, PARK_Y));
    }
    hide_menu_window(manager); // returning to the builder also dismisses the floating ⇄ menu
}

