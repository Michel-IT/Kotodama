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
use tauri::{Emitter, LogicalPosition, LogicalSize, Manager, Runtime, WebviewUrl, WebviewWindow, Window};
use tauri_plugin_opener::OpenerExt;   // open external links in the system browser

/// The provider is "active" (on screen) or "parked" out of view. Used by
/// `resize_provider` to NOT bring it back on screen on a resize when it is parked.
static PROVIDER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The provider is loading a new page while parked out of view: it must be
/// brought back on screen ONLY once loading is finished (on_page_load) — or via
/// the fallback. Avoids the "flash" of the previous provider's page during the switch.
static PROVIDER_PENDING_SHOW: AtomicBool = AtomicBool::new(false);

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
pub const PROVIDER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Script injected into EVERY provider page (external sites):
/// - disables the right-click menu and devtools shortcuts (no browser/Tauri hints);
/// - sends links meant for a new tab (target="_blank") to the system default
///   browser, via a sentinel URL that the Rust `on_navigation` handler intercepts.
pub const NO_MENU_JS: &str = r#"
(function(){
  document.addEventListener('contextmenu', function(e){ e.preventDefault(); }, {capture:true});
  document.addEventListener('keydown', function(e){
    var k=(e.key||'').toUpperCase();
    if(k==='F12'||((e.ctrlKey||e.metaKey)&&e.shiftKey&&(k==='I'||k==='J'||k==='C'))||((e.ctrlKey||e.metaKey)&&k==='U')){ e.preventDefault(); e.stopPropagation(); }
  }, {capture:true});
  // External links (open-in-new-tab) -> system browser. Same-tab navigations
  // (SPA, login redirects) are left untouched so provider login keeps working.
  document.addEventListener('click', function(e){
    var a = e.target && e.target.closest && e.target.closest('a[href]');
    if(!a) return;
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

/// Logical size available below the top bar (+ banner).
fn provider_bounds(window: &Window) -> Result<(f64, f64), String> {
    let size = window.inner_size().map_err(|e| e.to_string())?;
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let logical = size.to_logical::<f64>(scale);
    Ok((logical.width, (logical.height - provider_top()).max(0.0)))
}

/// Opens (or re-navigates) the provider view to the given `url`.
///
/// Async: on Windows, creating a webview in a synchronous command causes a
/// deadlock (WebView2). The async command runs off the UI thread.
#[tauri::command]
pub async fn open_provider_view(window: Window, url: String) -> Result<(), String> {
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
        let mut builder = WebviewBuilder::new(PROVIDER_LABEL, WebviewUrl::External(parsed))
            .user_agent(PROVIDER_UA)
            .initialization_script(NO_MENU_JS)
            .on_navigation(move |u| {
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
                // Once loading is finished, if we were waiting, bring the provider on screen.
                if payload.event() == PageLoadEvent::Finished
                    && PROVIDER_PENDING_SHOW.swap(false, Ordering::Relaxed)
                {
                    show_provider(&webview.window());
                }
            });
        // On Windows the child-webview must have the same flags as the main
        // (in particular --disable-quic): otherwise some providers stay blank
        // due to ERR_QUIC_PROTOCOL_ERROR (fixing only the main is not enough).
        #[cfg(windows)]
        {
            let args = provider_browser_args();
            builder = builder.additional_browser_args(&args);
        }
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
            let _ = app.run_on_main_thread(move || show_provider(&win_fallback));
        }
    });
    Ok(())
}

/// Brings the provider back on screen (page ready): positions it below the
/// topbar, sizes it and shows it; marks ACTIVE and notifies the frontend
/// (removes the loading overlay). Idempotent: calling it multiple times is harmless.
fn show_provider(window: &Window) {
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

/// Auto-fill: inserts the prompt into the provider page's input.
/// Polls for ~20s for an input (textarea / contenteditable / role=textbox) and
/// fills it ONLY if empty (so it doesn't disturb providers prefilled via ?q=).
/// Uses the native setter for textareas and `execCommand('insertText')` for
/// rich editors (ProseMirror/Quill), the most compatible. After filling it sends
/// the message by simulating Enter — used for providers without ?q=.
#[tauri::command]
pub fn provider_fill(window: Window, text: String) -> Result<(), String> {
    if let Some(webview) = window.get_webview(PROVIDER_LABEL) {
        let json = serde_json::to_string(&text).map_err(|e| e.to_string())?;
        let js = format!("var __apb_text = {json};")
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
      if (attempts < 6) setTimeout(attempt, 900);
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
      // riempi SOLO se vuoto (i provider ?q= sono già precompilati)
      if (getVal(el).trim().length === 0) {
        el.focus();
        if (el.tagName === 'TEXTAREA' || el.value !== undefined) {
          try {
            var set = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
            set.call(el, text);
          } catch (e) { el.value = text; }
          el.dispatchEvent(new Event('input', { bubbles: true }));
        } else {
          try { document.execCommand('selectAll', false, null); document.execCommand('insertText', false, text); }
          catch (e) { el.textContent = text; el.dispatchEvent(new InputEvent('input', { bubbles: true })); }
        }
      }
      // invia se l'input ha testo (riempito ora o precompilato da ?q=)
      setTimeout(function(){ if (getVal(el).trim().length) submit(el); }, 400);
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

/// Last bounds applied to the provider (to skip redundant set_size/set_position:
/// Windows emits `Resized` even duplicated / with unchanged measurements).
fn last_provider_bounds() -> &'static Mutex<Option<(f64, f64)>> {
    static B: OnceLock<Mutex<Option<(f64, f64)>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(None))
}

/// Keeps the child webview's bounds aligned with the window resize.
pub fn resize_provider(window: &WebviewWindow) {
    // If the provider is parked (builder on screen), do NOT bring it back up on a
    // resize: it would stay on top of the builder.
    if !PROVIDER_ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    if let Some(webview) = window.get_webview(PROVIDER_LABEL) {
        if let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) {
            let logical = size.to_logical::<f64>(scale);
            let w = logical.width;
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
}

