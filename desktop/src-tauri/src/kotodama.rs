//! Kotodama meta-provider — broadcast gateway.
//!
//! Sends one prompt to N provider child-webviews (kept PARKED off-screen, never shown),
//! then harvests each provider's answer from its DOM and delivers it back to the main
//! webview as `app://kotodama-answer` events.
//!
//! Channels (same trust model as browser.rs — remote pages have NO Tauri IPC):
//! - Rust -> provider page: `webview.eval` (fill + harvest script, fire-and-forget).
//! - provider page -> Rust: navigation sentinel `https://kotodama.result/?...` intercepted
//!   in `create_tab`'s `on_navigation` (returns false: the page never actually navigates).
//!   Long answers travel CHUNKED in the URL query (seq/total), reassembled here.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use tauri::{Emitter, Manager, Runtime, Url, Window};

use crate::browser;
use crate::debug;

/// One in-flight broadcast: which provider keys still owe an answer.
struct Broadcast {
    pending: HashSet<String>,
    #[allow(dead_code)]
    started: Instant,
}

/// Reassembly buffer for one (broadcast, provider) answer delivered in URL chunks.
struct ChunkBuf {
    parts: Vec<Option<String>>,
    status: String,
    trunc: bool,
    /// Markdown rendering of the answer (tables/code/bold/lists), extracted client-side from the
    /// provider's own rendered HTML -- only ever sent whole, over the direct-IPC path (see
    /// `deliver()` in HARVEST_JS), so this is just set once, not chunked/reassembled like `parts`.
    md: String,
}

/// Fill+harvest JS waiting for a provider page to finish loading (set before
/// create/navigate; consumed in `on_page_finished` or by the 8s fallback).
struct PendingInjection {
    broadcast_id: String,
    text: String,
    /// Fresh conversation (page just navigated): there is NO previous answer in the
    /// DOM, so the harvester must not snapshot one (a ?q= provider may auto-send and
    /// even finish answering before we inject).
    fresh: bool,
    /// User wants provider temporary chats (kt_temp_chats at broadcast time).
    temp: bool,
}

fn broadcasts() -> &'static Mutex<HashMap<String, Broadcast>> {
    static B: OnceLock<Mutex<HashMap<String, Broadcast>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}
fn chunk_bufs() -> &'static Mutex<HashMap<(String, String), ChunkBuf>> {
    static C: OnceLock<Mutex<HashMap<(String, String), ChunkBuf>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}
fn pending_injections() -> &'static Mutex<HashMap<String, PendingInjection>> {
    static P: OnceLock<Mutex<HashMap<String, PendingInjection>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Injections that already ran: provider key -> (broadcast_id, sent text). Needed because some
/// provider pages NAVIGATE right after send (Qwen landing -> chat route, ChatGPT /?q= -> /c/<id>),
/// killing the injected harvester with the old document: on the next page-load we re-inject a
/// HARVEST-ONLY script to resume collection. Cleared when the key's answer is delivered.
fn active_harvests() -> &'static Mutex<HashMap<String, (String, String)>> {
    static A: OnceLock<Mutex<HashMap<String, (String, String)>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Best-known per-provider DOM selectors: (key, answer container, "still generating" marker).
