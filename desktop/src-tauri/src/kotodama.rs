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
    ("anthropic", r#".font-claude-message"#, r#"div[data-is-streaming="true"]"#),
    ("gemini", r#"message-content, .model-response-text"#, ""),
    ("perplexity", r#"main .prose"#, r#"button[aria-label*="stop" i]"#),
    ("deepseek", r#".ds-markdown"#, ""),
    ("qwen", "", ""),
    ("grok", r#"[class*="message-bubble"]"#, ""),
    ("zai", "", ""),
];

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
/// DELIVER: chunked sentinel navigations, serialized 200ms apart (rapid successive
///      location.href assignments coalesce — only the last would fire).
const HARVEST_JS: &str = r##"
(function(){
  var BID = __kt_bid, KEY = __kt_key, ANS_SEL = __kt_ans, BUSY_SEL = __kt_busy, FRESH = __kt_fresh;
  // The prompt we just sent (from the fill script): never harvest our own message back
  // (the generic selector chain can match the USER bubble on providers without a
  // dedicated assistant selector).
  var SENT = (typeof __apb_text === 'string') ? __apb_text.trim() : '';
  window.__ktBid = BID;               // a newer injection overwrites; older loops self-terminate
  var t0 = Date.now();
  // Screen-reader-only labels (e.g. Claude's "Claude ha risposto:") are clipped, not
  // display:none, so innerText INCLUDES them -> hide them for real before harvesting.
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
  function nav(q){ try { window.location.href = 'https://kotodama.result/?' + q; } catch(e){} }
  function esc(s){ return encodeURIComponent(s); }
  function lastMatch(sel){
    if (!sel) return null;
    try { var els = document.querySelectorAll(sel); return els.length ? els[els.length-1] : null; }
    catch(e){ return null; }
  }
  function getAnswerEl(){
    return lastMatch(ANS_SEL)
        || lastMatch('[data-message-author-role="assistant"]')
        || lastMatch('[class*="assistant" i]')
        || lastMatch('[class*="answer" i]')
        || lastMatch('[class*="response" i]')
        || lastMatch('.markdown, .prose')
        || lastMatch('article')
        || lastMatch('[class*="bubble" i]');
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
    return t;
  }
  // "Still generating?" - LANGUAGE-INDEPENDENT (no localized aria-label text). Uses the
  // per-provider BUSY_SEL (data-* attrs) + neutral streaming markers. NB: this only speeds
  // up completion; the reliable, language-independent signal is text STABILITY (below).
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
  function composerVal(){
    // Same VISIBLE-only pick as the fill script (ChatGPT keeps a hidden legacy textarea).
    var el = null;
    var sels = ['textarea:not([readonly]):not([aria-hidden="true"])', '[contenteditable="true"]', 'div[role="textbox"]'];
    for (var i=0;i<sels.length && !el;i++){
      var els = document.querySelectorAll(sels[i]);
      for (var j=0;j<els.length;j++){ if (els[j].offsetParent !== null) { el = els[j]; break; } }
    }
    if (!el) return null;
    return (el.value !== undefined ? el.value : el.innerText) || '';
  }
  function deliver(st, txt){
    if (window.__ktBid !== BID) return;
    window.__ktBid = null;
    txt = txt || '';
    var MAXC = 150000, trunc = 0;
    if (txt.length > MAXC) { txt = txt.slice(0, MAXC); trunc = 1; }
    var CH = 1500, n = Math.max(1, Math.ceil(txt.length / CH)), i = 0;
    function sendNext(){
      if (i >= n) return;
      var chunk = txt.slice(i*CH, (i+1)*CH);
      nav('b='+esc(BID)+'&k='+esc(KEY)+'&st='+st+'&s='+i+'&n='+n+(trunc?'&tr=1':'')+'&d='+esc(chunk));
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
  var sawText = false, armTries = 0;
  var armIv = setInterval(function(){
    if (window.__ktBid !== BID) { clearInterval(armIv); return; }
    armTries++;
    if (document.querySelector('input[type="password"]')) { clearInterval(armIv); deliver('login',''); return; }
    var cur = answerTxt();
    if (cur && cur !== initialAnswer) { clearInterval(armIv); harvest(); return; }  // answer streaming
    var v = composerVal();
    if (v !== null) {
      if (v.trim().length > 0) { sawText = true; }
      else if (sawText) { clearInterval(armIv); harvest(); return; }                 // composer emptied = accepted
    }
    // ~30s: never sent. census() first (-> debug log), then deliver AFTER a gap: two
    // back-to-back location.href assignments coalesce and the first would be lost.
    if (armTries > 60) { clearInterval(armIv); census(); setTimeout(function(){ deliver('sendfail',''); }, 300); }
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
    var c = composerVal(); out.push('composer='+(c===null?'NONE':('len'+c.length)));
    // visible buttons inventory (send-button selector tuning): testid/aria-label + disabled flag
    try {
      var bs = document.querySelectorAll('button'), bl = [];
      for (var k=0;k<bs.length && bl.length<8;k++){
        var b = bs[k]; if (b.offsetParent === null) continue;
        var idl = (b.getAttribute('data-testid') || b.getAttribute('aria-label') || b.id || '').slice(0,25);
        if (idl) bl.push(idl + (b.disabled ? '!D' : ''));
      }
      out.push('btns=' + bl.join(','));
    } catch(e){}
    nav('b='+esc(BID)+'&k='+esc(KEY)+'&st=diag&d='+esc(out.join(' || ').slice(0,1400)));
  }
  function harvest(){
    var last = '', stable = 0, polls = 0, sentCensus = false;
    var iv = setInterval(function(){
      if (window.__ktBid !== BID) { clearInterval(iv); return; }
      polls++;
      var txt = answerTxt();
      if (txt === initialAnswer) txt = '';   // still showing the previous answer, new one not in DOM yet
      var busy = isBusy();
      if (txt && txt === last) { stable++; } else { stable = 0; }
      last = txt;
      // done = text stable 3 polls with no busy marker; OR stable 10 polls regardless
      // (some pages keep a false-positive "stop"-like control on screen forever, e.g. Qwen).
      if (stable >= 3 && !busy || stable >= 10) { clearInterval(iv); deliver('done', txt); return; }
      if (Date.now() - t0 > 180000) { clearInterval(iv); deliver(txt ? 'timeout' : 'error', txt); return; }
      if (!sentCensus && polls === 15 && !txt) { sentCensus = true; census(); }
      if (polls % 3 === 0) { nav('b='+esc(BID)+'&k='+esc(KEY)+'&st=progress&len='+txt.length); }
    }, 1000);
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

fn temp_trigger_js(key: &str) -> Option<String> {
    match key {
        // anthropic (Claude): URL-based, see ktBaseUrlFor. The others have NO incognito URL, so we
        // click their toggle by its ICON. The frontend loads the compose page (drops ?q= where
        // present) when temp is on so the toggle can be clicked before fill+send.
        "grok" => Some(temp_click_js(GROK_PRIVATE_SVG)),         // "Passa alla chat privata" ghost
        "perplexity" => Some(temp_click_js(PERPLEXITY_INCOG_SVG)), // "Usa in incognito" spy icon
        "qwen" => Some(temp_click_js(QWEN_TEMP_SVG)),           // "Temporary Chat" toggle
        "gemini" => Some(temp_click_js(GEMINI_TEMP_SVG)),       // "Chat temporanea" mat-icon
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
  function diag(m){{ try{{ if(window.__ktDiag) window.location.href='https://kotodama.result/?b='+encodeURIComponent(__kt_bid)+'&k='+encodeURIComponent(__kt_key)+'&st=diag&d='+encodeURIComponent(m); }}catch(e){{}} }}
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
      window.location.href = 'https://kotodama.result/?b='+encodeURIComponent(__kt_bid)+'&k='+encodeURIComponent(__kt_key)+'&st=diag&d='+encodeURIComponent(msg.slice(0,1400));
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
        "var __kt_bid = {}; var __kt_key = {}; var __kt_ans = {}; var __kt_busy = {}; var __kt_fresh = {fresh}; window.__ktDiag = {diag};",
        serde_json::to_string(broadcast_id).map_err(|e| e.to_string())?,
        serde_json::to_string(key).map_err(|e| e.to_string())?,
        serde_json::to_string(ans).map_err(|e| e.to_string())?,
        serde_json::to_string(busy).map_err(|e| e.to_string())?,
        diag = crate::debug::enabled(),
    );
    // incognito/temporary trigger (holds the fill until done), only on fresh turns of providers
    // that have an in-page trigger AND the user enabled it for this provider.
    let temp_part = if fresh && temp { temp_trigger_js(key).unwrap_or_default() } else { String::new() };
    // The INCOG diagnostic probe only runs under KOTODAMA_DEBUG (used to discover a provider's
    // incognito URL/selector); never in production.
    let probe = if fresh && crate::debug::enabled() { TEMP_PROBE_JS } else { "" };
    Ok(prelude + &temp_part + &browser::fill_js(text, true)? + HARVEST_JS + probe)
}

/// Resume script for a page that navigated mid-broadcast. Two cases, decided IN PAGE:
/// - the sent text is visible in the DOM -> the send happened, only harvest (never re-send:
///   a duplicate would double-post on ChatGPT-style redirects);
/// - the sent text is NOT in the DOM -> the original injection died before sending (Qwen/Z.ai
///   landing pages navigate right after load), so fill+send first, then harvest.
fn build_resume_js(broadcast_id: &str, key: &str, text: &str) -> Result<String, String> {
    let (ans, busy) = selectors_for(key);
    // Whitespace-collapsed head of the message for a robust "is it on the page?" check.
    let head: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect();
    let prelude = format!(
        "var __apb_text = {}; var __apb_send = true; var __kt_head = {}; var __kt_bid = {}; var __kt_key = {}; var __kt_ans = {}; var __kt_busy = {}; var __kt_fresh = true;",
        serde_json::to_string(text).map_err(|e| e.to_string())?,
        serde_json::to_string(&head).map_err(|e| e.to_string())?,
        serde_json::to_string(broadcast_id).map_err(|e| e.to_string())?,
        serde_json::to_string(key).map_err(|e| e.to_string())?,
        serde_json::to_string(ans).map_err(|e| e.to_string())?,
        serde_json::to_string(busy).map_err(|e| e.to_string())?,
    );
    let fill = browser::fill_js(text, true)?;
    Ok(prelude
        + &format!(
            "if (!(document.body && document.body.innerText.replace(/\\s+/g,' ').indexOf(__kt_head) !== -1)) {{ {fill} }}"
        )
        + HARVEST_JS)
}

/// Marks (bid, key) answered: removes it from the broadcast, emits `app://kotodama-answer`
/// and, when the broadcast empties, `app://kotodama-finished`. Duplicate calls are no-ops.
fn finish_key(window: &Window, bid: &str, key: &str, status: &str, text: &str, truncated: bool) {
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
    // Answer delivered: stop resuming this key's harvest on future page loads (only if the
    // registered harvest belongs to THIS broadcast — a newer one must keep its entry).
    {
        let mut ah = active_harvests().lock().unwrap();
        if ah.get(key).map(|(b, _)| b == bid).unwrap_or(false) {
            ah.remove(key);
        }
    }
    debug::log(format!("kotodama answer bid={bid} key={key} status={status} len={}", text.len()));
    let _ = window.emit(
        "app://kotodama-answer",
        serde_json::json!({ "broadcastId": bid, "key": key, "status": status, "text": text, "truncated": truncated }),
    );
    if all_done {
        let _ = window.emit("app://kotodama-finished", serde_json::json!({ "broadcastId": bid }));
    }
}

/// Sentinel handler, called from `create_tab`'s `on_navigation` for `kotodama.result` URLs.
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
    if st == "diag" {
        // DOM census from a stuck harvest: log-only, this is how provider selectors get tuned.
        debug::log(format!("kotodama DIAG key={key}: {}", data.unwrap_or_default()));
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
        let buf = bufs
            .entry((bid.clone(), key.clone()))
            .or_insert_with(|| ChunkBuf { parts: vec![None; total], status: st.clone(), trunc });
        if buf.parts.len() != total {
            buf.parts = vec![None; total]; // total changed: superseded delivery, restart buffer
            buf.status = st.clone();
        }
        buf.parts[seq] = Some(data.unwrap_or_default());
        if trunc {
            buf.trunc = true;
        }
        if buf.parts.iter().all(|p| p.is_some()) {
            let text: String = buf.parts.iter().map(|p| p.as_deref().unwrap_or("")).collect();
            let status = buf.status.clone();
            let tr = buf.trunc;
            bufs.remove(&(bid.clone(), key.clone()));
            Some((text, status, tr))
        } else {
            None
        }
    };
    if let Some((text, status, tr)) = done {
        finish_key(window, &bid, &key, &status, &text, tr);
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
            let _ = webview.eval(&(prelude + TEMP_PROBE_JS));
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
            debug::log(format!("kotodama RESUME harvest after nav key={key} bid={bid}"));
            if let Ok(js) = build_resume_js(&bid, key, &text) {
                let _ = webview.eval(&js);
            }
        }
    }
}

/// Aborts any in-flight harvest for `key` (recipe re-navigation, supersede): the pending
/// card flips to error right away instead of waiting for the watchdog.
pub fn abort_key(window: &Window, key: &str) {
    pending_injections().lock().unwrap().remove(key);
    active_harvests().lock().unwrap().remove(key);
    chunk_bufs().lock().unwrap().retain(|(_, k), _| k != key);
    let bids: Vec<String> = broadcasts()
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, bc)| bc.pending.contains(key))
        .map(|(bid, _)| bid.clone())
        .collect();
    for bid in bids {
        finish_key(window, &bid, key, "error", "", false);
    }
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
                    finish_key(&window, &bid, key, "error", "", false);
                }
            }
        }
        // Instant card in the UI.
        let _ = window.emit(
            "app://kotodama-answer",
            serde_json::json!({ "broadcastId": broadcast_id, "key": key, "status": "pending", "text": "" }),
        );
        let Some(base) = bases.get(key) else {
            finish_key(&window, &broadcast_id, key, "error", "", false);
            continue;
        };
        let label = browser::provider_label(key);
        let existing = window.get_webview(&label);
        if let (Some(webview), false) = (&existing, new_chat) {
            // Warm follow-up: inject straight into the loaded page (keeps the conversation).
            match build_inject_js(&broadcast_id, key, &text, false, false) {
                Ok(js) => {
                    debug::log(format!("kotodama inject (warm) key={key}"));
                    if webview.eval(&js).is_err() {
                        finish_key(&window, &broadcast_id, key, "error", "", false);
                    } else {
                        active_harvests()
                            .lock()
                            .unwrap()
                            .insert(key.clone(), (broadcast_id.clone(), text.clone()));
                    }
                }
                Err(_) => finish_key(&window, &broadcast_id, key, "error", "", false),
            }
            continue;
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
                finish_key(&window, &broadcast_id, key, "error", "", false);
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
            finish_key(&window, &broadcast_id, key, "error", "", false);
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
                        finish_key(&win, &inj.broadcast_id, &key, "error", "", false);
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
                finish_key(&win, &bid, &key, "error", "", false);
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
        finish_key(&window, &broadcast_id, &key, "cancelled", "", false);
    }
    Ok(())
}
