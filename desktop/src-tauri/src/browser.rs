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
    Emitter, LogicalPosition, LogicalSize, Manager, Runtime, WebviewUrl, WebviewWindow, Window,
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
/// `accept-lang` is pinned to English for EVERY provider (was: followed the OS language) --
/// this is shared across the whole WebView2 environment, so there is no way to set it only for
/// one provider. Needed so Kotodama's login-wall detection can match stable English button text
/// ("Log in"/"Sign up") -- Grok specifically has NO structural (data-testid/id/stable-class)
/// marker on its login controls, and its DOM around them isn't even deterministic between page
/// loads, so text matching pinned to a known language is the only reliable signal left. The
/// trade-off: every provider's UI now renders in English regardless of the user's own OS/app
/// language, not just Grok's.
#[cfg(windows)]
pub fn provider_browser_args() -> String {
    // Provider webviews are parked OFF-SCREEN almost all the time (only shown when their tab is
    // active), so Chromium's Native Window Occlusion + background-tab timer throttling see them as
    // "hidden" essentially forever and progressively slow their JS timers -> the Kotodama gateway's
    // harvest polling (setInterval) can stall for minutes after the app has been idle for a while,
    // so an inline transform / broadcast triggered post-standby never delivers an answer even though
    // the page is visibly working (fill+send ran, harvesting never fires). Same root cause as the
    // macOS WKWebView occlusion fix (`mac_disable_occlusion`); this is the WebView2/Chromium side.
    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion --disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-background-timer-throttling --disable-quic --accept-lang=en-US,en".to_string()
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

/// Closes a gap left by `PROVIDER_UA`: that only rewrites the classic `navigator.userAgent`
/// string/header, but on Chromium/WebView2 the newer Client Hints API (`navigator.userAgentData`
/// + the `Sec-CH-UA*` request headers) still exposes the REAL engine identity (Edge/WebView2
/// brand tokens), even with the classic UA spoofed to plain Chrome -- a site cross-checking both
/// could catch the mismatch. Self-gating: on WKWebView (macOS) `navigator.userAgentData` doesn't
/// exist at all (real Safari has no Client Hints), so the early return leaves it correctly
/// absent there -- no `#[cfg(target_os...)]` split needed, unlike `PROVIDER_UA`.
/// NOTE: brand/version here (Chrome 126) must stay in sync with `PROVIDER_UA`'s `Chrome/126...`.
pub const CLIENT_HINTS_JS: &str = r#"
(function(){
  try {
    if (!navigator.userAgentData) return;
    var brands = [
      { brand: 'Not/A)Brand', version: '8' },
      { brand: 'Chromium', version: '126' },
      { brand: 'Google Chrome', version: '126' }
    ];
    var fullVersionList = [
      { brand: 'Not/A)Brand', version: '8.0.0.0' },
      { brand: 'Chromium', version: '126.0.0.0' },
      { brand: 'Google Chrome', version: '126.0.0.0' }
    ];
    var fake = {
      brands: brands, mobile: false, platform: 'Windows',
      toJSON: function(){ return { brands: brands, mobile: false, platform: 'Windows' }; },
      getHighEntropyValues: function(hints){
        var full = { architecture: 'x86', bitness: '64', model: '', platformVersion: '19.0.0',
          uaFullVersion: '126.0.0.0', fullVersionList: fullVersionList };
        var out = { brands: brands, mobile: false, platform: 'Windows' };
        (hints || []).forEach(function(h){ if (h in full) out[h] = full[h]; });
        return Promise.resolve(out);
      }
    };
    Object.defineProperty(navigator, 'userAgentData', { get: function(){ return fake; }, configurable: true });
  } catch(e){}
})();
"#;

/// Forces `navigator.language`/`navigator.languages` to English on grok.com ONLY (self-gated by
/// hostname, like the other injected scripts in this file are self-gated by feature presence --
/// no per-provider branching needed in `create_tab`). Grok's own login/sign-up UI text has no
/// stable structural marker to detect by (confirmed live: no reliable data-testid/id, and the
/// generic Tailwind wrapper classes around those buttons are NOT deterministic between page loads
/// -- 3 separate captures showed 3 different structures), so detecting the login wall by TEXT is
/// the only option left; pinning that text to English keeps the check language-independent from
/// KOTODAMA's side even though it now relies on Grok always answering in English. Client-side
/// only (many modern SPAs, Grok included, pick UI language from `navigator.language` rather than
/// solely the `Accept-Language` HTTP header, which stays user-locale via `provider_browser_args`
/// -- that header is set once for the whole WebView2 environment, not overridable per-webview).
///
/// ALSO overwrites the `i18nextLng` cookie/localStorage key: live diagnostics (AUTHWALL-CENSUS)
/// showed `navigator.language` was ALREADY correctly "en-US" (Accept-Language pinning worked),
/// yet Grok still rendered Italian -- because Grok's i18next language-detector reads a PERSISTED
/// `i18nextLng=it` cookie (saved on an earlier visit, before Accept-Language was pinned) with
/// higher priority than navigator.language, and the WebView2 profile survives across app updates
/// so that stale cookie never expires on its own. Runs at document-start (before Grok's own i18n
/// init reads it), so the overwrite wins the race.
pub const FORCE_EN_LANG_JS: &str = r#"
(function(){
  try {
    if (location.hostname.indexOf('grok.com') === -1) return;
    Object.defineProperty(navigator, 'language', { get: function(){ return 'en-US'; }, configurable: true });
    Object.defineProperty(navigator, 'languages', { get: function(){ return ['en-US', 'en']; }, configurable: true });
    document.cookie = 'i18nextLng=en; path=/; max-age=31536000';
    try { localStorage.setItem('i18nextLng', 'en'); } catch(e){}
  } catch(e){}
})();
"#;

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

/// Injected on every page-load finish (provider tabs) and by the main-window cold-start watchdog
/// (`lib.rs::run`'s `.setup()`): if the load actually failed, WebView2/Chromium replaces the
/// document with its own internal error page at `chrome-error://chromewebdata/` (detectable by
/// protocol, language-independent -- never the localized error TEXT, which would break in other
/// UI languages). Swaps it for a branded page with Riavvia/Segnala/Chiudi.
///
/// Button wiring is intentionally two-layered, because a `chrome-error://` document is NOT the
/// app's own origin: Tauri's capability ACL only grants `default.json`'s commands (open_url,
/// quit_app, restart_app) to the app's real local origin, so a direct `invoke()` from THIS page
/// may be silently denied. Every click therefore ALSO stamps `document.title` with a sentinel --
/// reading/writing a window title is a bare OS-level API with no ACL involved at all, so it is
/// the one channel guaranteed to reach Rust regardless of origin. The MAIN window's watchdog
/// polls for these sentinels and acts on them directly (see `lib.rs`). Provider webviews have no
/// such poller, so their "Segnala" button ALSO tries `invoke()` first, falling back to the
/// existing `kotodama.external` navigation sentinel (`NO_MENU_JS` uses the same trick for links,
/// and navigation interception happens before any ACL check, so it works regardless of origin).
const OOPS_PAGE_JS_TEMPLATE: &str = r#"
(function(){
  if (location.protocol !== 'chrome-error:') return;
  if (document.getElementById('__ktOops')) return;   // already shown
  var css = 'html,body{height:100%}body{margin:0;background:#14171f;color:#eaeefb;'
    + 'font-family:-apple-system,Segoe UI,sans-serif;display:flex;flex-direction:column;'
    + 'align-items:center;justify-content:center;text-align:center;gap:14px}'
    + 'h1{font-size:19px;margin:0}p{color:#9aa4c4;font-size:13px;max-width:340px;margin:0}'
    + '.btns{display:flex;gap:10px;margin-top:6px}'
    + 'button{font:inherit;font-size:13px;font-weight:600;padding:8px 18px;border-radius:8px;'
    + 'border:1px solid #2c3554;background:#1c2440;color:#eaeefb;cursor:pointer}'
    + 'button.pri{background:#f4a52a;color:#1a1a1a;border-color:#f4a52a}';
  var div = document.createElement('div');
  div.id = '__ktOops';
  div.innerHTML = '<style>' + css + '</style>'
    + '<h1>__OOPS_TITLE__</h1>'
    + '<p>__OOPS_MESSAGE__</p>'
    + '<div class="btns"><button id="__ktOopsRestart">__OOPS_RESTART__</button>'
    + '<button id="__ktOopsIssue" class="pri">__OOPS_ISSUE__</button>'
    + '<button id="__ktOopsClose">__OOPS_CLOSE__</button></div>';
  document.body.innerHTML = '';
  document.body.appendChild(div);
  // Dual mechanism: title-sentinel (the main window's watchdog polls for it, no ACL involved) AND
  // a direct invoke() try (works if this window's ACL turns out to accept it after all -- not
  // confirmed either way, so both fire rather than betting on one). The invoke()-failure fallback
  // (navigate to the kotodama.external sentinel) is ONLY safe on provider tabs, whose on_navigation
  // handler intercepts it (see NO_MENU_JS) -- the MAIN window has no such handler, so that same
  // navigation would actually try to resolve a fake domain for real and show ANOTHER, worse error
  // page (confirmed: this happened). __OOPS_IS_MAIN__ is substituted by oops_page_js() below.
  var IS_MAIN = __OOPS_IS_MAIN__;
  function signal(kind, cmd, args){
    document.title = '__kt_oops:' + kind;
    try {
      if (window.__TAURI__ && window.__TAURI__.core && typeof window.__TAURI__.core.invoke === 'function') {
        window.__TAURI__.core.invoke(cmd, args).catch(function(){
          if (kind === 'issue' && !IS_MAIN) {
            var url = 'https://github.com/Michel-IT/Kotodama/issues/new';
            location.href = 'https://kotodama.external/open?u=' + encodeURIComponent(url);
          }
        });
      }
    } catch(e){}
  }
  document.getElementById('__ktOopsRestart').addEventListener('click', function(){ signal('restart', 'restart_app'); });
  document.getElementById('__ktOopsClose').addEventListener('click', function(){ signal('close', 'quit_app'); });
  document.getElementById('__ktOopsIssue').addEventListener('click', function(){
    signal('issue', 'open_url', { url: 'https://github.com/Michel-IT/Kotodama/issues/new' });
  });
})();
"#;

/// Builds `OOPS_PAGE_JS_TEMPLATE` with the strings for `app`'s current UI language (falls back to
/// English for anything missing). Reads `desktop/ui/i18n/<lang>.json` via Tauri's own asset
/// resolver -- the exact same bundled bytes the frontend itself loads, so this works identically
/// in dev and in a packaged release build (a plain `std::fs::read` would not: frontendDist assets
/// are embedded into the binary, not left as loose files on disk).
pub fn oops_page_js(app: &tauri::AppHandle, is_main: bool) -> String {
    let lang = {
        let state = app.state::<crate::AppState>();
        let g = state.settings.lock().unwrap();
        g.language.clone()
    };
    let load = |code: &str| -> Option<serde_json::Value> {
        let asset = app.asset_resolver().get(format!("i18n/{code}.json"))?;
        serde_json::from_slice(&asset.bytes).ok()
    };
    let primary = if lang.is_empty() { None } else { load(&lang) };
    let en = load("en").unwrap_or(serde_json::Value::Null);
    let pick = |key: &str| -> String {
        primary
            .as_ref()
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_str())
            .or_else(|| en.get(key).and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string()
    };
    // JSON-string-escape each value for safe embedding as a JS string literal (handles quotes/
    // backslashes/unicode correctly), trimming the surrounding quotes json produces.
    let esc = |s: &str| -> String {
        let quoted = serde_json::to_string(s).unwrap_or_default();
        quoted[1..quoted.len().saturating_sub(1)].to_string()
    };
    let message = pick("oopsMessage").replace('\n', "<br>");
    OOPS_PAGE_JS_TEMPLATE
        .replace("__OOPS_IS_MAIN__", if is_main { "true" } else { "false" })
        .replace("__OOPS_TITLE__", &esc(&pick("oopsTitle")))
        .replace("__OOPS_MESSAGE__", &esc(&message))
        .replace("__OOPS_RESTART__", &esc(&pick("oopsRestart")))
        .replace("__OOPS_ISSUE__", &esc(&pick("linkIssues")))
        .replace("__OOPS_CLOSE__", &esc(&pick("winClose")))
}

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
/// SINGLE point of truth for "a provider's real page is now on screen": emits both
/// `app://provider-opened` (topbar chrome: back/reload/switch controls) and
/// `app://provider-loaded` (clears the loading overlay) -- every caller inherits both.
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
    let _ = window.emit("app://provider-opened", ());
    let _ = window.emit("app://provider-loaded", ());
}