/// Empty string = rely on the generic fallback chain in the harvest JS. These WILL drift as
/// providers redesign; the JS treats them as the first candidate only, so a stale selector
/// degrades to the generic chain instead of breaking.
const HARVEST_SELECTORS: &[(&str, &str, &str)] = &[
    ("openai", r#"[data-message-author-role="assistant"]"#, r#"button[data-testid="stop-button"]"#),
    // `.font-claude-response` is the current class; `.font-claude-message` was the previous one and is
    // kept so an older build of their UI still matches. Captured live 2026-08-18 -- with neither
    // matching, the chain fell back to the whole message wrapper and harvested the screen-reader
    // label and the reasoning block along with the answer.
    ("anthropic", r#".font-claude-response, .font-claude-message"#, r#"div[data-is-streaming="true"]"#),
    ("gemini", r#"message-content, .model-response-text"#, ""),
    ("perplexity", r#"main .prose"#, r#"button[aria-label*="stop" i]"#),
    ("deepseek", r#".ds-markdown"#, ""),
    ("qwen", "", ""),
    // Was `[class*="message-bubble"]`, matching the USER's own bubble too (both share the
    // class) -- with no distinct answer element, the SENT-text safety net kept discarding it
    // as "that's my own message", stalling forever at 0 chars. `rounded-br-lg` decorates only
    // the sender's (user's) bubble corner, so excluding it isolates the assistant's reply.
    // Verified against a real captured DOM (not guessed); still no confirmed busy-marker.
    ("grok", r#".message-bubble:not(.rounded-br-lg)"#, ""),
    // Verified live (chat.z.ai): explicit assistant/user class pair (no ambiguity), and the
    // round stop button that replaces send while generating.
    ("zai", r#".chat-assistant"#, r#"button.rounded-full.bg-black"#),
    // Its data-message-author-role marks the WHOLE message row, which includes the timestamp and the
    // "Was this helpful?/Skip" controls -- they ended up inside the delivered answer ("OK\n\n1:16pm").
    // So we descend to the content container. If a redesign changes it, this selector stops matching
    // and the generic chain resumes from `[data-message-author-role="assistant"]`, i.e. back to the
    // previous behaviour: no risk of getting worse, only of not getting better.
    ("mistral", r#"[data-message-author-role="assistant"] .prose, [data-message-author-role="assistant"] [class*="markdown" i]"#, ""),
    ("poe", r#"[class*="Message_botMessageBubble"]"#, ""),
    ("kimi", r#".segment-assistant"#, r#".send-button-container.stop"#),
    ("meta", r#"[data-testid="assistant-message"]"#, r#"[data-testid="composer-stop-button"]"#),
    // copilot.com has no verified selectors anywhere (the only sources found cover
    // copilot.microsoft.com, a different domain/bundle) -- empty rather than guessed, same as
    // qwen: falls back to the generic chain, to be tightened after live DOM verification.
    ("copilot", r#"[data-testid="ai-message-body"]"#, ""),
];

/// Sends ALREADY OUT: (broadcast id, provider key). The single authority on "this message has been
/// sent", and it lives in Rust for a precise reason: the page is not a place to keep that fact. Every
/// provider navigation resets the JS context and causes the script to be re-injected, and it restarts
/// convinced it never sent -- that is how the same message ended up in TWO conversations (measured:
/// two different /c/<id> on ChatGPT from a single user send). Any heuristic based on reading the page
/// fails for the same reason: in the NEW page the message is not there, so it "looks unsent". Marked
/// by the fill loop at the instant it pressed Enter, read before re-injecting: whoever arrives later
/// harvests the answer and does NOT send.
fn sent_marks() -> &'static Mutex<HashSet<(String, String)>> {
    static S: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}
fn already_sent(bid: &str, key: &str) -> bool {
    sent_marks().lock().unwrap().contains(&(bid.to_string(), key.to_string()))
}

/// WARM TABS. A brand-new conversation used to cost a full page load on the critical path: the
/// frontend builds a fresh URL, we navigate there, and only then can we type and send. Measured on
/// ChatGPT: ~4s of the ~8.8s the user waits. So after an answer has been delivered we send the tab
/// back to an EMPTY new conversation in the background; the next send then finds the page already
/// loaded and only has to type into it -- the same path a follow-up message takes (measured 3.8s).
///
/// `fresh_bases`: per provider, the fresh-conversation URL with the prompt stripped out (the `q=` /
/// `prompt=` parameters carry the message; everything else -- notably the temporary-chat markers --
/// must be preserved). Recorded from the URL the frontend already sends us, so no new command and no
/// duplicated knowledge of provider URLs.
/// `prewarmed`: which providers are currently sitting on such a page, and at which URL, so a send can
/// tell "ready to type into" from "the user browsed somewhere else in the meantime".
fn fresh_bases() -> &'static Mutex<HashMap<String, String>> {
    static F: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(HashMap::new()))
}
fn prewarmed() -> &'static Mutex<HashMap<String, String>> {
    static P: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Providers whose pre-warm navigation has STARTED but not finished. A page that is still loading is
/// not a warm tab: injecting into it is worse than navigating normally, because committing the new
/// document wipes the injected script (measured: a send into a page pre-warmed 4s earlier took 29s
/// instead of 3s). A tab is only promoted to `prewarmed` when its load actually completes.
fn prewarming() -> &'static Mutex<HashSet<String>> {
    static W: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(HashSet::new()))
}

/// The fresh URL without the message in it. Keeps scheme/host/path and every parameter EXCEPT the
/// ones that carry the prompt, so `?temporary-chat=true` (and any other provider marker) survives:
/// pre-warming into a non-anonymous conversation would silently break the user's incognito setting.
fn strip_prompt_params(url: &str) -> Option<String> {
    let mut u = url.parse::<Url>().ok()?;
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| k != "q" && k != "prompt")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    u.set_query(None);
    if !kept.is_empty() {
        let mut qs = u.query_pairs_mut();
        for (k, v) in kept {
            qs.append_pair(&k, &v);
        }
    }
    Some(u.to_string())
}

/// Is this webview still sitting where we pre-warmed it? Compared on origin + path + the presence of
/// the temporary-chat markers, not on the exact query string: providers rewrite their own URL after
/// loading (ChatGPT turns `?q=` into `&prompt=`), so an exact match would always fail.
fn prewarm_still_valid(current: &str, expected: &str) -> bool {
    let (Ok(c), Ok(e)) = (current.parse::<Url>(), expected.parse::<Url>()) else {
        return false;
    };
    if c.origin() != e.origin() || c.path() != e.path() {
        return false;
    }
    let temp_of = |u: &Url| {
        u.query_pairs()
            .any(|(k, v)| (k == "temporary-chat" || k == "incognito") && v != "false")
    };
    temp_of(&c) == temp_of(&e)
}

/// The provider has a send or a harvest IN FLIGHT. Queries the only two structures that know:
/// `pending_injections` (fill waiting for the page to load) and `active_harvests` (answer incoming).
/// Used by the idle-provider janitor in `browser.rs`: suspending a page while it is working would
/// freeze its JS and the answer would never arrive.
pub(crate) fn provider_busy(key: &str) -> bool {
    pending_injections().lock().unwrap().contains_key(key)
        || active_harvests().lock().unwrap().contains_key(key)
}

/// Providers whose "generating" marker (the second field of `HARVEST_SELECTORS`) has been verified
/// live, and where HARVEST_JS may therefore trust its disappearance as a real end-of-answer event
/// instead of waiting for text stability. Being wrong here truncates answers, so it is opened one
/// provider at a time, after measuring: ChatGPT first (`button[data-testid="stop-button"]`).
/// `KOTO_NO_FASTDONE=1` turns it off, to compare with and without on the SAME binary.
fn fast_done_for(key: &str) -> bool {
    if std::env::var("KOTO_NO_FASTDONE").is_ok() {
        return false;
    }
    matches!(key, "openai")
}

fn selectors_for(key: &str) -> (&'static str, &'static str) {
    HARVEST_SELECTORS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, a, b)| (*a, *b))
        .unwrap_or(("", ""))
}

/// Harvest script (appended AFTER the fill script; both are independent IIFEs).
/// ARM: waits until the composer transitions filled -> empty (message accepted) or the
///      answer container visibly changes from its injection-time snapshot.
/// HARVEST: 1s poll (Chromium clamps hidden-page timers to 1s) until the answer text is
///      stable for 3 polls with no "stop" button; 180s budget; heartbeat every 3 polls.
/// DELIVER: via `window.__ktPush` (PUSH_HELPER_JS) — direct Tauri IPC when available (whole
///      answer in one call, no delay), else the navigation-sentinel fallback (chunked, spaced
///      200ms apart: rapid successive location.href assignments coalesce — only the last fires).
const PUSH_HELPER_JS: &str = r#"
if (!window.__ktPush) {
  window.__ktPushNav = function(obj){
    var q = [];
    for (var k in obj) { if (obj[k] === undefined || obj[k] === null) continue; q.push(encodeURIComponent(k)+'='+encodeURIComponent(String(obj[k]))); }
    try { window.location.href = 'https://kotodama.result/?' + q.join('&'); } catch(e){}
  };
  window.__ktPush = function(obj){
    try {
      if (window.__TAURI__ && window.__TAURI__.core && typeof window.__TAURI__.core.invoke === 'function') {
        window.__TAURI__.core.invoke('kotodama_push', obj).catch(function(err){
          try { window.__ktPushNav({ b: obj.b, k: obj.k, st: 'diag', d: '__IPCERR__: ' + (err && (err.message||JSON.stringify(err))) }); } catch(e){}
          window.__ktPushNav(obj);
        });
        return;
      }
    } catch(e){}
    window.__ktPushNav(obj); // no Tauri IPC bridge in this page -> sentinel navigation fallback
  };
}
"#;
/// Hides screen-reader-only labels (e.g. "Claude ha risposto:", ChatGPT's hidden "Modifica"/"Edit"
/// label next to the pencil icon) that are visually clipped, not `display:none`, so they are still
/// PART OF A NATIVE SELECTION -- a manual (or inline-transform) Ctrl+A/Ctrl+C on the page copies
/// them right along with the real message text. `display:none` removes them from layout entirely,
/// which excludes them from both `innerText` (harvesting) AND any native text selection (copying).
/// Matches by CSS class/attribute pattern only (developer-set, never translated) so this holds in
/// EVERY UI language without per-language text matching -- same principle as the rest of the
/// language-independent selectors in this codebase.
pub(crate) const SR_HIDE_JS: &str = r##"
(function(){
  try {
    if (!document.getElementById('__ktSrHide')) {
      var st = document.createElement('style');
      st.id = '__ktSrHide';
      // sr-only labels ("Claude ha risposto:") + collapsed thinking blocks ("Ha pensato per 1s").
      // Claude nests the thinking toggle INSIDE the answer container as an interactive control:
      // hide buttons/summaries within it too (real answer text never lives in a button).
      st.textContent = '.sr-only,[class*="sr-only"],[class*="screen-reader"],[data-testid*="sr-only"],'
        + '[class*="thinking" i],[class*="thought" i],[data-testid*="thinking" i],'
        + '.font-claude-message button,.font-claude-message summary{display:none !important;}';
      document.head.appendChild(st);
    }
  } catch(e){}
})();
"##;
/// DISCOVERY probe (debug only, `KOTO_NETPROBE=1`): wraps the page's own network APIs to learn how a
/// provider actually streams its answer, so the harvest can be driven by REAL EVENTS instead of
/// polling the DOM and inferring the end from text stability (measured: 6s of pure waiting after the
/// answer was already complete, and a warm turn that never concluded at all in 180s).
///
/// Deliberately observation-only: it logs method/URL/status/content-type, the first bytes of each
/// streamed chunk and the moment the stream CLOSES -- which is the event we want. Nothing is
/// intercepted or altered, and every hook is wrapped so a failure can never break the provider page.
/// Must be injected BEFORE the fill script, otherwise the send request itself is missed.
const NET_PROBE_JS: &str = r##"
(function(){
  if (window.__ktNetProbe) return;
  window.__ktNetProbe = true;
  function say(m){ try { if (window.__ktPush) window.__ktPush({ b: __kt_bid, k: __kt_key, st: 'diag', d: 'NET ' + String(m).slice(0,700) }); } catch(e){} }
  var seq = 0;
  // fetch: the modern streaming path. The body is TEE'd so the page keeps its own copy untouched.
  try {
    var of = window.fetch;
    window.fetch = function(input, init){
      var id = ++seq;
      var url = ''; try { url = (typeof input === 'string') ? input : (input && input.url) || ''; } catch(e){}
      var method = (init && init.method) || (input && input.method) || 'GET';
      var short = url.replace(/^https?:\/\/[^/]+/, '').slice(0, 110);
      return of.apply(this, arguments).then(function(res){
        var ct = '';
        try { ct = res.headers.get('content-type') || ''; } catch(e){}
        say('#' + id + ' ' + method + ' ' + short + ' -> ' + res.status + ' ct=' + ct);
        var streamy = /event-stream|x-ndjson|octet-stream/i.test(ct);
        if (!streamy || !res.body || !res.body.tee) return res;
        try {
          var pair = res.body.tee();
          var mine = pair[0].getReader(), chunks = 0, bytes = 0, t0 = Date.now(), first = '';
          (function pump(){
            mine.read().then(function(r){
              if (r.done) {
                say('#' + id + ' STREAM END chunks=' + chunks + ' bytes=' + bytes + ' ms=' + (Date.now() - t0) + ' first=' + JSON.stringify(first.slice(0,220)));
                return;
              }
              chunks++; bytes += (r.value && r.value.length) || 0;
              if (chunks <= 2) { try { first += new TextDecoder().decode(r.value); } catch(e){} }
              pump();
            }, function(){ say('#' + id + ' STREAM ERROR chunks=' + chunks); });
          })();
          return new Response(pair[1], { status: res.status, statusText: res.statusText, headers: res.headers });
        } catch(e) { say('#' + id + ' tee failed: ' + e); return res; }
      });
    };
  } catch(e) { say('fetch hook failed: ' + e); }
  // EventSource: the other streaming shape some providers use.
  try {
    var OES = window.EventSource;
    if (OES) {
      window.EventSource = function(u, c){
        var id = ++seq;
        say('#' + id + ' EventSource ' + String(u).replace(/^https?:\/\/[^/]+/, '').slice(0,110));
        var es = new OES(u, c);
        es.addEventListener('message', function(ev){ if (id) { say('#' + id + ' ES msg ' + JSON.stringify(String(ev.data).slice(0,160))); id = 0; } });
        es.addEventListener('error', function(){ say('#' + id + ' ES error/close'); });
        return es;
      };
      window.EventSource.prototype = OES.prototype;
    }
  } catch(e) { say('EventSource hook failed: ' + e); }
})();
"##;

/// Turns "the answer is finished" into an EVENT instead of an inference.
///
/// Every provider streams its answer over a long-lived HTTP response, and when it has finished it
/// CLOSES that response. That close is the exact signal we want, and it needs no knowledge of any
/// provider's DOM or of its JSON format: the discriminator is the content type
/// (`text/event-stream`, ndjson), which is how streaming is done on the web, not a ChatGPT detail.
/// Captured live on ChatGPT: `POST /backend-api/f/conversation -> text/event-stream`, stream closed
/// 1581ms after the request, while the DOM-stability path was still counting and delivered at 4.0s.
///
/// Only streams opened AFTER our own send count (`__ktSentAt`, set by the fill script when it presses
/// Enter): a page can keep telemetry or notification streams open, and those must never be mistaken
/// for the answer. The response body is TEE'd, so the page keeps its own untouched copy and the
/// provider's UI behaves exactly as before -- this observes, it never intercepts.
/// If anything here fails, or the provider does not stream, nothing happens and the stability
/// counters in HARVEST_JS remain in charge, exactly as before.
const STREAM_WATCH_JS: &str = r##"
(function(){
  if (window.__ktStreamWatch) return;
  window.__ktStreamWatch = true;
  var STREAMY = /event-stream|x-ndjson|application\/stream/i;
  function ended(){ try { if (window.__ktStreamEnd) window.__ktStreamEnd(); } catch(e){} }
  try {
    var of = window.fetch;
    if (typeof of !== 'function') return;
    window.fetch = function(){
      var p = of.apply(this, arguments);
      try {
        if (!window.__ktSentAt) return p;              // nothing sent yet: not our stream
        return p.then(function(res){
          try {
            var ct = (res.headers && res.headers.get('content-type')) || '';
            if (!STREAMY.test(ct) || !res.body || typeof res.body.tee !== 'function') return res;
            var pair = res.body.tee();
            var mine = pair[0].getReader();
            // An OPEN stream means the provider is working on our answer right now. The arming loop
            // reads this before deciding to give up: a model that reasons before writing can stay
            // silent in the DOM for longer than the arming budget, and declaring `sendfail` there is
            // wrong twice over -- the message did go out, and the answer is on its way.
            // Two counters, two questions. `__ktStreamOpen` = how many streams are open RIGHT NOW
            // (is it generating?). `__ktStreamEver` = has one ever opened since this injection (did
            // the message reach the provider at all?). The second is what the arming loop needs: a
            // model can finish its reasoning stream and open the answer one a moment later, and in
            // that gap the first counter is legitimately zero while the send was plainly fine.
            try { window.__ktStreamOpen = (window.__ktStreamOpen || 0) + 1; } catch(e){}
            try { window.__ktStreamEver = (window.__ktStreamEver || 0) + 1; } catch(e){}
            function closed(){ try { window.__ktStreamOpen = Math.max(0, (window.__ktStreamOpen || 1) - 1); } catch(e){} ended(); }
            (function pump(){
              mine.read().then(function(r){ if (r.done) { closed(); return; } pump(); }, function(){ closed(); });
            })();
            return new Response(pair[1], { status: res.status, statusText: res.statusText, headers: res.headers });
          } catch(e) { return res; }
        });
      } catch(e) { return p; }
    };
  } catch(e){}
  try {
    var OES = window.EventSource;
    if (OES) {
      window.EventSource = function(u, c){
        var es = new OES(u, c), got = false;
        try {
          es.addEventListener('message', function(){ got = true; });
          // For EventSource a close surfaces as 'error'. Only counted once at least one message has
          // arrived: an error before any data is a failed connection, not a finished answer.
          es.addEventListener('error', function(){ if (got && window.__ktSentAt) ended(); });
        } catch(e){}
        return es;
      };
      try { window.EventSource.prototype = OES.prototype; } catch(e){}
    }
  } catch(e){}
})();
"##;

const HARVEST_JS: &str = r##"
(function(){
  var BID = __kt_bid, KEY = __kt_key, ANS_SEL = __kt_ans, BUSY_SEL = __kt_busy, FRESH = __kt_fresh;
  var FAST_DONE = __kt_fast;   // trust the provider's own "generating" marker, see below
  // Set by Rust on a re-injection after the page navigated: this message is already out.
  var KNOWN_SENT = (typeof __kt_sent !== 'undefined') && !!__kt_sent;
  // The prompt we just sent (from the fill script): never harvest our own message back
  // (the generic selector chain can match the USER bubble on providers without a
  // dedicated assistant selector).
  var SENT = (typeof __apb_text === 'string') ? __apb_text.trim() : '';
  window.__ktBid = BID;               // a newer injection overwrites; older loops self-terminate
  var t0 = Date.now();
  function lastMatch(sel){
    if (!sel) return null;
    try { var els = document.querySelectorAll(sel); return els.length ? els[els.length-1] : null; }
    catch(e){ return null; }
  }
  // A candidate is never valid if it IS the composer/input control itself, or directly wraps/is
  // wrapped BY it -- when no real answer exists yet (e.g. a logged-out page with zero messages),
  // every selector in the fallback chain below can end up matching the input toolbar itself
  // (observed on Grok: `.query-bar`, the composer's own wrapper, picked up as the "answer"
  // because it happens to also match a generic candidate -- its innerText was a mode-toggle
  // label, not a reply). Checked with `matches()` on the candidate ITSELF, never `closest()` on
  // its ancestors: a real answer bubble commonly lives inside the SAME outer `<form>`/composer
  // region as the input box (observed on ChatGPT), so rejecting anything merely NESTED under
  // such a wrapper throws real answers away too -- confirmed live: harvest found "OK" via
  // ChatGPT's own dedicated selector, composer had correctly emptied (message sent), and this
  // check discarded it anyway before the ancestor-vs-self fix below.
  function isInputArea(el){
    if (!el) return false;
    try {
      var composerEl = findComposerEl();
      if (composerEl && el === composerEl) return true;
      return el.matches('form, [role="textbox"], [class*="query-bar" i]');
    } catch(e){ return false; }
  }
  function getAnswerEl(){
    var el = lastMatch(ANS_SEL)
        || lastMatch('[data-message-author-role="assistant"]')
        || lastMatch('[class*="assistant" i]')
        || lastMatch('[class*="answer" i]')
        || lastMatch('[class*="response" i]')
        || lastMatch('.markdown, .prose')
        || lastMatch('article')
        || lastMatch('[class*="bubble" i]');
    return isInputArea(el) ? null : el;
  }
  function answerTxt(){
    var el = getAnswerEl();
    var t = el ? ((el.innerText||'').trim()) : '';
    t = t.replace(new RegExp('[' + String.fromCharCode(57344) + '-' + String.fromCharCode(63743) + ']', 'g'), '').trim();   // strip private-use icon glyphs (UI font icons)
    if (t && SENT && t === SENT) return '';   // that's our own message, not an answer
    // Drop a leading collapsed-thinking header ("Ha pensato per 2s" / "Thought for 2s"):
    // Claude nests it inside the answer container with no stable class to hide via CSS.
    var lines = t.split('\n');
    if (lines.length > 1 && lines[0].trim().length < 40
        && /^(ha pensato|thought|pensato|processo di ragionamento|reasoning|ragionamento|r[ée]fl[ée]ch|pens[óé]|dachte|thinking)/i.test(lines[0].trim())) {
      lines.shift();
      t = lines.join('\n').trim();
    }
    // Trailing timestamp: some providers print it inside the message row (measured on Mistral:
    // "OK\n\n1:16pm"). Stripped ONLY in the hour:minute form with optional am/pm -- digits and a
    // colon, hence language-independent -- and ONLY when it sits ON ITS OWN LINE, which is how a
    // message-row timestamp is printed. The earlier version matched a trailing time anywhere and so
    // ate real content: an answer that IS a time ("17:25") was erased down to nothing, and the
    // harvest then waited out its whole budget and reported a failure for an answer sitting in plain
    // sight (measured on all four of Claude/ChatGPT/DeepSeek/Gemini asked for an arrival time). An
    // answer ENDING in a time ("arriva alle 17:25") lost it the same way. The final guard makes the
    // rule unable to empty an answer under any input.
    var noStamp = t.replace(/\n\s*\d{1,2}:\d{2}(:\d{2})?\s*(am|pm|AM|PM)?\s*$/, '').trim();
    if (noStamp) t = noStamp;
    return t;
  }
  // "Still generating?" - LANGUAGE-INDEPENDENT (no localized aria-label text). Uses the
  // per-provider BUSY_SEL (data-* attrs) + neutral streaming markers. NB: this only speeds
  // up completion; the reliable, language-independent signal is text STABILITY (below).
  // HTML -> lightweight Markdown, run INSIDE the untrusted provider page. Markdown-syntax text
  // can carry no executable content, so this is safe to ship over IPC and render as-is in the
  // trusted main window (after escaping) -- unlike transporting raw HTML, which would need a
  // sanitizer. Covers what actually shows up in provider answers: tables, fenced code, bold/
  // italic, inline code, headings, ordered/unordered lists, blockquotes, links.
  function elToMd(el){
    if (!el) return '';
    function esc(s){ return (s||'').replace(/[*_`]/g, '\\$&'); }
    function inlineMd(node){
      var out = '';
      node.childNodes.forEach(function(n){
        if (n.nodeType === 3) { out += esc(n.textContent); return; }
        if (n.nodeType !== 1) return;
        var tag = n.tagName.toLowerCase();
        if (tag === 'br') { out += '\n'; return; }
        if (tag === 'code') { out += '`' + n.textContent + '`'; return; }
        if (tag === 'strong' || tag === 'b') { out += '**' + inlineMd(n) + '**'; return; }
        if (tag === 'em' || tag === 'i') { out += '*' + inlineMd(n) + '*'; return; }
        if (tag === 'a') { var href = n.getAttribute('href') || ''; out += '[' + inlineMd(n) + '](' + href + ')'; return; }
        out += inlineMd(n);
      });
      return out;
    }
    function blockMd(node, depth){
      var out = [];
      node.childNodes.forEach(function(n){
        if (n.nodeType === 3) { var t = n.textContent.trim(); if (t) out.push(esc(t)); return; }
        if (n.nodeType !== 1) return;
        var tag = n.tagName.toLowerCase();
        if (/^h[1-6]$/.test(tag)) { out.push('#'.repeat(+tag[1]) + ' ' + inlineMd(n).trim()); return; }
        if (tag === 'pre') {
          var codeEl = n.querySelector('code');
          var lang = '';
          if (codeEl) { var m = (codeEl.className||'').match(/language-(\S+)/); if (m) lang = m[1]; }
          out.push('```' + lang + '\n' + (codeEl || n).textContent.replace(/\n+$/, '') + '\n```');
          return;
        }
        if (tag === 'blockquote') { out.push(blockMd(n, depth).split('\n').map(function(l){ return '> ' + l; }).join('\n')); return; }
        if (tag === 'ul' || tag === 'ol') {
          var i = 0;
          n.querySelectorAll(':scope > li').forEach(function(li){
            i++;
            var marker = tag === 'ol' ? (i + '. ') : '- ';
            out.push('  '.repeat(depth) + marker + inlineMd(li).trim());
          });
          return;
        }
        if (tag === 'table') {
          var rows = n.querySelectorAll('tr'), lines = [];
          rows.forEach(function(tr, ri){
            var cells = tr.querySelectorAll('th,td');
            var cellTxt = Array.prototype.map.call(cells, function(c){ return inlineMd(c).trim().replace(/\|/g, '\\|'); });
            lines.push('| ' + cellTxt.join(' | ') + ' |');
            if (ri === 0) lines.push('| ' + cellTxt.map(function(){ return '---'; }).join(' | ') + ' |');
          });
          out.push(lines.join('\n'));
          return;
        }
        if (tag === 'p' || tag === 'div') { var s = blockMd(n, depth); if (s.trim()) out.push(s); return; }
        var txt = inlineMd(n).trim();
        if (txt) out.push(txt);
      });
      return out.join('\n\n');
    }
    try { return blockMd(el, 0).trim(); } catch(e){ return ''; }
  }
  // The provider's OWN verified "generating" marker, without the generic fallbacks. Kept separate
  // from `isBusy()` on purpose: the fallbacks are guesses that false-positive on unrelated controls,
  // and the fast-completion path below is only sound on a marker we have verified for that provider.
  function busyVerified(){
    if (!BUSY_SEL) return false;
    try {
      var els = document.querySelectorAll(BUSY_SEL);
      for (var i=0;i<els.length;i++){ if (els[i].offsetParent !== null) return true; }
    } catch(e){}
    return false;
  }
  function isBusy(){
    var sels = [BUSY_SEL, '[data-testid*="stop" i]', '[data-is-streaming="true"]', '[class*="result-streaming" i]', '[class*="is-streaming" i]'];
    for (var i=0;i<sels.length;i++){
      if (!sels[i]) continue;
      try {
        var els = document.querySelectorAll(sels[i]);
        for (var j=0;j<els.length;j++){ if (els[j].offsetParent !== null) return true; }
      } catch(e){}
    }
    return false;
  }
  function findComposerEl(){
    // Same VISIBLE-only pick as the fill script (ChatGPT keeps a hidden legacy textarea).
    var el = null;
    var sels = ['textarea:not([readonly]):not([aria-hidden="true"])', '[contenteditable="true"]', 'div[role="textbox"]'];
    for (var i=0;i<sels.length && !el;i++){
      var els = document.querySelectorAll(sels[i]);
      for (var j=0;j<els.length;j++){ if (els[j].offsetParent !== null) { el = els[j]; break; } }
    }
    return el;
  }
  function composerVal(){
    var el = findComposerEl();
    if (!el) return null;
    return (el.value !== undefined ? el.value : el.innerText) || '';
  }
  // Blocked/needs-manual-intervention wall: a password field (classic login form) OR a captcha
  // challenge (some providers gate SENDING behind one when not authenticated instead of showing a
  // login form -- e.g. Z.ai's own "chat-captcha-trigger" button). Matched by code-level
  // class/id/data-testid/iframe-src substrings, never translated text, so this holds in every UI
  // language. A captcha can in principle appear for anti-bot reasons even while logged in; treated
  // the same as a login wall here because either way the send is stuck and needs the user to open
  // the real page and resolve it by hand.
  // Landed on the provider's OWN dedicated login URL, reached by an automatic redirect the
  // provider's own app code performed (never guessed/typed by us) -- confirmed live (Playwright,
  // real logout+relogin, 2026-08-10) for these three: visiting the base chat URL while logged out
  // bounces straight to this path on its own. Far more reliable than DOM/text scraping (the site
  // itself is telling us it needs a login), and the path never changes with UI language.
  // See docs/research/login-detection-providers.md.
  function loginUrlRedirected(){
    try {
      var p = location.pathname || '';
      if (KEY === 'anthropic' && p.indexOf('/login') !== -1) return true;
      if (KEY === 'deepseek' && p.indexOf('/sign_in') !== -1) return true;
      if (KEY === 'poe' && p.indexOf('/login') !== -1) return true;
    } catch(e){}
    return false;
  }
  function authWallPresent(){
    try {
      if (loginUrlRedirected()) return true;
      if (document.querySelector('input[type="password"]')) return true;
      if (document.querySelector('[class*="captcha" i], [id*="captcha" i], [data-testid*="captcha" i], iframe[src*="captcha" i], iframe[src*="turnstile" i]')) return true;
      // Some providers (observed: Meta AI) show NEITHER a password field nor a captcha when
      // signed out -- just a visible "log in"/"sign in" control and no composer anywhere on
      // the page. Matched by testid/id substring (developer-set, language-independent, same
      // principle as the captcha check above) + composer absence, so a login link that's
      // merely present-but-irrelevant (e.g. "sign in with another account" while already
      // logged in, composer working fine) doesn't false-positive.
      var loginEls = document.querySelectorAll('[data-testid*="login" i], [id*="login-button" i], [data-testid*="sign-in" i], [id*="sign-in-button" i]');
      if (loginEls.length && composerVal() === null) {
        for (var i=0;i<loginEls.length;i++){ if (loginEls[i].offsetParent !== null) return true; }
      }
      // Grok-specific: its login/signup buttons carry NO testid/id/distinguishing class of their
      // own, and the generic Tailwind wrapper classes around them are NOT deterministic between
      // page loads either (3 separate live captures showed 3 different structures -- confirmed
      // NOT a selector bug, the DOM itself varies, likely an A/B test or JIT class hashing).
      // Structural matching is therefore a dead end here. `browser.rs`'s FORCE_EN_LANG_JS pins
      // this page's `navigator.language` to English specifically so this text check is reliable
      // regardless of the user's own app/OS language -- Grok always renders "Log in"/"Sign up"
      // here, never a translation of them.
      if (KEY === 'grok') {
        var els = document.querySelectorAll('button, a');
        for (var gi=0; gi<els.length; gi++){
          var gt = (els[gi].innerText || '').trim();
          if ((gt === 'Log in' || gt === 'Sign up') && els[gi].offsetParent !== null) return true;
        }
      }
    } catch(e){}
    return false;
  }
  function deliver(st, txt, md){
    if (window.__ktBid !== BID) return;
    window.__ktBid = null;
    txt = txt || '';
    var MAXC = 150000, trunc = 0;
    if (txt.length > MAXC) { txt = txt.slice(0, MAXC); trunc = 1; }
    var hasIpc = !!(window.__TAURI__ && window.__TAURI__.core && typeof window.__TAURI__.core.invoke === 'function');
    if (hasIpc) {
      // Direct IPC: no URL-length or navigation-coalescing constraints -> the whole answer goes
      // in ONE call, no artificial delay. `md` (elToMd() output) travels ONLY on this path -- the
      // chunked nav fallback below never carries it, degrading gracefully to plain text.
      window.__ktPush({ b: BID, k: KEY, st: st, s: 0, n: 1, tr: !!trunc, d: txt, md: md || '' });
      return;
    }
    // Fallback (no Tauri bridge in this page): chunk + space sends 200ms apart — rapid successive
    // location.href assignments coalesce, only the last would fire.
    var CH = 1500, n = Math.max(1, Math.ceil(txt.length / CH)), i = 0;
    function sendNext(){
      if (i >= n) return;
      window.__ktPush({ b: BID, k: KEY, st: st, s: i, n: n, tr: !!trunc, d: txt.slice(i*CH, (i+1)*CH) });
      i++;
      if (i < n) setTimeout(sendNext, 200);
    }
    // First chunk DELAYED: a heartbeat may have fired in this same tick, and two
    // location.href assignments back-to-back coalesce (the first one is lost).
    setTimeout(sendNext, 250);
  }
  // Fresh conversation: NO previous answer exists; ?q= providers (ChatGPT) may auto-send
  // and even FINISH answering before this script runs, so snapshotting would swallow the
  // whole answer. Warm turns: snapshot the previous answer so we wait for the new one.
  var initialAnswer = FRESH ? '' : answerTxt();
  var sawText = false, armTries = 0, authCensusSent = false;
  // One-time diagnostic (debug log only, via the existing 'diag' push): inventories every
  // visible <a>/<button> whose text/attrs look login-related, so a provider that shows a
  // WORKING-LOOKING composer while still logged out (observed: Grok -- typing looks fine, only
  // sending actually fails) can be diagnosed from real data instead of guessed at again. Fires
  // once per harvest regardless of which path (login/answer/sendfail) it ends up taking.
  function authCensus(){
    if (authCensusSent) return;
    authCensusSent = true;
    try {
      var out = [], seen = document.querySelectorAll('a, button'), n = 0;
      for (var i=0;i<seen.length && n<10;i++){
        var el = seen[i]; if (el.offsetParent === null) continue;
        var txt = (el.innerText||'').trim().slice(0,20);
        var idl = (el.getAttribute('data-testid')||el.getAttribute('aria-label')||el.id||'').slice(0,25);
        var hay = (txt+' '+idl+' '+(el.getAttribute('href')||'')).toLowerCase();
        if (!/log.?in|sign.?in|sign.?up|regist|accedi|login|entrar|anmeld|connexion/.test(hay)) continue;
        var par = el.parentElement, pdesc = par ? (par.tagName+'.'+(par.className||'').toString().slice(0,60)) : '';
        var gpar = par && par.parentElement, gdesc = gpar ? (gpar.tagName+'.'+(gpar.className||'').toString().slice(0,60)) : '';
        out.push(el.tagName+':"'+txt+'" id='+idl+' cls='+(el.className||'').toString().slice(0,50)+' parent='+pdesc+' gparent='+gdesc);
        n++;
      }
      out.push('composer='+(composerVal()===null?'NONE':'present'));
      try { out.push('nav.language='+navigator.language+' nav.languages='+JSON.stringify(navigator.languages)); } catch(e){}
      try { out.push('cookie='+(document.cookie||'').slice(0,300)); } catch(e){}
      window.__ktPush({ b: BID, k: KEY, st: 'diag', d: ('AUTHWALL-CENSUS '+out.join(' || ')).slice(0,1400) });
    } catch(e){}
  }
  /* ===================== END OF ANSWER AS AN EVENT =====================
     Everything below this line used to be inferred: the answer was considered finished when its text
     stopped changing for N polls. Measured cost of that inference on ChatGPT: 6.0s of pure waiting
     after the answer was already complete on screen (and one warm turn that never concluded at all
     in 180s). The provider itself knows exactly when it has finished -- it closes the response
     stream. `STREAM_WATCH_JS` watches the page's own network calls and calls in here when a streaming
     response opened after our send has CLOSED; from that moment the text in the DOM is final, so
     there is nothing left to wait for.
     The stability counters stay as the fallback: providers that do not stream, or a hook that sees
     nothing, keep working exactly as before. */
  var streamEnded = false, harvesting = false, stepNow = null;
  window.__ktStreamEnd = function(){
    if (streamEnded || window.__ktBid !== BID) return;
    streamEnded = true;
    try { if (window.__ktDiag) window.__ktPush({ b: BID, k: KEY, st: 'diag', d: 'STREAM-END event received' }); } catch(e){}
    // If the answer had not even been detected yet, stop waiting for it and start harvesting now.
    if (!harvesting) { try { clearInterval(armIv); } catch(e){} harvest(); return; }
    if (stepNow) stepNow();   // already harvesting: evaluate immediately, do not wait for the next poll
  };
  var armT0 = Date.now();
  var armIv = setInterval(function(){
    if (window.__ktBid !== BID) { clearInterval(armIv); return; }
    // The budget below must measure the time since the message COULD have gone out, not since this
    // script was injected. While the page is still hydrating there is no composer, nothing has been
    // sent, and counting that time punishes a provider for how many tabs happen to be loading at
    // once. Measured on a 10-provider broadcast from cold: the last in the queue (Qwen, Z.ai) were
    // declared `sendfail` at 45-49s having simply not reached their own composer yet -- the very same
    // providers answer fine when sent to alone. So the clock only runs once there is a composer.
    if (composerVal() !== null) armTries++;
    // Absolute ceiling, so a page that never produces a composer cannot wait forever.
    // A message we KNOW went out and that produced nothing in 180s is a timeout, not a failed send.
    if (Date.now() - armT0 > 180000) { clearInterval(armIv); census(); setTimeout(function(){ deliver(KNOWN_SENT ? 'timeout' : 'sendfail', ''); }, 300); return; }
    authCensus();
    if (authWallPresent()) { clearInterval(armIv); deliver('login',''); return; }
    // Arms the stream watcher as soon as the message is known to be out. Needed because on `?q=`
    // providers the URL itself sends, so our fill script exits without ever pressing Enter and never
    // sets `__ktSentAt` -- which left exactly the fastest providers falling back to text stability
    // (measured: ChatGPT decided by stability at poll 5 while its stream had long since closed). The
    // answer's stream is still open at this point, so its close is captured.
    function armWatcher(){ try { if (!window.__ktSentAt) window.__ktSentAt = Date.now(); } catch(e){} }
    var cur = answerTxt();
    if (cur && cur !== initialAnswer) { armWatcher(); clearInterval(armIv); harvest(); return; }  // answer streaming
    var v = composerVal();
    if (v !== null) {
      if (v.trim().length > 0) { sawText = true; }
      else if (sawText) { armWatcher(); clearInterval(armIv); harvest(); return; }    // composer emptied = accepted
    }
    // ~30s: never sent. census() first (-> debug log), then deliver AFTER a gap: two
    // back-to-back location.href assignments coalesce and the first would be lost.
    // ~30s with nothing to show: normally that means the message never left. But NOT once a response
    // stream has been seen -- the provider took the message and is working on it, it simply has not
    // painted anything yet, which is exactly what a model that reasons before writing does. Checking
    // only "a stream is open right now" was not enough: measured on a reasoning question, all four
    // providers had their stream open and closed inside the budget and were declared `sendfail`
    // anyway, with the answer already on its way. `__ktStreamEver` is the honest test -- did this
    // send ever reach the provider -- and the harvest's own 180s budget remains the outer bound.
    // KNOWN_SENT closes the last hole: after the page navigates (Claude and friends jump to the new
    // conversation the moment the message goes out) the script is re-injected on a page where the
    // composer was never filled, so "field emptied = accepted" cannot fire, and the answer's stream
    // opened before the watcher was reinstalled, so neither counter sees it. Rust, however, knows
    // the send is out -- it marked it -- and passes that in. Calling THAT `sendfail` was simply
    // false, and it is what made every reasoning answer fail while quick ones worked.
    if (armTries > 60 && !window.__ktStreamOpen && !window.__ktStreamEver && !KNOWN_SENT) { clearInterval(armIv); census(); setTimeout(function(){ deliver('sendfail',''); }, 300); }
  }, 500);
  // One-shot DOM census when the harvest stays empty: which candidate selectors match
  // what on THIS provider's page. Only reaches the debug log (KOTODAMA_DEBUG) — it is
  // how new/changed provider DOMs get diagnosed without guessing.
  function census(){
    var sels = [ANS_SEL, '[data-message-author-role="assistant"]', '[class*="assistant" i]',
      '[class*="answer" i]', '[class*="response" i]', '.markdown, .prose', 'article',
      '[class*="message" i]', '[class*="bubble" i]'];
    var out = [];
    for (var i=0;i<sels.length;i++){
      if (!sels[i]) continue;
      try {
        var els = document.querySelectorAll(sels[i]);
        var lastTxt = els.length ? ((els[els.length-1].innerText||'').trim().replace(/\s+/g,' ').slice(0,60)) : '';
        out.push(sels[i].slice(0,34)+' >> n='+els.length+' last="'+lastTxt+'"');
      } catch(e){ out.push(sels[i].slice(0,34)+' >> ERR'); }
    }
    out.push('title="'+document.title.slice(0,60)+'"');
    // The counts above say a selector matched; they do not say WHICH element the harvest ends up
    // taking, and that is the part that goes wrong: on Qwen `[class*="message"]` matched nine
    // elements with the LAST one empty, so the answer sat in the page while the chain returned
    // nothing. Here we list the tail of that chain with each element's identity and text length, so
    // the provider's real answer container can be read off the page instead of guessed at.
    try {
      var tail = document.querySelectorAll('[class*="message" i], [class*="bubble" i], [class*="chat" i]');
      var td = [];
      for (var ti = Math.max(0, tail.length - 6); ti < tail.length; ti++) {
        var e = tail[ti];
        var cls = (typeof e.className === 'string' ? e.className : '').trim().split(/\s+/).slice(0,3).join('.');
        var role = e.getAttribute('data-role') || e.getAttribute('data-message-role') || '';
        td.push(e.tagName.toLowerCase() + (cls ? '.' + cls.slice(0,40) : '') + (role ? '[' + role + ']' : '')
          + ' len=' + ((e.innerText||'').trim().length));
      }
      out.push('tail=' + td.join(' ; '));
    } catch(e){}
    var c = composerVal(); out.push('composer='+(c===null?'NONE':('len'+c.length)));
    // ALL candidate fields, not only the chosen one: `composer=len1` on Grok did not say whether the
    // fill had landed in the wrong field or failed to take in the right one. Here we see each one's
    // tag/class/visibility/length/bottom-edge -- and the edge matters, because the fill picks the
    // BOTTOM-most while this diagnostic used to report the first.
    try {
      var csels = ['textarea:not([readonly]):not([aria-hidden="true"])', '[contenteditable="true"]', 'div[role="textbox"]'];
      var cc = [];
      for (var ci=0; ci<csels.length; ci++){
        var cels = document.querySelectorAll(csels[ci]);
        for (var cj=0; cj<cels.length && cc.length<8; cj++){
          var ce = cels[cj];
          var cv = (ce.value !== undefined ? ce.value : ce.innerText) || '';
          cc.push(ce.tagName + '.' + String(ce.className||'').slice(0,26)
            + '[' + (ce.offsetParent === null ? 'HID' : 'vis') + ',len' + cv.length
            + ',y' + Math.round(ce.getBoundingClientRect().bottom) + ']');
        }
      }
      out.push('composerCands=' + cc.join(' , '));
    } catch(e){}
    // Button inventory for tuning the SEND selector. It used to be the first 8 in DOM order: it ran
    // out on the sidebar's buttons and NEVER reached the composer, i.e. exactly the one thing worth
    // knowing when a provider changes its editor. Now two signals:
    //   1) sendSel  -> does findSendBtn's primary selector (browser.rs) still exist? is it enabled?
    //   2) btnsComposer -> the buttons in the composer's geometric band, with their x so we can tell
    //      which is the rightmost (the one the geometric fallback picks).
    try {
      var prim = document.querySelector('button[data-testid="send-button"], button[data-testid*="send" i], button[type="submit"]');
      out.push('sendSel=' + (prim ? ((prim.getAttribute('data-testid') || prim.type || 'submit')
        + (prim.disabled ? '!D' : '') + (prim.offsetParent === null ? '/HID' : '/vis')) : 'NONE'));
      var cel = findComposerEl(), bl = [];
      if (cel) {
        var cr = cel.getBoundingClientRect(), bs = document.querySelectorAll('button');
        for (var k=0;k<bs.length && bl.length<10;k++){
          var b = bs[k]; if (b.offsetParent === null) continue;
          var r = b.getBoundingClientRect();
          if (r.top < cr.top - 10 || r.top > cr.bottom + 72) continue;   // stessa fascia di findSendBtn
          var idl = (b.getAttribute('data-testid') || b.getAttribute('aria-label') || b.id
            || ('svg?' + (b.querySelector('svg') ? 'y' : 'n'))).slice(0,22);
          bl.push(idl + (b.disabled ? '!D' : '') + '@' + Math.round(r.left));
        }
      }
      out.push('btnsComposer=' + bl.join(','));
    } catch(e){}
    window.__ktPush({ b: BID, k: KEY, st: 'diag', d: out.join(' || ').slice(0,1400) });
  }
  // DISCOVERY probe (debug only, KOTO_THINKPROBE=1): reasoning models print their thinking in a
  // block that is a SIBLING of the answer, inside the same assistant turn -- so the answer selector
  // never sees it and we have nothing to show the user. Rather than guessing a selector per
  // provider, this dumps the structural identity (tag + data-* + class fragments) of every text
  // node group in the turn that sits OUTSIDE the answer element, so the real marker can be read off
  // a live page and codified. Emitted once, at delivery time, with the answer already complete.
  function thinkCensus(){
    try {
      var ans = getAnswerEl(); if (!ans) return;
      // Climb to the assistant TURN: the first ancestor that is meaningfully taller than the answer
      // (the reasoning block is what makes it taller). Bounded, or we end up at <body>.
      var wrap = ans.parentElement, hops = 0, ah = ans.getBoundingClientRect().height;
      while (wrap && hops < 8) {
        var wh = wrap.getBoundingClientRect().height;
        if (wh > ah + 24 && (wrap.innerText || '').length > (ans.innerText || '').length + 20) break;
        wrap = wrap.parentElement; hops++;
      }
      if (!wrap) return;
      function ident(el){
        var a = [el.tagName.toLowerCase()];
        for (var i=0;i<el.attributes.length;i++){
          var at = el.attributes[i];
          if (at.name.indexOf('data-') === 0 || at.name === 'id' || at.name === 'role' || at.name === 'aria-expanded') {
            a.push(at.name + '=' + String(at.value).slice(0,28));
          }
        }
        var cl = (typeof el.className === 'string' ? el.className : '').trim();
        if (cl) a.push('.' + cl.split(/\s+/).slice(0,4).join('.').slice(0,60));
        return a.join('|');
      }
      var out = ['THINK-CENSUS hops=' + hops + ' wrap=' + ident(wrap)];
      var all = wrap.querySelectorAll('*');
      for (var i=0; i<all.length && out.length < 14; i++) {
        var el = all[i];
        if (el === ans || ans.contains(el) || el.contains(ans)) continue;  // the answer itself
        var t = (el.innerText || '').trim();
        if (t.length < 12) continue;                                       // labels, icons, chrome
        // Only the OUTERMOST element of each text group: its children repeat the same text.
        if (el.parentElement && el.parentElement !== wrap && !ans.contains(el.parentElement)
            && (el.parentElement.innerText || '').trim().length === t.length) continue;
        out.push(ident(el) + ' len=' + t.length + ' "' + t.replace(/\s+/g,' ').slice(0,60) + '"');
      }
      window.__ktPush({ b: BID, k: KEY, st: 'diag', d: out.join(' || ').slice(0,1400) });
    } catch(e){}
  }
  function harvest(){
    harvesting = true;
    var last = '', stable = 0, polls = 0, sentCensus = false, sawBusy = false;
    // Timing instrumentation (debug only). `sinceLastChange` is the number that matters: how long
    // after the answer STOPPED GROWING we actually handed it over. Guessing it from the wall clock
    // conflates it with the model's own generation time.
    var t0 = Date.now(), lastChangeAt = t0, trace = [];
    var iv = null;
    function step(){
      if (window.__ktBid !== BID) { clearInterval(iv); return; }
      polls++;
      var txt = answerTxt();
      if (txt === initialAnswer) txt = '';   // still showing the previous answer, new one not in DOM yet
      var busy = isBusy();
      if (busyVerified()) sawBusy = true;    // the marker exists on this page and we have seen it
      if (txt && txt === last) { stable++; } else { stable = 0; lastChangeAt = Date.now(); }
      last = txt;
      // Per-poll trace: poll number, len, and WHICH busy signal is up (B = generic isBusy, v = the
      // provider's own verified marker). This is what tells whether a late delivery was the
      // stability count or a busy marker that never went away.
      if (window.__ktDiag && trace.length < 45) {
        trace.push(polls + (busy ? 'B' : '-') + (busyVerified() ? 'v' : '-') + ':' + txt.length);
      }
      // done = text stable N polls with no busy marker; OR stable 10 polls regardless
      // (some pages keep a false-positive "stop"-like control on screen forever, e.g. Qwen).
      // N is higher for SHORT text (<40 chars): a brief opener ("Ciao!") followed by a
      // thinking pause before the model continues can look "stable" for a few seconds even
      // though the answer isn't finished -- observed truncating real multi-sentence Claude
      // replies down to just the first word. Longer text stabilizing for 3s is a much safer
      // signal (a real answer that long rarely pauses mid-stream for multiple seconds).
      var neededStable = (txt.length < 40) ? 6 : 3;
      // FAST COMPLETION. The counts above infer the end from text stability, because providers emit
      // no "answer finished" event -- and for short answers they deliberately wait 6 polls (~6s),
      // since a brief opener plus a thinking pause looks stable. But where the provider has its OWN
      // verified "generating" marker, that marker going from PRESENT to ABSENT is a real end signal,
      // not an inference: waiting six seconds on top of it buys nothing. Two conditions, both
      // required: the marker must have been SEEN during this answer (if it never appeared we cannot
      // read anything into its absence), and the provider must be one where it is verified live.
      if (FAST_DONE && sawBusy && !busy && txt) neededStable = 2;
      // THE EVENT WINS over the stability counts, but it is not the whole story: the stream closing
      // means the SERVER has finished sending, not that the page has finished PAINTING. Measured on
      // Claude: at the poll right after the close the DOM held "O" of "OK", with its own
      // `data-is-streaming="true"` still up -- delivering there truncated the answer to one letter.
      // So the event still requires the page to agree: no busy marker, and the text unchanged for one
      // poll. That costs about a second and removes the truncation, while still being far ahead of
      // the six polls the stability rule would have waited.
      var doneByEvent = streamEnded && !!txt && !busy && stable >= 1;
      if (doneByEvent || (stable >= neededStable && !busy) || stable >= 10) {
        clearInterval(iv);
        // How long the completion decision took, in polls (~1s each): the number to compare when
        // tuning the thresholds above, instead of guessing from the wall clock.
        if (window.__ktDiag) {
          try {
            window.__ktPush({ b: BID, k: KEY, st: 'diag',
              d: 'HARVEST-DONE by=' + (doneByEvent ? 'STREAM-EVENT' : 'stability')
                 + ' polls=' + polls + ' stable=' + stable + ' needed=' + neededStable
                 + ' sawBusy=' + sawBusy + ' fast=' + FAST_DONE + ' len=' + txt.length
                 + ' elapsedMs=' + (Date.now() - t0)
                 + ' sinceLastChangeMs=' + (Date.now() - lastChangeAt)
                 + ' trace=' + trace.join(',') });
          } catch(e){}
        }
        // Diagnostic aid: a short "done" answer is exactly the shape a wrong selector produces
        // (some unrelated short UI label matched instead of a real reply, e.g. Grok's mode-toggle
        // pill briefly mistaken for the answer bubble) -- dump the matched element's own identity
        // BEFORE delivering, so a bad selector shows itself in the debug log instead of silently
        // reporting a fake "success". Real answers under 40 chars ("OK", "Ciao!") also trigger
        // this; that's fine, false positives here just cost a harmless log line.
        if (txt.length < 40) {
          try {
            var elDbg = getAnswerEl();
            var idl = elDbg ? (elDbg.tagName + '.' + (elDbg.className||'').toString().slice(0,120) + ' #' + (elDbg.id||'')) : 'NONE';
            var outer = elDbg ? (elDbg.outerHTML||'').slice(0,300) : '';
            // Direct children of the harvested element: used to NARROW the selector when provider UI
            // ends up inside it (Mistral delivered "OK\n\n1:16pm" plus "Was this helpful?/Skip",
            // because its data-message-author-role marks the whole message row). Without this list the
            // only alternative was guessing a class name.
            var kids = [];
            try {
              var chs = elDbg ? elDbg.children : [];
              for (var ki=0; ki<chs.length && ki<10; ki++){
                var ch = chs[ki];
                kids.push(ch.tagName + '.' + String(ch.className||'').slice(0,34)
                  + '("' + (ch.innerText||'').trim().replace(/\s+/g,' ').slice(0,22) + '")');
              }
            } catch(e){}
            // The harvested text can contain real newlines, which break the log line and cut off
            // everything after it: flatten them so the diagnostic arrives whole.
            var txtFlat = String(txt).replace(/\s+/g,' ');
            window.__ktPush({ b: BID, k: KEY, st: 'diag', d: ('SHORT-DONE txt="'+txtFlat+'" el='+idl+' kids=[' + kids.join(' | ') + '] html='+outer).slice(0,1400) });
          } catch(e){}
        }
        if (window.__ktThinkProbe) thinkCensus();
        deliver('done', txt, elToMd(getAnswerEl()));
        return;
      }
      if (Date.now() - t0 > 180000) { clearInterval(iv); deliver(txt ? 'timeout' : 'error', txt, txt ? elToMd(getAnswerEl()) : ''); return; }
      if (!sentCensus && polls === 15 && !txt) { sentCensus = true; census(); }
      if (polls % 3 === 0) { window.__ktPush({ b: BID, k: KEY, st: 'progress', len: txt.length }); }
    }
    stepNow = step;
    iv = setInterval(step, 1000);
    // The stream may already have closed while the answer was being detected: in that case there is
    // nothing to wait for, evaluate at once instead of losing a poll interval.
    if (streamEnded) step();
  }
})();
"##;

/// Two language-INDEPENDENT strategies for a provider's incognito/temporary chat:
///  - URL: incognito is addressable via a query param (Claude `/new?incognito=`) -> handled in the
///    frontend (`ktBaseUrlFor` sets it as the base when temp is enabled). PREFERRED: no DOM, no
///    click, no reload race, works in every UI language. Discover a provider's param from the URL
///    its own toggle produces (visible in the `page_finished` debug log).
///  - Click: only for providers with NO incognito URL -> a purpose-built in-page trigger returned
///    here, holding the fill (`__ktHoldFill`) until it activates.
/// Distinctive signature of a provider's incognito/private toggle ICON, captured via the INCOG-DUMP
/// probe. Matched language-INDEPENDENTLY by `temp_click_js`: either a substring of an inline
/// `<path d>` (Grok) or a substring of a sprite `<use href="#icon-id">` (Perplexity/Qwen) — both
/// are code constants, identical in every UI language.
const GROK_PRIVATE_SVG: &str = "5.562148571014404,-0.8140220046"; // ghost <path d>
const PERPLEXITY_INCOG_SVG: &str = "pplx-icon-spy";               // <use href="#pplx-icon-spy">
const QWEN_TEMP_SVG: &str = "icon-line-private-chat-01";          // <use href="#icon-line-private-chat-01">
const GEMINI_TEMP_SVG: &str = "gemini_chat_temp";                // <mat-icon data-mat-icon-name="gemini_chat_temp">
// Captured live 2026-08-10 (Playwright, real logged-in session) -- see
// docs/research/login-detection-providers.md for the per-provider incognito/temp-chat survey.
const POE_TEMP_SVG: &str = "M12.014 19.837a1 1 0 0 1 1.149";      // "Attiva chat temporanea" clock icon <path d>
const COPILOT_TEMP_SVG: &str = "M0.860549 14.0576";               // "Immetti chat temporanea" dashed-clock <path d>

fn temp_trigger_js(key: &str) -> Option<String> {
    match key {
        // anthropic (Claude): URL-based, see ktBaseUrlFor. The others have NO incognito URL, so we
        // click their toggle by its ICON. The frontend loads the compose page (drops ?q= where
        // present) when temp is on so the toggle can be clicked before fill+send.
        "grok" => Some(temp_click_js(GROK_PRIVATE_SVG)),         // "Passa alla chat privata" ghost
        "perplexity" => Some(temp_click_js(PERPLEXITY_INCOG_SVG)), // "Usa in incognito" spy icon
        // "qwen" is deliberately absent: its private-chat toggle can no longer be found (the discovery
        // probe reports `visMatches=0 :: (no control matched)`, 2026-08-18), so clicking did nothing
        // while the app kept promising anonymity. The frontend no longer offers it either -- see the
        // note on the qwen entry in PROVIDERS. Restore both together once the new control is captured.
        "gemini" => Some(temp_click_js(GEMINI_TEMP_SVG)),       // "Chat temporanea" mat-icon
        "poe" => Some(temp_click_js(POE_TEMP_SVG)),             // "Attiva chat temporanea" toggle
        "copilot" => Some(temp_click_js(COPILOT_TEMP_SVG)),     // "Immetti chat temporanea" toggle
        _ => None,
    }
}

/// Clicks a provider's incognito/temporary toggle, found by its ICON (`<path d>` prefix) —
/// language-INDEPENDENT: the icon is identical in every UI language, so we never touch the
/// localized aria-label. `.closest()` walks up to the clickable ancestor. Holds the fill until the
/// click lands (+ a safety release); if the click reloads the page, the resume script fills+sends
/// in the new (incognito) document. `svg` = a distinctive prefix of the toggle icon's path `d`,
/// captured via the INCOG-DUMP probe.
fn temp_click_js(svg: &str) -> String {
    format!(
        r##"(function(){{
  var SVG={svg};
  window.__ktHoldFill=true;
  var CLICKABLE='button,a,[role="button"],[role="menuitem"],[role="switch"],[role="menuitemcheckbox"]';
  function composer(){{ var s=['textarea:not([readonly]):not([aria-hidden="true"])','[contenteditable="true"]','div[role="textbox"]']; for(var i=0;i<s.length;i++){{var e=document.querySelectorAll(s[i]);for(var j=0;j<e.length;j++){{if(e[j].offsetParent!==null)return e[j];}}}} return null; }}
  function findCtl(){{
    // (a) inline icon: a <path d> that CONTAINS the signature (Grok's ghost).
    try{{ var ps=document.querySelectorAll('svg path[d*="'+SVG+'"]'); for(var k=0;k<ps.length;k++){{ var b=ps[k].closest(CLICKABLE); if(b&&b.offsetParent!==null) return b; }} }}catch(e){{}}
    // (b) sprite icon: a <use href="#icon-id"> whose id CONTAINS the signature (Perplexity/Qwen) —
    //     the sprite id is a code constant, identical in every UI language.
    var us=document.querySelectorAll('use');
    for(var i=0;i<us.length;i++){{ var h=(us[i].getAttribute('href')||us[i].getAttribute('xlink:href')||''); if(h.indexOf(SVG)>-1){{ var bb=us[i].closest(CLICKABLE); if(bb&&bb.offsetParent!==null) return bb; }} }}
    // (c) Material icon: a [data-mat-icon-name]/[fonticon]/[svgicon] CONTAINING the signature
    //     (Gemini's Angular <mat-icon>) — also a code constant, language-independent.
    var mis=document.querySelectorAll('[data-mat-icon-name],[fonticon],[svgicon]');
    for(var m=0;m<mis.length;m++){{ var nm=(mis[m].getAttribute('data-mat-icon-name')||mis[m].getAttribute('fonticon')||mis[m].getAttribute('svgicon')||''); if(nm.indexOf(SVG)>-1){{ var cc=mis[m].closest(CLICKABLE); if(cc&&cc.offsetParent!==null) return cc; }} }}
    return null;
  }}
  function diag(m){{ try{{ if(window.__ktDiag && window.__ktPush) window.__ktPush({{b:__kt_bid,k:__kt_key,st:'diag',d:m}}); }}catch(e){{}} }}
  var t0=Date.now();
  var iv=setInterval(function(){{
    if(!composer()){{ if(Date.now()-t0>12000){{ clearInterval(iv); window.__ktHoldFill=false; diag('TEMPCLICK-NOCOMPOSER'); }} return; }}   // wait hydration
    var ctl=findCtl();
    if(ctl){{ clearInterval(iv); try{{ ctl.click(); }}catch(e){{}} diag('TEMPCLICK-OK'); setTimeout(function(){{ window.__ktHoldFill=false; }},1500); return; }}
    if(Date.now()-t0>9000){{ clearInterval(iv); window.__ktHoldFill=false; diag('TEMPCLICK-NOTFOUND'); }}       // give up: fill anyway
  }},400);
}})();"##,
        svg = serde_json::to_string(svg).unwrap_or_else(|_| "\"\"".into()),
    )
}

/// LOG-ONLY probe for the providers' temporary/anonymous-chat toggles: inventories the
/// visible controls whose label/text mentions temporary/incognito/private and reports them
/// via the diag sentinel. No clicks: real per-provider toggle selectors get codified from
/// these logs (explore live, then codify). Runs only on FRESH injections.
const TEMP_PROBE_JS: &str = r##"
(function(){
  var RX = /incognito|incógnito|privat|priv[eéèo]|tempora|ephemeral|secret|segret|anonym|ghost|инкогнито|приват|временн|секрет|シークレット|秘密|一時|匿名|隐身|無痕|无痕|临时|臨時|私密|비공개|시크릿|익명|임시|خاص|مؤقت|سري|गुप्त|अस्थायी/i;
  function labelOf(e){ return (e.getAttribute&&(e.getAttribute('aria-label')||'')+' '+(e.getAttribute('title')||'')||'')+' '+((e.textContent||'').slice(0,40)); }
  function dump(tag){
    try {
      // 1) candidate incognito CONTROLS: full outerHTML (incl. SVG path -> language-neutral icon
      //    signal) + pressed/checked state, so we can codify an icon/attribute selector.
      var ctls = document.querySelectorAll('button,a,[role="button"],[role="menuitem"],[role="switch"],[role="menuitemcheckbox"]');
      var hits = [];
      for (var i=0;i<ctls.length && hits.length<3;i++){
        var e = ctls[i];
        if (!RX.test(labelOf(e))) continue;
        var st = (e.getAttribute('aria-pressed')||e.getAttribute('aria-checked')||'')+ (e.offsetParent===null?'/HID':'/vis');
        var svg=e.querySelector('svg'); var pth=svg?svg.querySelector('path'):null;
        var dsig;
        if(pth){ dsig='path:'+(pth.getAttribute('d')||'').slice(0,80); }
        else if(svg){ dsig='svg:'+String(svg.outerHTML||'').replace(/\s+/g,' ').slice(0,170); }
        else {
          var mi=e.querySelector('mat-icon,[data-mat-icon-name],[fonticon]');
          if(mi){ dsig='mat name="'+(mi.getAttribute('data-mat-icon-name')||mi.getAttribute('fonticon')||mi.getAttribute('svgicon')||'')+'" text="'+(mi.textContent||'').trim().slice(0,24)+'"'; }
          else { dsig='html:'+String(e.innerHTML||'').replace(/\s+/g,' ').slice(0,220); }
        }
        var tid = e.getAttribute('data-testid')||'-';
        hits.push('['+st+'] testid='+tid+' aria="'+(e.getAttribute('aria-label')||'').slice(0,28)+'" '+dsig);
      }
      // 2) incognito STATE indicator (language-neutral): any element flagged pressed/checked AND
      //    matching the stems, or the count of visible matches (drops to ~0 once toggled in-place).
      var vis=0, pressed=0;
      for (var j=0;j<ctls.length;j++){ var c=ctls[j]; if(!RX.test(labelOf(c))) continue; if(c.offsetParent!==null) vis++; if((c.getAttribute('aria-pressed')==='true')||(c.getAttribute('aria-checked')==='true')) pressed++; }
      // 3) TOP-BAR icon buttons (candidate ghost/private toggles WITHOUT an aria-label): dump each
      //    small header icon's left-x + label + svg-path prefix, to spot the toggle by its icon.
      var icons=[];
      for (var t=0;t<ctls.length && icons.length<12;t++){
        var b=ctls[t]; if(b.offsetParent===null) continue;
        var r=b.getBoundingClientRect(); if(r.top>150 || r.width>76 || r.width<14) continue;
        var sp=b.querySelector('svg path'); if(!sp) continue;
        icons.push((r.left|0)+':'+(b.getAttribute('aria-label')||'').slice(0,16)+':'+(sp.getAttribute('d')||'').slice(0,40));
      }
      var msg = 'INCOG['+tag+'] url='+location.pathname+' visMatches='+vis+' pressed='+pressed+' :: '+(hits.length?hits.join('  ||  '):'(no control matched)')+' :: TOPICONS '+icons.join(' | ');
      // Deliver over IPC when the page has the bridge, and navigate ONLY as a fallback. Navigating
      // is not free: it TEARS DOWN the very page being probed. Measured on Qwen -- the two probe
      // dumps navigated the tab away mid-answer, the app came back on the site's root (a brand new
      // empty chat), and the harvester then searched an empty page for three minutes and reported a
      // failure. The conversation and its answer were fine; the diagnostic had destroyed the thing
      // it was diagnosing. Any probe added here must obey the same rule.
      var d = msg.slice(0,1400);
      if (window.__ktPush) { window.__ktPush({ b: __kt_bid, k: __kt_key, st: 'diag', d: d }); return; }
      window.location.href = 'https://kotodama.result/?b='+encodeURIComponent(__kt_bid)+'&k='+encodeURIComponent(__kt_key)+'&st=diag&d='+encodeURIComponent(d);
    } catch(err){}
  }
  try { setTimeout(function(){ dump('pre'); }, 2500); } catch(e){}
  try { setTimeout(function(){ dump('post'); }, 9000); } catch(e){}
})();
"##;

/// Full injection script for one provider: (optional temp-chat toggle click) + fill+send
/// (browser.rs) + harvester. `fresh` = new conversation (no previous answer to snapshot);
/// `temp` = the user wants provider temporary chats (kt_temp_chats).
fn build_inject_js(broadcast_id: &str, key: &str, text: &str, fresh: bool, temp: bool) -> Result<String, String> {
    let (ans, busy) = selectors_for(key);
    let prelude = format!(
        "var __kt_bid = {}; var __kt_key = {}; var __kt_ans = {}; var __kt_busy = {}; var __kt_fresh = {fresh}; var __kt_fast = {fast}; var __kt_sent = false; window.__ktDiag = {diag}; window.__ktStreamEver = 0;",
        serde_json::to_string(broadcast_id).map_err(|e| e.to_string())?,
        serde_json::to_string(key).map_err(|e| e.to_string())?,
        serde_json::to_string(ans).map_err(|e| e.to_string())?,
        serde_json::to_string(busy).map_err(|e| e.to_string())?,
        fast = fast_done_for(key),
        diag = crate::debug::enabled(),
    );
    // incognito/temporary trigger (holds the fill until done), only on fresh turns of providers
    // that have an in-page trigger AND the user enabled it for this provider.
    let temp_part = if fresh && temp { temp_trigger_js(key).unwrap_or_default() } else { String::new() };
    // The INCOG diagnostic probe only runs under KOTODAMA_DEBUG (used to discover a provider's
    // incognito URL/selector); never in production.
    let probe = if fresh && crate::debug::enabled() { TEMP_PROBE_JS } else { "" };
    // Reasoning discovery probe: only sets a flag; the census itself runs at delivery time.
    let think = if crate::debug::enabled() && std::env::var("KOTO_THINKPROBE").is_ok() {
        "window.__ktThinkProbe = true;"
    } else {
        ""
    };
    // Network discovery probe: BEFORE the fill, or the send request itself is missed.
    let net = if crate::debug::enabled() && std::env::var("KOTO_NETPROBE").is_ok() {
        NET_PROBE_JS
    } else {
        ""
    };
    // STREAM_WATCH_JS goes BEFORE the fill: it has to be in place before the send opens the answer's
    // stream, otherwise the one request that matters is the one it misses.
    Ok(prelude
        + think
        + PUSH_HELPER_JS
        + SR_HIDE_JS
        + net
        + STREAM_WATCH_JS
        + &temp_part
        + &browser::fill_js(text, true)?
        + HARVEST_JS
        + probe)
}

/// Resume script for a page that navigated mid-broadcast. Two cases, decided IN PAGE:
/// - the sent text is visible in the DOM -> the send happened, only harvest (never re-send:
///   a duplicate would double-post on ChatGPT-style redirects);
/// - the sent text is NOT in the DOM -> the original injection died before sending (Qwen/Z.ai
///   landing pages navigate right after load), so fill+send first, then harvest.
fn build_resume_js(
    broadcast_id: &str,
    key: &str,
    text: &str,
    allow_send: bool,
) -> Result<String, String> {
    let (ans, busy) = selectors_for(key);
    // Whitespace-collapsed head of the message for a robust "is it on the page?" check.
    let head: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect();
    let prelude = format!(
        // `window.__ktDiag` must be set HERE too: without it, all the fill-loop diagnostics stayed
        // silent in exactly the path where they are needed -- the resume after a navigation (providers
        // whose temporary chat is a click DO navigate).
        "var __apb_text = {}; var __kt_head = {}; var __apb_send = true; var __kt_bid = {}; var __kt_key = {}; var __kt_ans = {}; var __kt_busy = {}; var __kt_fresh = true; var __kt_fast = {fast}; var __kt_sent = {sent}; window.__ktDiag = {diag}; window.__ktStreamEver = 0;",
        serde_json::to_string(text).map_err(|e| e.to_string())?,
        serde_json::to_string(&head).map_err(|e| e.to_string())?,
        serde_json::to_string(broadcast_id).map_err(|e| e.to_string())?,
        serde_json::to_string(key).map_err(|e| e.to_string())?,
        serde_json::to_string(ans).map_err(|e| e.to_string())?,
        serde_json::to_string(busy).map_err(|e| e.to_string())?,
        fast = fast_done_for(key),
        // The send is a FACT Rust holds (sent_marks), not something to re-derive from the page: the
        // harvester must never declare "never sent" about a message it knows went out.
        sent = !allow_send,
        diag = crate::debug::enabled(),
    );
    // `allow_send` is decided by Rust from `sent_marks`, NOT by reading the page:
    //  - send not out yet -> inject the fill (the case this resume exists for: pages that navigate
    //    right after loading, killing the script before it sends);
    //  - send ALREADY out -> harvest ONLY. This is where the mess was: the new page does not contain
    //    the message, so every DOM-based heuristic concludes "not sent" and sends again -- and the
    //    second copy, starting from the root URL, even opened a NEW CONVERSATION (and a non-anonymous
    //    one, because the temporary-chat route only applies to the first send).
    if !allow_send {
        // Already sent before the navigation: only harvest. The stream watcher still goes in -- the
        // answer's stream may well be opened by the NEW document, and its close is what we are after.
        // `__ktSentAt` is set here by hand: in this fresh JS context no fill script will set it, but
        // Rust already knows the send went out, so the watcher must be armed from the start.
        return Ok(prelude
            + PUSH_HELPER_JS
            + SR_HIDE_JS
            + "try { window.__ktSentAt = Date.now(); } catch(e){}\n"
            + STREAM_WATCH_JS
            + HARVEST_JS);
    }
    let fill = browser::fill_js(text, true)?;
    Ok(prelude + PUSH_HELPER_JS + SR_HIDE_JS + &fill + HARVEST_JS)
}

/// Marks (bid, key) answered: removes it from the broadcast, emits `app://kotodama-answer`
/// and, when the broadcast empties, `app://kotodama-finished`. Duplicate calls are no-ops.
fn finish_key(window: &Window, bid: &str, key: &str, status: &str, text: &str, truncated: bool, md: &str) {
    // Total wall-clock from the broadcast being registered to the answer being handed to the UI. Read
    // together with HARVEST-DONE's `sinceLastChangeMs` it splits the wait into "the model was still
    // writing" and "we were still deciding it had finished" -- the second is the only part we control.
    let total_ms = broadcasts()
        .lock()
        .unwrap()
        .get(bid)
        .map(|bc| bc.started.elapsed().as_millis());
    let (emit_it, all_done) = {
        let mut b = broadcasts().lock().unwrap();
        match b.get_mut(bid) {
            Some(bc) => {
                let removed = bc.pending.remove(key);
                let empty = bc.pending.is_empty();
                if empty {
                    b.remove(bid);
                }
                (removed, empty)
            }
            None => (false, false),
        }
    };
    if !emit_it {
        return;
    }
    // Turn over: drop the "already sent" mark too, otherwise the next message to the same provider
    // would find the previous turn's mark. The key is (bid, key), so in practice it does not collide,
    // but leaving it around is a leak that serves nobody.
    sent_marks().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
    // Answer delivered: stop resuming this key's harvest on future page loads (only if the
    // registered harvest belongs to THIS broadcast — a newer one must keep its entry).
    {
        let mut ah = active_harvests().lock().unwrap();
        if ah.get(key).map(|(b, _)| b == bid).unwrap_or(false) {
            ah.remove(key);
        }
    }
    debug::log(format!(
        "kotodama answer bid={bid} key={key} status={status} len={} totalMs={} preview={:?}",
        text.len(),
        total_ms.map(|m| m.to_string()).unwrap_or_else(|| "?".into()),
        text.chars().take(160).collect::<String>()
    ));
    if status == "login" {
        // A real send hit a login wall: this is the strongest, most direct signal that the
        // provider is no longer authenticated (much more common in practice than the passive
        // probe catching it) -- demote it out of known_providers so "chiedi a tutti" stops
        // offering/pre-selecting it until a real login (and a successful answer) restores it.
        crate::set_provider_known(&window.app_handle(), key, false);
    }
    let _ = window.emit(
        "app://kotodama-answer",
        serde_json::json!({ "broadcastId": bid, "key": key, "status": status, "text": text, "truncated": truncated, "md": md }),
    );
    if all_done {
        let _ = window.emit("app://kotodama-finished", serde_json::json!({ "broadcastId": bid }));
    }
}

/// Sentinel handler, called from `create_tab`'s `on_navigation` for `kotodama.result` URLs. This
/// is the FALLBACK delivery path (see `kotodama_push` for the primary, direct-IPC one): parses the
/// query string into the same structured message and hands off to `handle_push`.
pub fn on_result_url(window: &Window, u: &Url) {
    let mut bid = None;
    let mut key = None;
    let mut st = None;
    let mut seq: Option<usize> = None;
    let mut total: Option<usize> = None;
    let mut data = None;
    let mut len: Option<usize> = None;
    let mut trunc = false;
    for (k, v) in u.query_pairs() {
        match k.as_ref() {
            "b" => bid = Some(v.into_owned()),
            "k" => key = Some(v.into_owned()),
            "st" => st = Some(v.into_owned()),
            "s" => seq = v.parse().ok(),
            "n" => total = v.parse().ok(),
            "d" => data = Some(v.into_owned()),
            "len" => len = v.parse().ok(),
            "tr" => trunc = v.as_ref() == "1",
            _ => {}
        }
    }
    let (Some(bid), Some(key), Some(st)) = (bid, key, st) else { return };
    // Fallback path only: no `md` (see ChunkBuf doc comment) -- graceful degradation to plain
    // text on the rare pages where the direct-IPC path isn't available.
    handle_push(window, bid, key, st, seq, total, data, len, trunc, None);
}

/// Direct-IPC delivery from the provider webview (`window.__TAURI__.core.invoke('kotodama_push',
/// ...)`), preferred by the injected script's `__ktPush` helper whenever the Tauri bridge is
/// available in that page. Same wire shape as the navigation-sentinel fallback, just as real
/// command args instead of URL query params — no chunking/coalescing constraints, so the injected
/// script sends the WHOLE answer in one call instead of spaced-out 1500-char pieces.
#[tauri::command]
pub fn kotodama_push(
    window: Window,
    b: String,
    k: String,
    st: String,
    s: Option<usize>,
    n: Option<usize>,
    d: Option<String>,
    len: Option<usize>,
    tr: Option<bool>,
    md: Option<String>,
) {
    if crate::debug::enabled() && (st == "diag" || s == Some(0)) {
        // Log-once-per-delivery confirmation that the direct-IPC path is actually being used (vs.
        // the navigation-sentinel fallback) — useful to know per-provider if this ever needs
        // diagnosing (e.g. a provider whose CSP blocks the Tauri bridge would silently fall back).
        debug::log(format!("kotodama_push (IPC) key={k} st={st}"));
    }
    handle_push(&window, b, k, st, s, n, d, len, tr.unwrap_or(false), md);
}

/// Shared core for both delivery paths: diag/progress heartbeats emit straight away; chunked
/// payloads (`seq`/`total`) accumulate in `chunk_bufs()` until complete, then finish the turn.
fn handle_push(
    window: &Window,
    bid: String,
    key: String,
    st: String,
    seq: Option<usize>,
    total: Option<usize>,
    data: Option<String>,
    len: Option<usize>,
    trunc: bool,
    md: Option<String>,
) {
    if st == "diag" {
        // DOM census from a stuck harvest: log-only, this is how provider selectors get tuned.
        debug::log(format!("kotodama DIAG key={key}: {}", data.unwrap_or_default()));
        return;
    }
    // The fill loop announces that it pressed Enter. From here on NOBODY may send the same message
    // again, not even if the page navigates and the script is re-injected.
    if st == "sent" {
        debug::log(format!("kotodama SENT key={key} bid={bid} -- no further send allowed"));
        sent_marks().lock().unwrap().insert((bid, key));
        return;
    }
    if st == "progress" {
        let _ = window.emit(
            "app://kotodama-progress",
            serde_json::json!({ "broadcastId": bid, "key": key, "len": len.unwrap_or(0) }),
        );
        return;
    }
    let (Some(seq), Some(total)) = (seq, total) else { return };
    if total == 0 || total > 200 || seq >= total {
        return; // malformed
    }
    let done = {
        let mut bufs = chunk_bufs().lock().unwrap();
        let buf = bufs.entry((bid.clone(), key.clone())).or_insert_with(|| ChunkBuf {
            parts: vec![None; total],
            status: st.clone(),
            trunc,
            md: String::new(),
        });
        if buf.parts.len() != total {
            buf.parts = vec![None; total]; // total changed: superseded delivery, restart buffer
            buf.status = st.clone();
        }
        buf.parts[seq] = Some(data.unwrap_or_default());
        if trunc {
            buf.trunc = true;
        }
        if let Some(md) = md {
            buf.md = md; // only ever sent whole (direct-IPC path), see ChunkBuf doc comment
        }
        if buf.parts.iter().all(|p| p.is_some()) {
            let text: String = buf.parts.iter().map(|p| p.as_deref().unwrap_or("")).collect();
            let status = buf.status.clone();
            let tr = buf.trunc;
            let md = buf.md.clone();
            bufs.remove(&(bid.clone(), key.clone()));
            Some((text, status, tr, md))
        } else {
            None
        }
    };
    if let Some((text, status, tr, md)) = done {
        finish_key(window, &bid, &key, &status, &text, tr, &md);
    }
}

/// Passive, on-demand login-wall probe for a provider page that is NOT part of any active
/// Kotodama send (a manually opened tab, or an idle tab between broadcast turns). Reports via
/// `provider_login_probe` ONLY on an unambiguous read: a password field or captcha wall found =
/// needs login; a chat composer found = does not -- neither found (still loading) stays silent
/// rather than risk a false auto-show/auto-park. After a first "needs login" report it keeps
/// polling (budget ~10 min) so an in-page login (no navigation, e.g. a modal) is still caught --
/// most providers DO navigate/reload after login, which fires a fresh `Finished` event and a
/// fresh probe anyway, but this covers the ones that don't. Retreats immediately if a real
/// send/harvest (`window.__ktBid`) starts on this same page: the reactive auth-wall check inside
/// `HARVEST_JS` (see `deliver`) already owns login detection for that case.
fn login_probe_js(key: &str) -> String {
    format!(
        r##"(function(){{
  var KEY = {key};
  var BUDGET_MS = 600000, STEP_MS = 4000, t0 = Date.now(), reportedLogin = false;
  // Password field OR captcha challenge (see HARVEST_JS's authWallPresent for why both count).
  function loginUrlRedirected(){{
    try {{
      var p = location.pathname || '';
      if (KEY === 'anthropic' && p.indexOf('/login') !== -1) return true;
      if (KEY === 'deepseek' && p.indexOf('/sign_in') !== -1) return true;
      if (KEY === 'poe' && p.indexOf('/login') !== -1) return true;
    }} catch(e){{}}
    return false;
  }}
  function authWallPresent(){{
    try {{
      if (loginUrlRedirected()) return true;
      if (document.querySelector('input[type="password"]')) return true;
      if (document.querySelector('[class*="captcha" i], [id*="captcha" i], [data-testid*="captcha" i], iframe[src*="captcha" i], iframe[src*="turnstile" i]')) return true;
    }} catch(e){{}}
    return false;
  }}
  function composerPresent(){{
    var sels = ['textarea:not([readonly]):not([aria-hidden="true"])', '[contenteditable="true"]', 'div[role="textbox"]'];
    for (var i=0;i<sels.length;i++){{
      var els = document.querySelectorAll(sels[i]);
      for (var j=0;j<els.length;j++){{ if (els[j].offsetParent !== null) return true; }}
    }}
    return false;
  }}
  function report(needsLogin){{
    try {{
      if (window.__TAURI__ && window.__TAURI__.core && typeof window.__TAURI__.core.invoke === 'function') {{
        window.__TAURI__.core.invoke('provider_login_probe', {{ key: KEY, needsLogin: needsLogin }}).catch(function(){{}});
      }}
    }} catch(e){{}}
  }}
  function tick(){{
    if (window.__ktBid) {{ clearInterval(iv); return; }}   // a real send/harvest took over this page
    if (composerPresent()) {{
      clearInterval(iv);
      if (reportedLogin) report(false);   // was flagged needing login earlier -> now resolved
      return;
    }}
    if (authWallPresent()) {{
      if (!reportedLogin) {{ reportedLogin = true; report(true); }}
      return;   // keep polling: only the LATER composer-appears transition is still of interest
    }}
    if (Date.now() - t0 > BUDGET_MS) clearInterval(iv);   // ambiguous the whole time: give up silently
  }}
  var iv = setInterval(tick, STEP_MS);
  setTimeout(tick, 3500);
}})();"##,
        key = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into()),
    )
}

/// WARM TABS: get the given providers' tabs onto an empty new conversation NOW, in the background,
/// so the next send finds a loaded page and skips the page load entirely (measured on ChatGPT: a
/// second fresh conversation went from 6.8s to 3.1s).
///
/// Called by the frontend on a USER event -- the first keystroke of the next message, or "new
/// conversation" -- deliberately NOT right after an answer arrives. Pre-warming on delivery would
/// navigate the tab away from the conversation that was just answered, and opening the provider's tab
/// to read or continue it there is a normal thing to do: the speed is not worth taking that away.
/// Waiting for the user to start writing costs nothing, because the page loads while they type.
///
/// Refusals, all silent: a provider with no recorded fresh URL (never sent to yet, or its temporary
/// chat is a click-toggle -- see `kotodama_broadcast`), a tab the user is currently looking at, and a
/// provider with a send still in flight.
#[tauri::command]
pub fn kotodama_prewarm(window: Window, keys: Vec<String>) {
    // A provider still OWED an answer must never be pre-warmed: navigating its tab throws away the
    // answer that is on its way. `provider_busy` alone was not enough -- it only knows about the
    // injection and harvest bookkeeping, and there are moments in between where a provider is still
    // expected to answer while looking idle. The authoritative list is the broadcasts' pending sets.
    // Measured: a 10-provider run where pre-warming a tab mid-harvest turned three working providers
    // (ChatGPT, DeepSeek, Mistral) into `sendfail`.
    let awaited: HashSet<String> = broadcasts()
        .lock()
        .unwrap()
        .values()
        .flat_map(|bc| bc.pending.iter().cloned())
        .collect();
    for key in keys {
        if browser::foreground_key().as_deref() == Some(key.as_str())
            || provider_busy(&key)
            || awaited.contains(&key)
        {
            continue;
        }
        if prewarmed().lock().unwrap().contains_key(&key) {
            continue; // already sitting on a fresh page
        }
        let Some(url) = fresh_bases().lock().unwrap().get(&key).cloned() else {
            continue;
        };
        let Some(wv) = window.get_webview(&browser::provider_label(&key)) else {
            continue;
        };
        match url.parse::<Url>() {
            Ok(parsed) => {
                if wv.navigate(parsed).is_ok() {
                    debug::log(format!("kotodama prewarm START key={key} -> {}", &url[..url.len().min(90)]));
                    // Not ready yet: `on_page_finished` promotes it once the page has actually loaded.
                    prewarming().lock().unwrap().insert(key);
                }
            }
            Err(e) => debug::log(format!("kotodama prewarm key={key} bad url: {e}")),
        }
    }
}

/// Report from `login_probe_js` (a passive, on-demand check -- the reactive password check inside
/// `HARVEST_JS` has its own path via `finish_key`'s `status=="login"` branch, not this command).
/// `needs_login=true` demotes the provider out of `known_providers` (a stale "known" flag is
/// exactly how a logged-out provider kept showing up as available in "chiedi a tutti") -- it does
/// NOT bring the tab on screen: with more than one provider possibly needing login at the same
/// time, auto-showing would fight over the single visible-tab slot and could pop a page in front
/// of the user unprompted. The user instead resolves it explicitly, one at a time, via the
/// "Accedi" button the frontend shows on that provider's card. `needs_login=false` is a no-op
/// here (it does NOT re-promote to known); that only happens on an actual successful answer
/// (`mark_provider_known`), a much stronger signal than "a composer is visible".
#[tauri::command]
pub fn provider_login_probe(window: Window, key: String, needs_login: bool) {
    if crate::debug::enabled() {
        debug::log(format!("provider_login_probe key={key} needs_login={needs_login}"));
    }
    if needs_login {
        crate::set_provider_known(&window.app_handle(), &key, false);
    }
}

/// Page finished loading: if this provider has a pending injection, run it now.
/// The fill script itself polls ~20s for the composer, so SPA hydration after
/// `Finished` is already tolerated — no extra retry needed here.
pub fn on_page_finished<R: Runtime>(webview: &tauri::Webview<R>, key: &str) {
    if crate::debug::enabled() {
        let u = webview.url().map(|u| u.to_string()).unwrap_or_default();
        debug::log(format!("kotodama page_finished key={key} url={u}"));
        // DISCOVERY: KOTO_AUTOPROBE=<key[,key...]> injects ONLY the INCOG probe on each listed
        // provider's compose page (no fill/send) so we can read its incognito toggle icon/URL.
        if std::env::var("KOTO_AUTOPROBE").ok().map(|v| v.split(',').any(|k| k.trim() == key)).unwrap_or(false) {
            let prelude = format!("var __kt_bid={}; var __kt_key={};",
                serde_json::to_string("probe").unwrap(), serde_json::to_string(key).unwrap());
            // PUSH_HELPER_JS first: without it the probe has no IPC and falls back to navigation,
            // which is exactly what wrecks the page under examination.
            let _ = webview.eval(&(prelude + PUSH_HELPER_JS + TEMP_PROBE_JS));
            return;
        }
    }
    let inj = pending_injections().lock().unwrap().remove(key);
    if let Some(inj) = inj {
        debug::log(format!("kotodama inject (on load) key={key} bid={}", inj.broadcast_id));
        match build_inject_js(&inj.broadcast_id, key, &inj.text, inj.fresh, inj.temp) {
            Ok(js) => {
                let _ = webview.eval(&js);
                active_harvests()
                    .lock()
                    .unwrap()
                    .insert(key.to_string(), (inj.broadcast_id.clone(), inj.text.clone()));
            }
            Err(e) => debug::log(format!("kotodama inject build error: {e}")),
        }
        return;
    }
    // No queued injection: if a harvest is still owed for this key, the page must have
    // NAVIGATED after the send (Qwen landing -> chat, ChatGPT /?q= -> /c/<id>), killing the
    // injected script. Resume with a harvest-only script in the new document.
    let resume = active_harvests().lock().unwrap().get(key).cloned();
    if let Some((bid, text)) = resume {
        let still_pending = broadcasts()
            .lock()
            .unwrap()
            .get(&bid)
            .map(|bc| bc.pending.contains(key))
            .unwrap_or(false);
        if still_pending {
            let allow_send = !already_sent(&bid, key);
            debug::log(format!(
                "kotodama RESUME after nav key={key} bid={bid} resend={}",
                if allow_send { "YES (never went out)" } else { "NO (already sent)" }
            ));
            if let Ok(js) = build_resume_js(&bid, key, &text, allow_send) {
                let _ = webview.eval(&js);
            }
            return;
        }
    }
    // A pre-warm navigation has just finished: NOW the tab is a warm tab, and the next send can type
    // straight into it. Promoted here rather than when the navigation was requested, because a page
    // that is still loading is not ready -- see `prewarming`.
    if prewarming().lock().unwrap().remove(key) {
        let here = webview.url().map(|u| u.to_string()).unwrap_or_default();
        if !here.is_empty() {
            debug::log(format!("kotodama prewarm READY key={key}"));
            prewarmed().lock().unwrap().insert(key.to_string(), here);
        }
    }
    // Neither a queued injection nor an owed harvest resume: this page-load is not part of any
    // in-flight Kotodama send (a manually opened tab, or an idle tab between broadcast turns) --
    // passively probe whether it needs login, so the app can auto-show it without requiring an
    // actual send attempt first.
    let _ = webview.eval(&login_probe_js(key));
}

/// Broadcast `text` to the given provider tabs WITHOUT showing them.
/// `new_chat=true` (re)navigates each tab to its base URL first (fresh conversation);
/// `false` injects into the page as-is (follow-up turn, keeps the provider context).
/// Calling twice with the same `broadcast_id` MERGES keys (the UI splits fresh/warm tabs).
/// Async: creating a WebView2 webview in a sync command deadlocks on Windows.
#[tauri::command]
pub async fn kotodama_broadcast(
    window: Window,
    broadcast_id: String,
    text: String,
    keys: Vec<String>,
    new_chat: bool,
    bases: HashMap<String, String>,
) -> Result<(), String> {
    debug::log(format!("kotodama_broadcast bid={broadcast_id} keys={keys:?} new_chat={new_chat}"));
    // temporary provider chats: global switch + per-provider map (kt_temp_providers). A provider
    // gets the incognito trigger only if the global switch is on AND its per-provider entry is
    // not explicitly false. Snapshot the map so we can gate each key below.
    let temp_state = window.state::<crate::AppState>();
    let (temp_global, temp_map) = {
        let g = temp_state.settings.lock().unwrap();
        (g.kt_temp_chats, g.kt_temp_providers.clone())
    };
    let temp_for = |k: &str| temp_global && temp_map.get(k).copied().unwrap_or(true);
    // Register/merge the broadcast BEFORE any answer can arrive.
    {
        let mut b = broadcasts().lock().unwrap();
        let bc = b
            .entry(broadcast_id.clone())
            .or_insert_with(|| Broadcast { pending: HashSet::new(), started: Instant::now() });
        for k in &keys {
            bc.pending.insert(k.clone());
        }
    }
    for key in &keys {
        // One in-flight harvest per provider: kill any previous one (different bid).
        {
            let other_bids: Vec<String> = broadcasts()
                .lock()
                .unwrap()
                .iter()
                .filter(|(bid, bc)| *bid != &broadcast_id && bc.pending.contains(key))
                .map(|(bid, _)| bid.clone())
                .collect();
            if !other_bids.is_empty() {
                pending_injections().lock().unwrap().remove(key);
                chunk_bufs().lock().unwrap().retain(|(_, k), _| k != key);
                for bid in other_bids {
                    finish_key(&window, &bid, key, "error", "", false, "");
                }
            }
        }
        // Instant card in the UI.
        let _ = window.emit(
            "app://kotodama-answer",
            serde_json::json!({ "broadcastId": broadcast_id, "key": key, "status": "pending", "text": "" }),
        );
        let Some(base) = bases.get(key) else {
            finish_key(&window, &broadcast_id, key, "error", "", false, "");
            continue;
        };
        // Unico imbuto di TUTTI gli invii (broadcast, ritenta, inline transform): risvegliare qui
        // copre ogni percorso. Un webview congelato non eseguirebbe lo script di fill iniettato.
        browser::resume_provider(&window, key, true);
        let label = browser::provider_label(key);
        let existing = window.get_webview(&label);
        if let (Some(webview), false) = (&existing, new_chat) {
            // Warm follow-up: inject straight into the loaded page (keeps the conversation).
            match build_inject_js(&broadcast_id, key, &text, false, false) {
                Ok(js) => {
                    debug::log(format!("kotodama inject (warm) key={key}"));
                    if webview.eval(&js).is_err() {
                        finish_key(&window, &broadcast_id, key, "error", "", false, "");
                    } else {
                        active_harvests()
                            .lock()
                            .unwrap()
                            .insert(key.clone(), (broadcast_id.clone(), text.clone()));
                    }
                }
                Err(_) => finish_key(&window, &broadcast_id, key, "error", "", false, ""),
            }
            continue;
        }
        // Fresh conversation. Remember the URL WITHOUT the message: it is what a pre-warm has to
        // navigate to later, and it is the only place we get to know it (the provider URL rules live
        // in the frontend).
        //
        // NOT recorded -- i.e. no pre-warming -- for providers whose temporary chat is a CLICK on an
        // in-page toggle rather than a URL parameter: a reloaded page comes back in its normal state,
        // and if the provider happened to remember the private mode, clicking the toggle again would
        // switch it OFF. Anonymity must never be lost to a speed optimisation, so those keep loading
        // the way they do today. Providers whose temporary chat lives in the URL (ChatGPT, Claude)
        // carry it in the stripped base and are safe.
        let click_temp = temp_for(key) && temp_trigger_js(key).is_some();
        if !click_temp {
            if let Some(stripped) = strip_prompt_params(base) {
                fresh_bases().lock().unwrap().insert(key.clone(), stripped);
            }
        } else {
            fresh_bases().lock().unwrap().remove(key);
        }
        // A send supersedes any pre-warm still in flight: drop the pending state, or the page-load it
        // is about to finish would be mistaken for a ready warm tab while we are navigating elsewhere.
        prewarming().lock().unwrap().remove(key);
        // WARM TAB SHORTCUT: this provider is already sitting on an empty new conversation, so there
        // is nothing to load -- type into it instead, exactly like a follow-up. This is where the ~4s
        // page load leaves the user's waiting time. The URL is re-checked first: if the user browsed
        // elsewhere in that tab, or the temporary-chat state no longer matches, we navigate as usual.
        let prewarm_url = prewarmed().lock().unwrap().get(key).cloned();
        if let (Some(webview), Some(expected)) = (&existing, prewarm_url) {
            let here = webview.url().map(|u| u.to_string()).unwrap_or_default();
            if prewarm_still_valid(&here, &expected) {
                prewarmed().lock().unwrap().remove(key);
                // `fresh = true`: the conversation IS new, so no previous answer to snapshot. `temp`
                // is false because the temporary-chat state is already in place from the pre-warm --
                // clicking its toggle again would switch it back OFF.
                match build_inject_js(&broadcast_id, key, &text, true, false) {
                    Ok(js) => {
                        debug::log(format!("kotodama inject (warm tab, no page load) key={key}"));
                        if webview.eval(&js).is_err() {
                            finish_key(&window, &broadcast_id, key, "error", "", false, "");
                        } else {
                            active_harvests()
                                .lock()
                                .unwrap()
                                .insert(key.clone(), (broadcast_id.clone(), text.clone()));
                        }
                    }
                    Err(_) => finish_key(&window, &broadcast_id, key, "error", "", false, ""),
                }
                continue;
            }
            debug::log(format!("kotodama prewarm stale key={key} here={} ", &here[..here.len().min(90)]));
            prewarmed().lock().unwrap().remove(key);
        }
        // Fresh conversation: navigate (or create parked) and inject once loaded.
        pending_injections().lock().unwrap().insert(
            key.clone(),
            PendingInjection { broadcast_id: broadcast_id.clone(), text: text.clone(), fresh: true, temp: temp_for(key) },
        );
        debug::log(format!("fresh key={key} existing={} url={}", existing.is_some(), &base[..base.len().min(180)]));
        let parsed = match base.parse::<Url>() {
            Ok(u) => u,
            Err(e) => {
                debug::log(format!("fresh key={key} URL PARSE ERROR: {e}"));
                pending_injections().lock().unwrap().remove(key);
                finish_key(&window, &broadcast_id, key, "error", "", false, "");
                continue;
            }
        };
        let created_ok = if let Some(webview) = existing {
            webview.navigate(parsed).is_ok()
        } else {
            match browser::provider_bounds(&window) {
                Ok((w, h)) => browser::create_tab(&window, key, parsed, w, h).is_ok(),
                Err(e) => { debug::log(format!("fresh key={key} bounds ERROR: {e}")); false }
            }
        };
        debug::log(format!("fresh key={key} created_ok={created_ok}"));
        if !created_ok {
            pending_injections().lock().unwrap().remove(key);
            finish_key(&window, &broadcast_id, key, "error", "", false, "");
            continue;
        }
        // Fallback: if `Finished` never fires (cached page/redirect), inject anyway after 8s.
        {
            let win = window.clone();
            let key = key.clone();
            let bid = broadcast_id.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(8));
                let inj = {
                    let mut p = pending_injections().lock().unwrap();
                    match p.get(&key) {
                        Some(pi) if pi.broadcast_id == bid => p.remove(&key),
                        _ => None,
                    }
                };
                if let Some(inj) = inj {
                    debug::log(format!("kotodama inject (fallback 8s) key={key}"));
                    if let (Some(webview), Ok(js)) = (
                        win.get_webview(&browser::provider_label(&key)),
                        build_inject_js(&inj.broadcast_id, &key, &inj.text, inj.fresh, inj.temp),
                    ) {
                        let _ = webview.eval(&js);
                        active_harvests()
                            .lock()
                            .unwrap()
                            .insert(key.clone(), (inj.broadcast_id.clone(), inj.text.clone()));
                    } else {
                        finish_key(&win, &inj.broadcast_id, &key, "error", "", false, "");
                    }
                }
            });
        }
    }
    // Watchdog: whatever is still pending for this bid after 200s becomes an error card
    // (covers pages that never load, harvest scripts killed by an unload, login walls...).
    {
        let win = window.clone();
        let bid = broadcast_id.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(200));
            let stuck: Vec<String> = broadcasts()
                .lock()
                .unwrap()
                .get(&bid)
                .map(|bc| bc.pending.iter().cloned().collect())
                .unwrap_or_default();
            for key in stuck {
                debug::log(format!("kotodama watchdog: bid={bid} key={key} silent"));
                pending_injections().lock().unwrap().remove(&key);
                chunk_bufs().lock().unwrap().remove(&(bid.clone(), key.clone()));
                finish_key(&win, &bid, &key, "error", "", false, "");
            }
        });
    }
    Ok(())
}

/// Cancels a broadcast: every still-pending key gets a `cancelled` card; the injected
/// JS loops self-expire on their own timeouts (their deliveries will find nothing here).
#[tauri::command]
pub fn kotodama_cancel(window: Window, broadcast_id: String) -> Result<(), String> {
    let stuck: Vec<String> = broadcasts()
        .lock()
        .unwrap()
        .get(&broadcast_id)
        .map(|bc| bc.pending.iter().cloned().collect())
        .unwrap_or_default();
    for key in stuck {
        pending_injections().lock().unwrap().remove(&key);
        chunk_bufs().lock().unwrap().remove(&(broadcast_id.clone(), key.clone()));
        finish_key(&window, &broadcast_id, &key, "cancelled", "", false, "");
    }
    Ok(())
}