/// Localized labels for the native provider EDITING context menu, pushed by the frontend in the
/// active app language (field names match the i18n keys). Stored in AppState; read when the menu
/// is built. Defaults are empty -> `show_context_menu` falls back to English until set.
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
    #[serde(default, rename = "copyUrl")]
    pub copy_url: String,
}

/// Frontend pushes the menu labels (all app languages) at startup and on every language change,
/// mirroring `set_tray_labels`. Stored for `show_context_menu` to use.
#[tauri::command]
pub fn set_provider_menu_labels(window: Window, labels: MenuLabels) {
    if let Some(state) = window.try_state::<crate::AppState>() {
        *state.menu_labels.lock().unwrap() = labels;
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

/// Builds + adds (PARKED off-screen) the persistent child webview for provider `key`, navigated to
/// `url`. Its `on_page_load` brings it on screen once loading finishes IF it is still the pending
/// tab (anti-flash). Shared by `open_provider_view` (recipe) and `show_provider_tab` (first switch).
pub(crate) fn create_tab(window: &Window, key: &str, url: tauri::Url, w: f64, h: f64) -> Result<(), String> {
    let win_for_nav = window.clone();
    let key_load = key.to_string();
    let builder = WebviewBuilder::new(provider_label(key), WebviewUrl::External(url))
        .user_agent(PROVIDER_UA)
        .initialization_script(NO_MENU_JS)
        .initialization_script(CLIENT_HINTS_JS)
        .initialization_script(FORCE_EN_LANG_JS)
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
                // ALWAYS, regardless of any pending fill/broadcast: hide screen-reader-only labels
                // (e.g. ChatGPT's hidden "Modifica"/"Edit" text next to the pencil icon) that would
                // otherwise ride along a native Ctrl+A/Ctrl+C on this tab -- a plain directly-opened
                // provider tab never runs the Kotodama harvest injection (which used to be the only
                // place this ran), so a manual copy on it could pick up that hidden label text.
                let _ = webview.eval(crate::kotodama::SR_HIDE_JS);
                let _ = webview.eval(&oops_page_js(&webview.app_handle(), false));
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
    #[cfg(windows)]
    win_probe_login_signals(&_child, key);
    Ok(())
}

/// DEBUG-only probe (Windows): checks whether the WebView2 CookieManager and
/// WebResourceResponseReceived APIs are usable from this exact Tauri/wry setup, as real signal
/// candidates for provider login-state detection (see docs/research/login-detection-providers.md)
/// -- more robust than DOM/text scraping because a session cookie or an actual HTTP 401/403 is
/// server-issued ground truth, not a proxy inferred from page markup. Logs cookie names (never
/// values, even in local debug logs) for this webview's whole cookie jar, and the URL+status of
/// the first responses it receives. No-op unless KOTODAMA_DEBUG is set.
#[cfg(windows)]
fn win_probe_login_signals<R: tauri::Runtime>(webview: &tauri::Webview<R>, key: &str) {
    if !debug::enabled() {
        return;
    }
    let key = key.to_string();
    let _ = webview.with_webview(move |pw| {
        use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_2;
        use webview2_com::{take_pwstr, GetCookiesCompletedHandler, WebResourceResponseReceivedEventHandler};
        use windows::core::Interface;

        let controller = pw.controller();
        let core = match unsafe { controller.CoreWebView2() } {
            Ok(c) => c,
            Err(e) => {
                debug::log(format!("win_probe[{key}]: CoreWebView2() failed: {e}"));
                return;
            }
        };
        let core2 = match core.cast::<ICoreWebView2_2>() {
            Ok(c) => c,
            Err(e) => {
                debug::log(format!("win_probe[{key}]: cast to ICoreWebView2_2 failed: {e}"));
                return;
            }
        };

        // Cookies: whole jar for this webview's profile (empty URI = no filter), names only.
        if let Ok(mgr) = unsafe { core2.CookieManager() } {
            let k = key.clone();
            let handler = GetCookiesCompletedHandler::create(Box::new(move |hr, list| {
                if let Err(e) = hr {
                    debug::log(format!("win_probe[{k}]: GetCookies failed: {e}"));
                    return Ok(());
                }
                let Some(list) = list else { return Ok(()) };
                let mut count = 0u32;
                let _ = unsafe { list.Count(&mut count) };
                let mut names = Vec::new();
                for i in 0..count.min(60) {
                    if let Ok(cookie) = unsafe { list.GetValueAtIndex(i) } {
                        let mut name_p = windows::core::PWSTR::null();
                        let mut domain_p = windows::core::PWSTR::null();
                        let _ = unsafe { cookie.Name(&mut name_p) };
                        let _ = unsafe { cookie.Domain(&mut domain_p) };
                        names.push(format!("{}@{}", take_pwstr(name_p), take_pwstr(domain_p)));
                    }
                }
                debug::log(format!("win_probe[{k}]: cookies count={count} names={names:?}"));
                Ok(())
            }));
            let _ = unsafe { mgr.GetCookies(windows::core::PCWSTR::null(), &handler) };
        } else {
            debug::log(format!("win_probe[{key}]: CookieManager() failed"));
        }

        // Network: log URL+status of the first responses this webview receives (capped, so a busy
        // provider tab doesn't flood the debug log). Handler is never removed -- fine for a
        // one-shot probe; a real implementation would remove_WebResourceResponseReceived once
        // it has what it needs.
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let k2 = key.clone();
        let handler2 = WebResourceResponseReceivedEventHandler::create(Box::new(move |_sender, args| {
            let Some(args) = args else { return Ok(()) };
            let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n >= 20 {
                return Ok(());
            }
            if let (Ok(req), Ok(resp)) = (unsafe { args.Request() }, unsafe { args.Response() }) {
                let mut uri_p = windows::core::PWSTR::null();
                let _ = unsafe { req.Uri(&mut uri_p) };
                let mut status = 0i32;
                let _ = unsafe { resp.StatusCode(&mut status) };
                debug::log(format!("win_probe[{k2}]: net {status} {}", take_pwstr(uri_p)));
            }
            Ok(())
        }));
        let mut token: i64 = 0;
        if let Err(e) = unsafe { core2.add_WebResourceResponseReceived(&handler2, &mut token) } {
            debug::log(format!("win_probe[{key}]: add_WebResourceResponseReceived failed: {e}"));
        }
    });
}

/// SWITCH to provider `key` (header icon strip / double-click tile / ⇄ "Open"): if its tab already
/// exists, bring it on screen AS-IS (keeps its page, NO reload) UNLESS `force_reload` -- used when
/// opening specifically to log in (e.g. from a "login required" card): the tab may already be
/// sitting on an unrelated/stale page (mid Kotodama harvest, or just its normal chat homepage,
/// which for some providers never surfaces the sign-in screen on its own), so it must be
/// navigated to `base_url` fresh instead of shown as-is. Async for the create path (WebView2
/// deadlock on Windows).
#[tauri::command]
pub async fn show_provider_tab(
    window: Window,
    key: String,
    base_url: String,
    force_reload: bool,
) -> Result<(), String> {
    debug::log(format!("show_provider_tab key={key} force_reload={force_reload}"));
    if let Some(wv) = window.get_webview(&provider_label(&key)) {
        park_active(&window);
        if force_reload {
            let json = serde_json::to_string(&base_url).map_err(|e| e.to_string())?;
            let _ = wv.eval(format!("location.href = {json};"));
        }
        show_tab(&window, &key);
        Ok(())
    } else {
        let parsed = base_url.parse::<tauri::Url>().map_err(|e| e.to_string())?;
        let (w, h) = provider_bounds(&window)?;
        park_active(&window);
        create_tab(&window, &key, parsed, w, h)?;
        // Brand-new tab: nothing to flash (no old page), so bring it on screen IMMEDIATELY and let
        // the user watch the provider's own page load, instead of staring at our spinner. Waiting
        // for PageLoadEvent::Finished here is what made a first open feel slow.
        show_tab(&window, &key);   // app://provider-opened fires from show_tab itself
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
/// `fill_js` builds the script, reused by the Kotodama broadcast gateway (`kotodama.rs`), the
/// only remaining caller now that the direct-fill commands are gone.
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
  // Among ALL visible candidates, prefer the BOTTOM-MOST one on screen (largest rect.bottom):
  // the real send composer is always the lowest input on the page, in EVERY provider and EVERY
  // language. This matters on long-lived REUSED tabs (Kotodama keeps a tab alive across turns):
  // if the user (or a stray click) leaves a per-message "Edit"/"Modifica" inline box open higher
  // up in the thread, a first-match pick would fill THAT box instead of the real composer,
  // dragging its neighboring UI (e.g. the localized Edit/Save button) into the sent message.
  // Geometry, not text, so this holds for every UI language without per-language matching.
  function pickComposer(){
    var sels = ['textarea:not([readonly]):not([aria-hidden="true"])', '[contenteditable="true"]', 'div[role="textbox"]'];
    var best = null, bestY = -1e9;
    for (var i=0;i<sels.length;i++){
      var els = document.querySelectorAll(sels[i]);
      for (var j=0;j<els.length;j++){
        var e = els[j]; if (e.offsetParent === null) continue;
        var b = e.getBoundingClientRect().bottom;
        if (b > bestY){ bestY = b; best = e; }
      }
    }
    return best;
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
}


