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
/// Injections that already ran: provider key -> (broadcast_id, sent text, private chat clicked). Needed because
/// some provider pages NAVIGATE right after send (Qwen landing -> chat route, ChatGPT /?q= -> /c/<id>; Grok's own
/// private-chat toggle is a real `<a href>` whose click fires the same WebView2 navigation events even for its
/// in-page route change), killing the injected harvester with the old document: on the next page-load we
/// re-inject a HARVEST-ONLY (or resend) script to resume. The third field lets that resume re-assert the
/// private chat instead of silently sending into a normal conversation saved in the account's history
/// (measured on Grok, 27/09/2026). Cleared when the key's answer is delivered.
fn active_harvests() -> &'static Mutex<HashMap<String, (String, String, bool)>> {
    static A: OnceLock<Mutex<HashMap<String, (String, String, bool)>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Best-known per-provider DOM selectors: (key, answer container, "still generating" marker).
/// Empty string = rely on the generic fallback chain in the harvest JS. These WILL drift as
/// providers redesign; the JS treats them as the first candidate only, so a stale selector
/// degrades to the generic chain instead of breaking.
const HARVEST_SELECTORS: &[(&str, &str, &str)] = &[
    // Since 26/09/2026 ChatGPT no longer marks messages with data-message-author-role: the answer's markdown
    // root carries data-markdown-text-style="assistant-message" (read live through the WebView2 inspection
    // port on 27/09). The old marker stays second, for an older build of their page.
    ("openai", r#"[data-markdown-text-style="assistant-message"], [data-message-author-role="assistant"]"#, r#"button[data-testid="stop-button"]"#),
    // `.font-claude-response` is the current class; `.font-claude-message` was the previous one and is
    // kept so an older build of their UI still matches. Captured live 2026-08-18 -- with neither
    // matching, the chain fell back to the whole message wrapper and harvested the screen-reader
    // label and the reasoning block along with the answer.
    // Since October 2026 neither class exists any more (measured 14/10/2026 on the page itself): each message is
    // `[data-testid="assistant-message"]` (it carries `data-is-streaming`, which is the busy marker) and the text
    // of the reply sits in a block with `data-perf-reply-text` / `.standard-markdown`, apart from the message's
    // status pill ("Searched the web"), its screen-reader heading and its action bar with the time. The block, not
    // the message, is the answer. The old classes stay for an older build of their page.
    ("anthropic", r#"[data-testid="assistant-message"] [data-perf-reply-text], [data-testid="assistant-message"] .standard-markdown, .font-claude-response, .font-claude-message"#, r#"div[data-is-streaming="true"]"#),
    ("gemini", r#"message-content, .model-response-text"#, ""),
    ("perplexity", r#"main .prose"#, r#"button[aria-label*="stop" i]"#),
    ("deepseek", r#".ds-markdown"#, ""),
    ("qwen", "", ""),
    // Was `.message-bubble:not(.rounded-br-lg)`: `rounded-br-lg` is gone from Grok's bubbles, so the
    // exclusion matched the user's bubble too (only "last match wins" kept it working). Measured live
    // 18/10/2026 (inspection port 9344, 10 turns): both bubbles share `.message-bubble` and carry an
    // explicit `data-testid` (`user-message` / `assistant-message`), which is what is used now.
    // No busy marker on purpose. Measured in the same run: `canvas-working-indicator` exists only for
    // ~0.7 s BEFORE the first character (then the page has no text anyway), and the `.thinking-container`
    // child that spans the generation is emptied at the end, but TODO 49 reported its "Working for 1s"
    // label left in the answer (not reproduced here), where a busy marker would hold a finished answer
    // for ten polls. The end is decided by the stream event plus a settled page (see `doneByEvent` in
    // HARVEST_JS); the label is kept out of the answer text by `CHROME_SEL`.
    ("grok", r#"[data-testid="assistant-message"]"#, ""),
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
/// (broadcast, provider) pairs whose send was refused because an attachment never showed up in the provider's
/// composer. The page only sends the code `ATTACH_FAIL_CODE`; the wording is the UI's (`ktAttachFailed`).
fn attach_fail_marks() -> &'static Mutex<HashSet<(String, String)>> {
    static S: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}
const ATTACH_FAIL_CODE: &str = "@attach";
/// True for the page's "the files did not arrive, nothing was sent" delivery.
fn is_attach_failure(st: &str, d: Option<&str>) -> bool {
    st == "sendfail" && d == Some(ATTACH_FAIL_CODE)
}
/// Providers whose page may still receive a late upload after an attachment failure (a slow file that shows up in the
/// composer after the last check): the next send to them must land on a NEW page, never on that one, or the late
/// file would travel with an unrelated message.
fn dirty_pages() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}
/// Both delivery paths (IPC and the navigation fallback) go through `handle_push`, which calls this first: when the
/// delivery is the attachment-failure code it marks (bid, key) and strips the code from the data, so the card
/// carries no text. Returns whether it was one.
fn take_attach_failure(bid: &str, key: &str, st: &str, data: &mut Option<String>) -> bool {
    if !is_attach_failure(st, data.as_deref()) {
        return false;
    }
    attach_fail_marks().lock().unwrap().insert((bid.to_string(), key.to_string()));
    *data = Some(String::new());
    true
}

/// (broadcast, provider) pairs currently waiting on the user (human check, sign-in, window over the
/// composer). The watchdog leaves them alone: the time spent solving a check is not a silent page.
fn blocked_marks() -> &'static Mutex<HashSet<(String, String)>> {
    static S: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}
/// The subset of `blocked_marks` whose block is a sign-in wall. Signing in navigates the page, and the new
/// document does not know a block was ever pushed, so it never pushes "unblocked": the page load that brings
/// the tab back ends the block instead (on_page_finished). A message that met a sign-in wall did not really
/// reach the user's account, so that same page load sends it again, once.
fn login_marks() -> &'static Mutex<HashSet<(String, String)>> {
    static S: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}
/// Network readers per (broadcast, provider), one per answer request the page made (netread.rs).
type NetReaders = HashMap<String, Box<dyn crate::netread::Reader>>;
/// When the last network preview went out for a (broadcast, provider), and how long the text was then. Both
/// matter: the time throttles the flood of frames, the length keeps the preview from ever going backwards.
fn net_partials() -> &'static Mutex<HashMap<(String, String), (std::time::Instant, usize)>> {
    static P: OnceLock<Mutex<HashMap<(String, String), (std::time::Instant, usize)>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

fn net_readers() -> &'static Mutex<HashMap<(String, String), NetReaders>> {
    static S: OnceLock<Mutex<HashMap<(String, String), NetReaders>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}
/// (broadcast, provider) whose network stream has ended and whose delivery from the network is already
/// scheduled (see `NET_FINISH_GRACE`), so a socket that keeps sending does not schedule it again.
fn net_finish_armed() -> &'static Mutex<HashSet<(String, String)>> {
    static S: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}
/// How long the page gets, after the provider's stream says the answer is complete, to deliver it
/// itself. The page's answer carries what only the rendered page has (links, images), so it wins when it
/// works; this grace only matters when it does not. Measured on ChatGPT on 2026-09-26: after a redesign
/// its answer element no longer matched any selector, the stream had ended with the full text, and the
/// turn stayed "in progress" until the next send cancelled it ("Interrupted" next to a visible answer).
/// A provider changing its page must never again leave a finished answer hanging.
const NET_FINISH_GRACE: std::time::Duration = std::time::Duration::from_millis(3000);
/// The same fall-back for a provider whose stream has no end marker of its own (the transport closing is the only
/// end there is), and only for the providers listed in `NET_ORPHAN_KEYS`: their page can fail to RENDER an answer
/// the stream delivered whole. Measured 15/10/2026 on Gemini: after Ferma the next question was answered on the
/// network in 2 s ("105") and the page never showed it, so the card waited out the 180 s budget. Much longer than
/// the explicit-end grace because a closed transport is weaker evidence of a finished answer than a marker.
const NET_ORPHAN_GRACE: std::time::Duration = std::time::Duration::from_millis(20000);
const NET_ORPHAN_KEYS: &[&str] = &["gemini"];
/// JS literal for `window.__ktNetUrl`: the provider's answer endpoint pattern, or null (reading from the
/// network off for this provider, or disabled with KOTO_NO_NETREAD to compare against the DOM).
fn net_url_js(key: &str) -> String {
    if std::env::var("KOTO_NO_NETREAD").is_ok() {
        return "null".into();
    }
    crate::netread::url_pattern(key)
        .and_then(|p| serde_json::to_string(p).ok())
        .unwrap_or_else(|| "null".into())
}
/// Debug only: the last per-reader summary logged, so a change shows and a steady state does not.
fn net_work_dbg() -> &'static Mutex<HashMap<(String, String), String>> {
    static S: OnceLock<Mutex<HashMap<(String, String), String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}
/// What the page was last told about the provider writing (see `Reader::working`), per (broadcast, provider).
fn net_work() -> &'static Mutex<HashMap<(String, String), bool>> {
    static S: OnceLock<Mutex<HashMap<(String, String), bool>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}
/// The stream (reader id) whose "writing / stopped" state changed last, per (broadcast, provider). Several streams of
/// one conversation can be open at once (Claude re-opens its timeline now and then, and the old one may stay open):
/// each says what it saw, and the one that spoke last knows the present, see `pick_working`.
fn net_work_owner() -> &'static Mutex<HashMap<(String, String), String>> {
    static S: OnceLock<Mutex<HashMap<(String, String), String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}
/// What the page is told about the provider writing, from the state of each open stream (`(id, working)`).
/// An OR over the streams made the answer's end wait for ALL of them: a stream that opened mid-answer and never
/// saw the idle state (or the old one left open) kept it "writing" after the one that follows the answer had
/// said "stopped", and the end came 3 s late from the grace delivery (`NET_FINISH_GRACE`) instead of from the
/// event. The stream that changed last wins while it still has a state; when it has none (closed while writing)
/// the old rule applies.
fn pick_working(states: &[(String, Option<bool>)], owner: Option<&str>) -> Option<bool> {
    if let Some(o) = owner {
        if let Some((_, Some(w))) = states.iter().find(|(id, _)| id == o) {
            return Some(*w);
        }
    }
    states.iter().filter_map(|(_, w)| *w).fold(None, |acc: Option<bool>, w| Some(acc.unwrap_or(false) || w))
}
/// Passes the provider's own "writing / stopped" signal, read from its stream, on to the DOM harvest -- for a
/// provider whose answer stream stays open between answers (Claude's timeline), so no response opens or closes and
/// the harvest's usual signals do not exist. While it writes: the send did reach the provider (`__ktStreamEver`, else
/// 30 s without text read as "never sent", which a long reasoning is) and text that pauses is not an end
/// (`__ktNetOpen`, as for ChatGPT: only 45 still polls or the end event may close it). When it stops: the end event
/// the harvest waits for (`__ktStreamEnd`). Only changes are sent.
fn tell_page_work(window: &Window, bid: &str, key: &str, work: Option<bool>) {
    let Some(now) = work else { return };
    let prev = net_work().lock().unwrap().insert((bid.to_string(), key.to_string()), now);
    if prev == Some(now) {
        return;
    }
    debug::log(format!("kotodama NET working key={key} bid={bid} -> {now}"));
    let js = if now {
        "window.__ktStreamEver=(window.__ktStreamEver||0)+1;window.__ktNetOpen=1;"
    } else {
        "window.__ktNetOpen=0;if(window.__ktStreamEnd)window.__ktStreamEnd();"
    };
    if let Some(wv) = window.get_webview(&crate::browser::provider_label(key)) {
        let _ = wv.eval(js);
    }
}
/// The complete answer read from the network for this provider, if any reader saw its end with text.
fn net_answer(bid: &str, key: &str) -> Option<crate::netread::NetAnswer> {
    let map = net_readers().lock().unwrap();
    let readers = map.get(&(bid.to_string(), key.to_string()))?;
    readers
        .values()
        .map(|r| r.answer())
        .filter(|a| a.done && !a.text.trim().is_empty())
        .max_by_key(|a| a.text.len())
        .cloned()
}
/// Files the user attached to a message, per broadcast, ready to be handed to each provider page.
#[derive(Clone, serde::Serialize)]
struct Attachment {
    name: String,
    mime: String,
    b64: String,
}
fn attachments() -> &'static Mutex<HashMap<String, Vec<Attachment>>> {
    static S: OnceLock<Mutex<HashMap<String, Vec<Attachment>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}
// ---- Read aloud (audio.rs): the provider's own voice for the answer its page still shows. ----
/// The conversation each provider page shows right now, as the broadcast whose answer is the LAST one in it.
/// Set when an answer is delivered, dropped as soon as the page moves on (a new send, a pre-warm navigation).
/// Read aloud only works on that answer: the provider's button reads what is on its page.
fn conv_bids() -> &'static Mutex<HashMap<String, String>> {
    static C: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}
fn set_conv(window: &Window, key: &str, bid: Option<&str>) {
    let changed = {
        let mut m = conv_bids().lock().unwrap();
        let before = m.get(key).cloned();
        match bid {
            Some(b) => m.insert(key.to_string(), b.to_string()),
            None => m.remove(key),
        };
        before.as_deref() != bid
    };
    if changed {
        let _ = window.emit(
            "app://kotodama-conv",
            serde_json::json!({ "key": key, "broadcastId": bid, "tts": crate::audio::tts_url_pattern(key).is_some(), "regen": regen_button(key).is_some() }),
        );
    }
}
/// One read-aloud request per provider: the speech socket's packets, until it closes.
struct AudioCap {
    bid: String,
    req: String,
    /// Raw Opus packets, for the providers that stream them over a WebSocket (Claude, DeepSeek).
    packets: Vec<Vec<u8>>,
    /// A finished audio file, for the providers that answer with one (ChatGPT: audio/aac), plus its type.
    file: Vec<u8>,
    file_ct: String,
    bytes: usize,
}
fn audio_caps() -> &'static Mutex<HashMap<String, AudioCap>> {
    static A: OnceLock<Mutex<HashMap<String, AudioCap>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(HashMap::new()))
}
/// About 30 minutes of speech at the bitrates captured (26-32 kb/s). A longer stream stops being recorded.
const AUDIO_MAX_BYTES: usize = 8 * 1024 * 1024;

fn audio_dir(window: &Window) -> Result<std::path::PathBuf, String> {
    let dir = window.app_handle().path().app_config_dir().map_err(|e| e.to_string())?.join("kotodama-audio");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}
/// A saved audio is addressed by its bare file name, never a path: nothing outside the audio folder is reachable.
fn audio_file_ok(name: &str) -> bool {
    [".ogg", ".aac", ".mp3", ".m4a", ".wav", ".webm"].iter().any(|e| name.ends_with(e)) && !name.contains("..") && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Presses the provider's read-aloud button (`mode` "play") or presses it again to stop ("stop"). The answer
/// must still be the last one on the provider page, otherwise its button would read another message.
/// Returns the request id the `app://kotodama-audio` events carry.
#[tauri::command]
pub fn kotodama_read_aloud(window: Window, broadcast_id: String, key: String, mode: String) -> Result<String, String> {
    let pattern = crate::audio::tts_url_pattern(&key).ok_or("unsupported")?;
    if conv_bids().lock().unwrap().get(&key) != Some(&broadcast_id) {
        return Err("gone".into());
    }
    let wv = window.get_webview(&browser::provider_label(&key)).ok_or("gone")?;
    browser::resume_provider(&window, &key, true);
    let req = if mode == "stop" {
        audio_caps().lock().unwrap().get(&key).map(|c| c.req.clone()).unwrap_or_default()
    } else {
        let req = format!("a{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0));
        audio_caps().lock().unwrap().insert(
            key.clone(),
            AudioCap { bid: broadcast_id.clone(), req: req.clone(), packets: Vec::new(), file: Vec::new(), file_ct: String::new(), bytes: 0 },
        );
        req
    };
    let (sel, path, item) = crate::audio::tts_button(&key);
    let js = format!(
        "var __kt_bid = {}; var __kt_key = {}; var __kt_req = {}; var __kt_mode = {}; var __kt_tts_url = {}; var __kt_tts_sel = {}; var __kt_tts_path = {}; var __kt_tts_item = {};",
        serde_json::to_string(&broadcast_id).map_err(|e| e.to_string())?,
        serde_json::to_string(&key).map_err(|e| e.to_string())?,
        serde_json::to_string(&req).map_err(|e| e.to_string())?,
        serde_json::to_string(&mode).map_err(|e| e.to_string())?,
        serde_json::to_string(pattern).map_err(|e| e.to_string())?,
        serde_json::to_string(sel).map_err(|e| e.to_string())?,
        serde_json::to_string(path).map_err(|e| e.to_string())?,
        serde_json::to_string(item).map_err(|e| e.to_string())?,
    ) + FIND_ACTION_JS + TTS_JS;
    debug::log(format!("kotodama READ-ALOUD key={key} bid={broadcast_id} mode={mode} req={req}"));
    wv.eval(&js).map_err(|e| e.to_string())?;
    Ok(req)
}

/// The provider's own "regenerate" control under the last answer, by structure only: (CSS selector, start of the
/// icon's SVG path when the button has no stable attribute, menu item to pick when the button opens a menu).
/// Captured with KOTO_ACTIONPROBE, 15/09/2026. Gemini's button opens "longer / shorter / again": the plain
/// regenerate is the menu item with the refresh icon.
fn regen_button(key: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match key {
        "anthropic" => Some(("[data-testid=\"action-bar-retry\"]", "", "")),
        "grok" => Some(("", "M4 20V15H4.31241", "")),
        "deepseek" => Some(("", "M7.92136 0.349152", "")),
        "gemini" => Some(("mat-icon[fonticon=\"refresh\"]", "", "[data-test-id=\"regenerate-option\"] mat-icon[fonticon=\"refresh\"]")),
        "zai" => Some(("", "M17.0441 10.7439", "")),
        "mistral" => Some(("", "M12.2432 1C18.2069", "")),
        _ => None,
    }
}

/// Presses the provider's regenerate button on the answer its page still ends with (`old_bid`) and harvests the new
/// answer as broadcast `new_bid`. The harvest is the normal warm one: it snapshots the current answer first and
/// waits for a different one, so the old text is never delivered as the new answer. The send is marked as done up
/// front (there is nothing to type), which keeps a re-injection after a navigation from sending anything.
#[tauri::command]
pub fn kotodama_regenerate(window: Window, old_bid: String, new_bid: String, key: String, text: String) -> Result<(), String> {
    let (sel, path, item) = regen_button(&key).ok_or("unsupported")?;
    if conv_bids().lock().unwrap().get(&key) != Some(&old_bid) {
        return Err("gone".into());
    }
    let wv = window.get_webview(&browser::provider_label(&key)).ok_or("gone")?;
    browser::resume_provider(&window, &key, true);
    broadcasts()
        .lock()
        .unwrap()
        .entry(new_bid.clone())
        .or_insert_with(|| Broadcast { pending: HashSet::new(), started: Instant::now() })
        .pending
        .insert(key.clone());
    sent_marks().lock().unwrap().insert((new_bid.clone(), key.clone()));
    set_conv(&window, &key, None);
    let _ = window.emit(
        "app://kotodama-answer",
        serde_json::json!({ "broadcastId": new_bid, "key": key, "status": "pending", "text": "" }),
    );
    let (ans, busy) = selectors_for(&key);
    let prelude = format!(
        "var __apb_text = {}; var __kt_bid = {}; var __kt_key = {}; var __kt_ans = {}; var __kt_busy = {}; var __kt_fresh = false; var __kt_fast = {fast}; var __kt_sent = true; var __kt_regen = true; window.__ktDiag = {diag}; window.__ktStreamEver = 0; window.__ktLimitHit = 0; window.__ktRateLimited = 0; window.__ktNetUrl = {net_url}; var __kt_regen_sel = {}; var __kt_regen_path = {}; var __kt_regen_item = {};",
        serde_json::to_string(&text).map_err(|e| e.to_string())?,
        serde_json::to_string(&new_bid).map_err(|e| e.to_string())?,
        serde_json::to_string(&key).map_err(|e| e.to_string())?,
        serde_json::to_string(ans).map_err(|e| e.to_string())?,
        serde_json::to_string(busy).map_err(|e| e.to_string())?,
        serde_json::to_string(sel).map_err(|e| e.to_string())?,
        serde_json::to_string(path).map_err(|e| e.to_string())?,
        serde_json::to_string(item).map_err(|e| e.to_string())?,
        fast = fast_done_for(&key),
        diag = crate::debug::enabled(),
        net_url = net_url_js(&key),
    );
    // Order matters: the harvest snapshots the answer on screen BEFORE the click replaces it.
    let js = prelude + PUSH_HELPER_JS + SR_HIDE_JS + RESPONSE_ADOPT_JS + net_probe_js() + STREAM_WATCH_JS + HARVEST_JS + FIND_ACTION_JS + REGEN_CLICK_JS;
    debug::log(format!("kotodama REGENERATE key={key} old={old_bid} new={new_bid}"));
    if wv.eval(&js).is_err() {
        finish_key(&window, &new_bid, &key, "error", "", false, "");
        return Ok(());
    }
    active_harvests().lock().unwrap().insert(key.clone(), (new_bid, text, false));
    Ok(())
}

/// A saved audio's bytes, base64, for the UI to play from a Blob (no file protocol scope to open).
#[tauri::command]
pub fn kotodama_audio_load(window: Window, file: String) -> Result<String, String> {
    if !audio_file_ok(&file) {
        return Err("bad name".into());
    }
    let bytes = std::fs::read(audio_dir(&window)?.join(&file)).map_err(|e| e.to_string())?;
    Ok(base64_encode(&bytes))
}

/// Copies a saved audio into Downloads/Kotodama and returns where it landed, so the user can keep or share it.
#[tauri::command]
pub fn kotodama_audio_export(window: Window, file: String) -> Result<String, String> {
    if !audio_file_ok(&file) {
        return Err("bad name".into());
    }
    let src = audio_dir(&window)?.join(&file);
    let dir = window.app_handle().path().download_dir().map_err(|e| e.to_string())?.join("Kotodama");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dst = dir.join(&file);
    std::fs::copy(&src, &dst).map_err(|e| e.to_string())?;
    // Show it in the file manager: the copy is silent otherwise, and a toast is easy to miss.
    {
        use tauri_plugin_opener::OpenerExt;
        let _ = window.app_handle().opener().reveal_item_in_dir(&dst);
    }
    debug::log(format!("kotodama audio export -> {}", dst.display()));
    Ok(dst.to_string_lossy().to_string())
}

/// Removes saved audios (a deleted conversation). Missing files are not an error.
#[tauri::command]
pub fn kotodama_audio_delete(window: Window, files: Vec<String>) -> Result<(), String> {
    let dir = audio_dir(&window)?;
    for f in files.iter().filter(|f| audio_file_ok(f)) {
        let _ = std::fs::remove_file(dir.join(f));
    }
    Ok(())
}

/// One message from TTS_JS: the socket opened, a batch of frames, the end, or why nothing played.
fn handle_audio_push(window: &Window, key: &str, raw: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else { return };
    let req = v.get("r").and_then(|x| x.as_str()).unwrap_or("");
    let ev = v.get("ev").and_then(|x| x.as_str()).unwrap_or("");
    let emit = |bid: &str, state: &str, extra: serde_json::Value| {
        let mut o = serde_json::json!({ "broadcastId": bid, "key": key, "req": req, "state": state });
        if let (Some(m), Some(e)) = (o.as_object_mut(), extra.as_object()) {
            m.extend(e.clone());
        }
        let _ = window.emit("app://kotodama-audio", o);
    };
    let mut caps = audio_caps().lock().unwrap();
    let Some(cap) = caps.get_mut(key).filter(|c| c.req == req) else { return };
    browser::touch_provider(key); // speaking is activity: the page must not be suspended mid-sentence
    match ev {
        "open" => emit(&cap.bid.clone(), "playing", serde_json::json!({})),
        "frames" => {
            let skip = crate::audio::frame_prefix(key);
            for f in v.get("f").and_then(|x| x.as_array()).into_iter().flatten() {
                let Some(bytes) = f.as_str().and_then(crate::audio::base64_decode) else { continue };
                if bytes.len() <= skip || cap.bytes + bytes.len() > AUDIO_MAX_BYTES {
                    continue;
                }
                cap.bytes += bytes.len();
                cap.packets.push(bytes[skip..].to_vec());
            }
        }
        "blob" => {
            if let Some(ct) = v.get("ct").and_then(|x| x.as_str()) {
                cap.file_ct = ct.to_string();
            }
            for f in v.get("f").and_then(|x| x.as_array()).into_iter().flatten() {
                let Some(bytes) = f.as_str().and_then(crate::audio::base64_decode) else { continue };
                if cap.bytes + bytes.len() > AUDIO_MAX_BYTES {
                    continue;
                }
                cap.bytes += bytes.len();
                cap.file.extend_from_slice(&bytes);
            }
        }
        "stopped" => emit(&cap.bid.clone(), "stopped", serde_json::json!({})),
        "end" | "nobutton" | "noaudio" => {
            let cap = caps.remove(key).unwrap();
            drop(caps);
            if ev != "end" {
                debug::log(format!("kotodama READ-ALOUD key={key} {ev}"));
                emit(&cap.bid, if ev == "nobutton" { "unavailable" } else { "error" }, serde_json::json!({}));
                return;
            }
            // A finished file is saved as it came; raw packets are wrapped in an Ogg Opus stream first.
            let (bytes, ext, ms) = if !cap.file.is_empty() {
                let ext = crate::audio::container_ext(&cap.file_ct).unwrap_or("bin");
                (cap.file.clone(), ext, crate::audio::file_duration_ms(ext, &cap.file))
            } else {
                let serial = req.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
                match crate::audio::ogg_opus(&cap.packets, serial) {
                    Some((ogg, ms)) => (ogg, "ogg", ms),
                    None => {
                        emit(&cap.bid, "error", serde_json::json!({}));
                        return;
                    }
                }
            };
            if ext == "bin" {
                debug::log(format!("kotodama READ-ALOUD key={key}: unknown audio type {:?}", cap.file_ct));
                emit(&cap.bid, "error", serde_json::json!({}));
                return;
            }
            let safe = |s: &str| s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>();
            let file = format!("{}-{}-{}.{ext}", safe(&cap.bid), safe(key), safe(req));
            match audio_dir(window).and_then(|d| std::fs::write(d.join(&file), &bytes).map_err(|e| e.to_string())) {
                Ok(()) => {
                    debug::log(format!("kotodama READ-ALOUD key={key} saved {file} ms={ms} bytes={}", bytes.len()));
                    emit(&cap.bid, "saved", serde_json::json!({ "file": file, "ms": ms }));
                }
                Err(e) => {
                    debug::log(format!("kotodama READ-ALOUD key={key} save failed: {e}"));
                    emit(&cap.bid, "error", serde_json::json!({}));
                }
            }
        }
        _ => {}
    }
}

const ATTACH_MAX_FILES: usize = 5;
const ATTACH_MAX_BYTES: u64 = 10 * 1024 * 1024;

fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "md" | "csv" | "log" => "text/plain",
        "json" => "application/json",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// A file the UI wants to attach: a path (dropped from the file manager) or inline base64 data (pasted).
#[derive(serde::Deserialize)]
pub struct AttachmentIn {
    name: String,
    path: Option<String>,
    data: Option<String>,
    mime: Option<String>,
}

/// What the UI shows for a dropped path before sending: name, size and whether it can be attached.
#[derive(serde::Serialize)]
pub struct FileInfo {
    path: String,
    name: String,
    size: u64,
    ok: bool,
}

#[tauri::command]
pub fn kotodama_file_info(paths: Vec<String>) -> Vec<FileInfo> {
    paths
        .into_iter()
        .map(|p| {
            let meta = std::fs::metadata(&p).ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let is_file = meta.map(|m| m.is_file()).unwrap_or(false);
            let name = std::path::Path::new(&p).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            FileInfo { ok: is_file && size <= ATTACH_MAX_BYTES, path: p, name, size }
        })
        .collect()
}

/// Stores the attachments of one message (by broadcast id) before its dispatch: every provider injection of
/// that broadcast receives them. Files are read here, not in the page, so the provider never sees a path.
#[tauri::command]
pub fn kotodama_set_attachments(broadcast_id: String, files: Vec<AttachmentIn>) -> Result<usize, String> {
    let mut out = Vec::new();
    for f in files.into_iter().take(ATTACH_MAX_FILES) {
        let b64 = if let Some(p) = &f.path {
            let meta = std::fs::metadata(p).map_err(|e| e.to_string())?;
            if !meta.is_file() || meta.len() > ATTACH_MAX_BYTES {
                continue;
            }
            base64_encode(&std::fs::read(p).map_err(|e| e.to_string())?)
        } else if let Some(d) = f.data {
            if d.len() as u64 > ATTACH_MAX_BYTES * 4 / 3 + 4 {
                continue;
            }
            d
        } else {
            continue;
        };
        let mime = f.mime.filter(|m| !m.is_empty()).unwrap_or_else(|| mime_for(&f.name).to_string());
        out.push(Attachment { name: f.name, mime, b64 });
    }
    let n = out.len();
    if n > 0 {
        attachments().lock().unwrap().insert(broadcast_id, out);
    }
    Ok(n)
}

/// JS prefix handing the broadcast's attachments to the page (empty when there are none).
fn attach_js(broadcast_id: &str) -> String {
    let Some(files) = attachments().lock().unwrap().get(broadcast_id).cloned() else { return String::new() };
    format!("window.__ktFiles = {};", serde_json::to_string(&files).unwrap_or_else(|_| "[]".into())) + ATTACH_JS
}

/// Hands the attached files to the provider page the way the page itself accepts them: through its own file
/// input (as if picked in its dialog), or, when it has none, as a paste/drop on its composer. Holds the text fill
/// until the attachment is VERIFIED in the composer; the page uploads with its own session as usual.
///
/// Rules, from a defect measured on 15/10/2026 (ChatGPT, new chat, image: 6 of 9 isolated sends reached the
/// provider as text only, the answer being "I can't see any attached file"):
///  1. Wait for a composer that is really there. ChatGPT first paints a plain `<textarea>` shell with no file input
///     and swaps it for its ProseMirror editor a moment later; files handed to the shell are dropped on the floor
///     while the old code logged "via paste" as a success. A composer that is a bare TEXTAREA is not ready until
///     the page offers a file input; a rich editor with no file input (Gemini creates it only on demand) is ready
///     after a short grace.
///  2. Verify with evidence that belongs to THIS file, never to the page finishing its own business: a new
///     blob:/data: preview or image in the composer's region (compared as a set with what was there before), or
///     the file's name showing up there (tolerant: entities decoded, extension dropped, truncated prefix).
///     Structure only, never visible words, which change with the interface language.
///  3. Never send without them. If the evidence does not come after the retries (each by a different route, never
///     while the previous upload is still climbing) the send is declared failed: the page sends only the CODE
///     '@attach'; the wording is the UI's, in the user's language.
///  4. One cycle per page: a newer cycle cancels the older one (like `__ktFillRun`), and the older one can neither
///     release the newer one's hold nor declare a failure on a message that already left.
const ATTACH_JS: &str = r##"
(function(){
  var files = window.__ktFiles;
  if (!files || !files.length || window.__ktFilesDone === __kt_bid) return;
  // Already declared failed for this broadcast (a re-injection after the failure): the message must not go out.
  if (window.__ktFilesFailed === __kt_bid) { try { window.__ktFillRun = '__ktCancelled'; window.__ktBid = null; } catch(e){} return; }
  var BID = __kt_bid;
  var RUN = 'a' + Date.now() + '-' + Math.random();
  window.__ktAttachRun = RUN;
  window.__ktHoldAttach = true;
  function current(){ return window.__ktAttachRun === RUN; }
  function diag(m){ try { if (window.__ktDiag) window.__TAURI__.core.invoke('kotodama_push', { b: BID, k: __kt_key, st: 'diag', d: 'ATTACH ' + m }).catch(function(){}); } catch(e){} }
  var totalBytes = 0;
  files.forEach(function(f){ totalBytes += Math.floor((f.b64 || '').length * 3 / 4); });
  function toFiles(){
    return files.map(function(f){
      var bin = atob(f.b64), u = new Uint8Array(bin.length);
      for (var i = 0; i < bin.length; i++) u[i] = bin.charCodeAt(i);
      return new File([u], f.name, { type: f.mime || 'application/octet-stream' });
    });
  }
  function composer(){
    var sels = ['textarea:not([readonly])', '[contenteditable="true"]', 'div[role="textbox"]'];
    // Never ChatGPT's open document block: its title <textarea> and body are editable too (same exclusion as
    // pickComposer in browser.rs and findComposerEl below), and an attachment dropped there never reaches the chat.
    for (var i = 0; i < sels.length; i++) { var e = document.querySelectorAll(sels[i]); for (var j = e.length - 1; j >= 0; j--) {
      var r = e[j].getBoundingClientRect();
      // Not Quill's hidden clipboard sink (Gemini: a contenteditable DIV.ql-clipboard, last in the document, which the
      // search took for the composer so every hand-over went to a node nobody reads) nor anything too small to type in.
      if (e[j].offsetParent !== null && r.width >= 40 && r.height >= 12 && !e[j].classList.contains('ql-clipboard') && !e[j].closest('[aria-hidden="true"]') && !e[j].closest('[data-testid="chatgpt-writing-block"]')) return e[j];
    } }
    return null;
  }
  function fileInput(){
    var ins = document.querySelectorAll('input[type="file"]:not([disabled])'), best = null;
    for (var i = 0; i < ins.length; i++) {
      var acc = (ins[i].getAttribute('accept') || '').toLowerCase();
      // Prefer a general input (no accept, or accepting images and documents) over one dedicated to e.g. audio.
      if (!acc || /image|\*|pdf|text/.test(acc)) { best = ins[i]; if (!acc || acc.indexOf('*') >= 0) break; }
    }
    return best || ins[0] || null;
  }
  // The composer's own region: its form, else the nearest ancestor that also holds the page's file input (where
  // the provider keeps the composer's previews and chips: on Claude the chip sits 6 levels above the editor, next
  // to its input), else up to 8 levels up. It never goes up to an ancestor that already holds the conversation
  // (messages), where a preview of an older message would pass for ours.
  function scopeOf(c){
    var f = c.closest('form');
    if (f) return f;
    var inp = fileInput(), limit = 8, s = c;
    for (var i = 0; i < limit && s.parentElement && s.parentElement !== document.body; i++) {
      var p = s.parentElement;
      if (p.querySelector('[data-message-author-role], article, [data-testid*="conversation-turn" i]')) break;
      s = p;
      if (inp && s.contains(inp)) break;
    }
    return s;
  }
  function decode(s){
    try { var t = document.createElement('textarea'); t.innerHTML = String(s); return t.value; } catch(e){ return String(s); }
  }
  function norm(s){ try { return decode(s).normalize('NFC').toLowerCase(); } catch(e){ return String(s).toLowerCase(); } }
  // Everything the region shows or carries as text: its text and its attribute values (alt, title, aria-label...).
  function haystack(sc){
    var parts = [sc.textContent || ''];
    try {
      var all = sc.querySelectorAll('*'), n = Math.min(all.length, 600);
      for (var i = 0; i < n; i++) { var at = all[i].attributes; for (var j = 0; j < at.length; j++) if (at[j].name !== 'class' && at[j].name !== 'd' && at[j].value.length < 300) parts.push(at[j].value); }
    } catch(e){}
    return norm(parts.join('\n'));
  }
  // Names a camera, a screenshot tool or a paste gives to many different files: seeing one on the page proves nothing.
  var GENERIC = /^(image|img|immagine|screenshot|screen shot|schermata|scan|scansione|foto|photo|picture|pasted|incollato|download|file|document|documento|untitled)([\s_\-.()0-9]*|[\s_\-]?\d.*)$/i;
  function nameHit(h, name){
    var n = norm(name); if (!n) return false;
    if (GENERIC.test(n.replace(/\.[a-z0-9]{1,6}$/, ''))) return false;
    if (h.indexOf(n) >= 0) return true;
    var base = n.replace(/\.[a-z0-9]{1,6}$/, '');
    if (base.length >= 3 && h.indexOf(base) >= 0) return true;
    // Chips shorten long names ("very-long-na..."): the first characters stand for the file.
    return base.length >= 14 && h.indexOf(base.slice(0, 12)) >= 0;
  }
  // What the region holds that can belong to a file: the sources of its media (compared as a set), the progress
  // bars of an upload, and which of our names are in sight.
  function snapshot(c, fixed){
    var s = { src: {}, names: [], progress: 0, srcN: 0 };
    try {
      // The region measured before the hand-over is the one measured after it (a menu's input can disappear
      // meanwhile and with it the way the region was found).
      var sc = (fixed && fixed.isConnected && fixed.contains(c)) ? fixed : scopeOf(c);
      s.scope = sc;
      sc.querySelectorAll('img, video, source, canvas').forEach(function(e){
        var u = e.currentSrc || e.getAttribute('src') || ''; if (!u) return;
        // A page's own images (icons, avatars, logos) are not previews: unless the source is a blob:/data: URL, an
        // image counts only once loaded and big enough to be a preview, never while it is still loading.
        if (!/^(blob|data):/.test(u) && e.tagName === 'IMG' && !(e.complete && e.naturalWidth >= 24)) return;
        s.src[u.slice(0, 300)] = 1; s.srcN++;
      });
      s.progress = sc.querySelectorAll('[role="progressbar"], progress, [aria-busy="true"]').length;
      var h = haystack(sc);
      files.forEach(function(f, i){ s.names[i] = nameHit(h, f.name); });
    } catch(e){}
    return s;
  }
  // New evidence specific to the files: a media source that was not there before, or one of our names that was not.
  function gained(before, now){
    var why = '';
    Object.keys(now.src).forEach(function(u){ if (!before.src[u] && !why) why = 'new media ' + u.slice(0, 40); });
    now.names.forEach(function(v, i){ if (v && !before.names[i] && !why) why = 'name of file ' + (i + 1); });
    return why;
  }
  function hand(how, c, input, dt){
    if (how === 'input') {
      input.files = dt.files;
      input.dispatchEvent(new Event('input', { bubbles: true }));
      input.dispatchEvent(new Event('change', { bubbles: true }));
    } else if (how === 'paste') {
      c.focus();
      c.dispatchEvent(new ClipboardEvent('paste', { bubbles: true, cancelable: true, clipboardData: dt }));
    } else {
      ['dragenter', 'dragover', 'drop'].forEach(function(t){ c.dispatchEvent(new DragEvent(t, { bubbles: true, cancelable: true, dataTransfer: dt })); });
    }
  }
  // Some editors create their file input only when the upload menu opens (Gemini: four hidden inputs appear when the
  // "+" menu is opened, measured 15/10/2026). The trigger is the first menu button inside the composer's region,
  // found by `aria-haspopup`, not by its label; the menu is closed again once the files are handed over.
  function menuButton(c){
    try {
      // Up to 8 levels (the "+" of Gemini sits 6 above its editor), still never into the conversation.
      var s = c.closest('form') || c, b = null;
      for (var i = 0; i < 8 && !b && s.parentElement && s.parentElement !== document.body; i++) {
        b = s.querySelector('button[aria-haspopup="menu"], [role="button"][aria-haspopup="menu"]');
        if (b) break;
        var p = s.parentElement;
        if (p.querySelector('[data-message-author-role], article, [data-testid*="conversation-turn" i]')) break;
        s = p;
      }
      // Never a button that would submit the form it sits in.
      if (b && b.tagName === 'BUTTON' && b.closest('form') && b.getAttribute('type') !== 'button') return null;
      return b;
    } catch(e){ return null; }
  }
  var menuBtn = null, menuTried = false, menuUntil = 0;
  // A late upload cannot be taken back from here (the files come from the provider's own widgets); what is done is
  // emptying the page's file inputs. Rust marks the page as dirty so the next send starts on a new one.
  function scrubInputs(){
    try { document.querySelectorAll('input[type="file"]').forEach(function(i){ try { i.value = ''; } catch(e){} }); } catch(e){}
  }
  function closeMenu(){
    try { if (menuBtn && menuBtn.getAttribute('aria-expanded') === 'true') menuBtn.click(); } catch(e){}
  }
  function release(ms){ setTimeout(function(){ if (current()) window.__ktHoldAttach = false; }, ms); }
  // The text must never go out without its files. The page only says THAT it failed (a code); the user-facing
  // wording belongs to the UI. `why` stays in the debug log.
  function fail(why){
    if (!current()) return;
    // The message already left (a newer cycle, a resume): nothing to declare, the send is a fact.
    if (window.__ktEnterPressed || window.__ktFilesDone === BID) { window.__ktHoldAttach = false; return; }
    window.__ktFilesFailed = BID;
    diag('FAILED ' + why + ': files were not attached, the message is NOT sent');
    try { window.__ktFillRun = '__ktCancelled'; } catch(e){}
    try { if (window.__ktBid === BID) window.__ktBid = null; } catch(e){}
    window.__ktHoldAttach = false;
    closeMenu();
    scrubInputs();
    try { window.__ktPush({ b: BID, k: __kt_key, st: 'sendfail', s: 0, n: 1, tr: false, d: '@attach', md: '', cp: false, se: false }); } catch(e){}
  }
  var t0 = Date.now(), seenAt = 0, attempt = 0, phase = 'ready', verifyUntil = 0, hardUntil = 0, before = null, how = '', input = null, c = null;
  var MAX_ATTEMPTS = 4;
  // Absolute ceiling for the whole cycle, whatever the phase.
  var CEILING_MS = 75000;
  // A composer with no file input is waited for: ChatGPT's shell textarea the longest (hydration of a cold page
  // behind several others), a rich editor only a short grace (the input is created on demand there).
  var READY_SHELL_MS = 25000, READY_RICH_MS = 4000;
  // How long one hand-over may take to show its preview. A big file takes longer to show up, and a second copy of a
  // file still on its way up is worse than a late verdict: the wait grows with the size, and while an upload
  // progress indicator is on screen it is extended (up to the hard limit).
  var VERIFY_MS = 8000 + Math.min(12000, Math.round(totalBytes / 1e6 * 1500));
  var iv = setInterval(function(){
    if (!current()) { clearInterval(iv); return; }
    if ((window.__ktBid && window.__ktBid !== BID) || window.__ktFillRun === '__ktCancelled') { clearInterval(iv); window.__ktHoldAttach = false; closeMenu(); return; }
    var now = Date.now();
    if (now - t0 > CEILING_MS) { clearInterval(iv); fail('ceiling of ' + CEILING_MS + ' ms reached'); return; }
    if (phase === 'ready') {
      c = composer();
      if (!c) { if (now - t0 > 30000) { clearInterval(iv); fail('no composer on the page'); } return; }
      if (!seenAt) seenAt = now;
      input = fileInput();
      var shell = c.tagName === 'TEXTAREA';
      var waited = now - seenAt, limit = shell ? READY_SHELL_MS : READY_RICH_MS;
      // Ready: the page offers a file input and the composer is not a just-seen shell; or the grace is over.
      if (!(input && !(shell && waited < 1500)) && waited < limit) return;
      phase = 'hand';
    }
    if (phase === 'menuwait') {
      input = fileInput();
      if (!input && now < menuUntil) return;
      phase = 'hand';
    }
    if (phase === 'hand') {
      c = composer() || c; input = fileInput();
      if (!input && !menuTried) {
        menuTried = true;
        menuBtn = menuButton(c);
        if (menuBtn) {
          try { menuBtn.click(); menuUntil = now + 2500; phase = 'menuwait'; diag('opened the upload menu to get a file input'); return; } catch(e){}
        }
      }
      attempt++;
      var routes = input ? ['input', 'paste', 'drop', 'input'] : ['paste', 'drop', 'paste', 'drop'];
      how = routes[Math.min(attempt, routes.length) - 1];
      if (how === 'input' && !input) how = 'paste';
      before = snapshot(c);
      var dt = new DataTransfer();
      toFiles().forEach(function(f){ dt.items.add(f); });
      try { hand(how, c, input, dt); } catch(e){ diag('attempt ' + attempt + ' via ' + how + ' threw ' + e); }
      diag('attempt ' + attempt + ' files=' + files.length + ' via ' + how + ' composer=' + c.tagName + ' input=' + (!!input) + ' media=' + before.srcN + ' wait=' + VERIFY_MS);
      verifyUntil = Date.now() + VERIFY_MS;
      hardUntil = Date.now() + VERIFY_MS + 15000;
      phase = 'verify';
      return;
    }
    if (phase === 'verify') {
      var cc = composer() || c;
      var snap = snapshot(cc, before && before.scope);
      var why = gained(before, snap);
      if (why) {
        clearInterval(iv);
        closeMenu();
        window.__ktFilesDone = BID;
        diag('files=' + files.length + ' via ' + how + ' verified (' + why + ', attempt ' + attempt + ', ' + (Date.now() - t0) + ' ms)');
        // The preview is on screen, so the upload has started: a short pause, then the text goes in.
        release(1200 + 600 * files.length);
        return;
      }
      if (Date.now() < verifyUntil) return;
      // Still climbing (a progress indicator the region did not have before): do not hand the files over again.
      if (snap.progress > before.progress && Date.now() < hardUntil) return;
      diag('attempt ' + attempt + ' via ' + how + ' not verified (media ' + snap.srcN + ', was ' + before.srcN + ')');
      if (attempt >= MAX_ATTEMPTS) { clearInterval(iv); fail('no evidence of the file in the composer after ' + attempt + ' attempts'); return; }
      phase = 'hand';
    }
  }, 400);
})();
"##;

/// The sites where a provider page can deliver results: exactly the hosts `capabilities/provider-push.json` lets
/// call `kotodama_push` (a test keeps the two lists equal). Anywhere else -- a sign-in page on accounts.google.com
/// or accounts.x.ai -- the IPC call is refused and the script falls back to its navigation sentinel, and every
/// such navigation cancels the page's own. Measured symptom (15/09/2026): signing in with Google on Gemini or
/// Grok while a message was waiting loaded forever, because the waiting send/harvest was injected into the
/// Google page. So Kotodama's scripts run only on these hosts; a send waits until the page is back.
const PROVIDER_HOSTS: &[&str] = &[
    "chatgpt.com",
    "claude.ai",
    "grok.com",
    "gemini.google.com",
    "www.perplexity.ai",
    "chat.qwen.ai",
    "chat.deepseek.com",
    "chat.z.ai",
    "chat.mistral.ai",
    "poe.com",
    "www.kimi.com",
    "www.meta.ai",
    "copilot.com",
    "www.copilot.com",
];
fn on_provider_site(url: Option<&Url>) -> bool {
    url.and_then(|u| u.host_str()).map(|h| PROVIDER_HOSTS.contains(&h)).unwrap_or(false)
}

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
/// Providers whose fresh page can start showing ANOTHER conversation some time after it finished loading,
/// with no navigation Kotodama could see. Claude in incognito, measured 27/09/2026 through the WebView2
/// inspection port: `/new?incognito=` is empty right after the load (0 messages), then its own client restores
/// the last incognito conversation about 3 s later, at the same address (which never changes in incognito).
/// A "this tab is sitting on an empty page" note kept for a later send goes stale without any sign, and the next
/// conversation was typed into the old one (eight messages of four conversations in one chat). For these
/// providers a pre-warm still loads the page early, but a send always navigates for real and types right after
/// that load.
fn unreliable_prewarm(key: &str) -> bool {
    matches!(key, "anthropic")
}

/// Claude's client resumes the previous incognito conversation instead of starting one when the tab's
/// sessionStorage still holds `incognito_temporary_conversation_uuid` (per tab, it survives navigation inside the
/// same WebView2 tab, and Kotodama reuses the tab across conversations). Measured live on 2026-09-27 through the
/// inspection port, A/B on the real send path: from the third fresh `/new?incognito=` load in the same tab the
/// answers came from the old conversation; with only this key removed the next send was a new conversation,
/// with it left in place the chain came back at once. Only this key is touched: login, cookies and
/// preferences stay as they are.
fn forget_incognito_conversation(webview: &tauri::Webview, key: &str, url: &str) {
    if key == "anthropic" && url.contains("incognito=") {
        let _ = webview.eval("try{sessionStorage.removeItem('incognito_temporary_conversation_uuid');}catch(e){}");
    }
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
/// The discovery probe, only when debugging with `KOTO_NETPROBE` set; empty otherwise.
fn net_probe_js() -> &'static str {
    if crate::debug::enabled() && std::env::var("KOTO_NETPROBE").is_ok() {
        NET_PROBE_JS
    } else {
        ""
    }
}

/// Hands a streamed response back to the page after we tee'd its body, WITHOUT replacing the Response
/// object. The earlier `new Response(copy, {status, headers})` looked identical but silently lost
/// `url`, `redirected`, `type` and `ok` semantics tied to the original request, and a provider page
/// that reads `res.url` (redirect handling, routing by endpoint) could break because we watched it.
/// Here the original object is kept and only its body-related members are redirected to an inner
/// Response built on the page's half of the tee. If redefining fails, the old behaviour is the fallback.
const RESPONSE_ADOPT_JS: &str = r##"
(function(){
  if (window.__ktAdoptBody) return;
  // `onCancel` (optional): called when the page cancels ITS half of the tee. A tee only releases the connection
  // when both halves are cancelled, so our own copy kept a stream the page had given up on downloading (a long-lived
  // one, such as Claude's timeline, stayed open for good). Wrapping the page's half in a pass-through stream is how
  // the cancel is seen; it changes nothing else (chunks, close and errors travel as they are).
  window.__ktAdoptBody = function adopt(res, stream, onCancel){
    if (typeof onCancel === 'function' && typeof ReadableStream === 'function') {
      try {
        var rd = stream.getReader();
        stream = new ReadableStream({
          pull: function(c){ return rd.read().then(function(r){ if (r.done) c.close(); else c.enqueue(r.value); }); },
          cancel: function(reason){ try { onCancel(reason); } catch(e){} return rd.cancel(reason); }
        });
      } catch(e){}
    }
    var init = { status: res.status, statusText: res.statusText, headers: res.headers };
    var inner = new Response(stream, init);
    try {
      var props = {
        body: { configurable: true, get: function(){ return inner.body; } },
        bodyUsed: { configurable: true, get: function(){ return inner.bodyUsed; } },
        // A clone is a separate object: the inner clone, given the original's request-bound identity.
        clone: { configurable: true, value: function(){
          var c = inner.clone();
          try { Object.defineProperties(c, { url: { value: res.url }, redirected: { value: res.redirected }, type: { value: res.type } }); } catch(e){}
          return c;
        } }
      };
      ['text', 'json', 'arrayBuffer', 'blob', 'formData', 'bytes'].forEach(function(m){
        if (typeof inner[m] === 'function') props[m] = { configurable: true, value: function(){ return inner[m](); } };
      });
      Object.defineProperties(res, props);
      return res;
    } catch(e) {
      return inner;
    }
  };
})();
"##;

/// Claude keeps its conversation's timeline stream open BEFORE a message is sent and reconnects it on its own
/// schedule (measured 14/10/2026: the page opened a fresh one at some sends and kept the previous one at others,
/// so a second turn had no new request to watch). A wrapper installed at the time of the send would only see
/// the streams opened after it, and the answer travels on that stream (netread.rs, `ClaudeTimeline`).
/// So this tap goes in at document start, like the other initialization scripts of the provider tab: for the
/// timeline request only, it keeps a tee of the body and hands each chunk to `window.__ktTlSink` when STREAM_WATCH_JS
/// has set one (from the first injection on) -- until then the chunks are dropped. The page keeps its own
/// untouched copy of the body. Nothing is sent, altered or repeated.
const CLAUDE_TIMELINE_TAP_JS: &str = r##"
(function(){
  if (window.__ktTlTap) return;
  window.__ktTlTap = true;
  try {
    var of = window.fetch;
    if (typeof of !== 'function') return;
    var seq = 0;
    window.fetch = function(input){
      var p = of.apply(this, arguments);
      try {
        var url = ''; try { url = (input && typeof input === 'object' && 'url' in input) ? input.url : String(input || ''); } catch(e){}
        if (url.indexOf('ConversationService/StreamTimeline') < 0) return p;
        return p.then(function(res){
          try {
            if (!res || !res.body || typeof res.body.tee !== 'function' || !window.__ktAdoptBody) return res;
            var pair = res.body.tee(), mine = pair[0].getReader(), sid = ++seq;
            function tell(bytes, end){ var s = window.__ktTlSink; if (s) { try { s(sid, bytes, end, url); } catch(e){} } }
            (function pump(){
              mine.read().then(function(r){
                if (r.done) { tell(null, true); return; }
                tell(r.value, false);
                pump();
              }, function(){ tell(null, true); });
            })();
            return window.__ktAdoptBody(res, pair[1], function(){ try { mine.cancel(); } catch(e){} });
          } catch(e) { return res; }
        });
      } catch(e) { return p; }
    };
  } catch(e){}
})();
"##;

/// Script a provider's tab needs from the very first byte of its page, beyond the common ones (`create_tab`).
pub fn early_provider_js(key: &str) -> Option<String> {
    // KOTO_NO_NETREAD (debug) turns the network reading off, and with it the tap.
    if key == "anthropic" && std::env::var("KOTO_NO_NETREAD").is_err() {
        return Some(format!("{RESPONSE_ADOPT_JS}{CLAUDE_TIMELINE_TAP_JS}"));
    }
    None
}

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
  // Records go ONLY over IPC: the navigation fallback of __ktPush would reload the provider page.
  function cap(rec){
    try {
      if (!(window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)) return;
      rec.t = window.__ktSentAt ? Date.now() - window.__ktSentAt : -1;
      window.__TAURI__.core.invoke('kotodama_push', { b: __kt_bid, k: __kt_key, st: 'netcap', d: JSON.stringify(rec) }).catch(function(){});
    } catch(e){}
  }
  var seq = 0;
  // Large enough to keep whole answer streams (Perplexity sends ~100 KB snapshots): these captures become
  // the fixtures the network readers are tested against.
  var PER_REQ = 4000000, PER_CHUNK = 400000;
  function short(u){ try { return String(u).slice(0, 300); } catch(e){ return ''; } }
  // Tauri's invoke() is itself a fetch to ipc.localhost: observing it would capture every record we send,
  // again and again (measured: 16k records for one answer).
  function isIpc(u){ return /^(https?:\/\/)?ipc\.localhost|^ipc:|^tauri:/i.test(String(u || '')); }
  function text(v){
    try {
      if (typeof v === 'string') return v;
      if (v instanceof ArrayBuffer) v = new Uint8Array(v);
      if (v && v.buffer instanceof ArrayBuffer) {
        var t = new TextDecoder('utf-8', { fatal: true });
        try { return t.decode(v); } catch(e) {
          var bin = ''; var n = Math.min(v.length, 3000);
          for (var i = 0; i < n; i++) bin += String.fromCharCode(v[i]);
          return 'base64:' + btoa(bin);
        }
      }
      return String(v);
    } catch(e){ return '?'; }
  }
  // Streaming chunks are only interesting after our send; before it the page is loading itself.
  function armed(){ return !!window.__ktSentAt; }

  // fetch: tee the body so the page keeps its own untouched copy.
  try {
    var of = window.fetch;
    window.fetch = function(input, init){
      var id = ++seq;
      // input can be a string, a URL object or a Request.
      var url = ''; try { url = (input && typeof input === 'object' && 'url' in input) ? input.url : String(input || ''); } catch(e){}
      if (isIpc(url)) return of.apply(this, arguments);
      var method = (init && init.method) || (input && input.method) || 'GET';
      return of.apply(this, arguments).then(function(res){
        if (!armed()) return res;
        var ct = ''; try { ct = res.headers.get('content-type') || ''; } catch(e){}
        cap({ kind: 'fetch', id: id, ev: 'open', method: method, url: short(url), status: res.status, ct: ct });
        if (/audio|mpeg|ogg|opus|aac|wav/i.test(ct) && res.body && res.body.tee) {
          try {
            var ap = res.body.tee(), ar = ap[0].getReader(), abytes = 0, achunks = 0, at0 = Date.now();
            (function apump(){ ar.read().then(function(r){ if (r.done) { cap({ kind: 'fetch', id: id, ev: 'audio-end', ct: ct, bytes: abytes, chunks: achunks, ms: Date.now() - at0 }); return; } achunks++; abytes += r.value.length; apump(); }); })();
            return (window.__ktAdoptBody ? window.__ktAdoptBody(res, ap[1]) : res);
          } catch(e) { return res; }
        }
        if (!/event-stream|ndjson|octet-stream|stream|json|text\/plain|proto/i.test(ct) || !res.body || !res.body.tee) return res;
        try {
          var pair = res.body.tee();
          // Protobuf bodies (Claude's ConversationService) are binary: keep the exact bytes, as base64.
          var bin = /proto/i.test(ct);
          var mine = pair[0].getReader(), bytes = 0, dec = new TextDecoder();
          (function pump(){
            mine.read().then(function(r){
              if (r.done) { cap({ kind: 'fetch', id: id, ev: 'end', bytes: bytes }); return; }
              bytes += (r.value && r.value.length) || 0;
              if (bytes <= PER_REQ) {
                if (bin) { var bs = ''; for (var q = 0; q < r.value.length; q++) bs += String.fromCharCode(r.value[q]); cap({ kind: 'fetch', id: id, ev: 'chunk', b64: btoa(bs) }); }
                else cap({ kind: 'fetch', id: id, ev: 'chunk', data: dec.decode(r.value, { stream: true }).slice(0, PER_CHUNK) });
              }
              pump();
            }, function(err){ cap({ kind: 'fetch', id: id, ev: 'error', err: String(err) }); });
          })();
          return (window.__ktAdoptBody ? window.__ktAdoptBody(res, pair[1]) : new Response(pair[1], res));
        } catch(e) { cap({ kind: 'fetch', id: id, ev: 'teefail', err: String(e) }); return res; }
      });
    };
  } catch(e){}

  // XMLHttpRequest: responseText grows while loading (readyState 3); record the new part each time.
  try {
    var XO = XMLHttpRequest.prototype.open, XS = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.open = function(m, u){ try { this.__ktReq = { method: m, url: short(u) }; } catch(e){} return XO.apply(this, arguments); };
    XMLHttpRequest.prototype.send = function(){
      var xhr = this;
      try {
        if (armed() && !isIpc((xhr.__ktReq || {}).url)) {
          var id = ++seq, seen = 0, bytes = 0, opened = false;
          var rq = xhr.__ktReq || {};
          xhr.addEventListener('readystatechange', function(){
            try {
              if (xhr.readyState >= 2 && !opened) { opened = true; cap({ kind: 'xhr', id: id, ev: 'open', method: rq.method, url: rq.url, status: xhr.status, ct: xhr.getResponseHeader('content-type') || '', rtype: xhr.responseType }); }
              if ((xhr.readyState === 3 || xhr.readyState === 4) && (xhr.responseType === '' || xhr.responseType === 'text')) {
                var all = xhr.responseText || '';
                if (all.length > seen) { var part = all.slice(seen); seen = all.length; bytes += part.length; if (bytes <= PER_REQ) cap({ kind: 'xhr', id: id, ev: 'chunk', data: part.slice(0, PER_CHUNK) }); }
              }
              if (xhr.readyState === 4) cap({ kind: 'xhr', id: id, ev: 'end', status: xhr.status, bytes: bytes });
            } catch(e){}
          });
        }
      } catch(e){}
      return XS.apply(this, arguments);
    };
  } catch(e){}

  // WebSocket: every frame after our send, both directions' existence (sent frames only by size).
  try {
    var OWS = window.WebSocket;
    if (OWS) {
      var W = function(u, pr){
        var ws = (pr === undefined) ? new OWS(u) : new OWS(u, pr);
        var id = ++seq, bytes = 0;
        try {
          cap({ kind: 'ws', id: id, ev: 'open', url: short(u) });
          ws.addEventListener('message', function(ev){
            if (!armed()) return;
            var d = ev.data;
            if (typeof Blob !== 'undefined' && d instanceof Blob) {
              d.arrayBuffer().then(function(b){ bytes += b.byteLength; if (bytes <= PER_REQ) cap({ kind: 'ws', id: id, ev: 'chunk', data: text(b).slice(0, PER_CHUNK) }); });
              return;
            }
            var tx = text(d); bytes += tx.length;
            if (bytes <= PER_REQ) cap({ kind: 'ws', id: id, ev: 'chunk', data: tx.slice(0, PER_CHUNK) });
          });
          ws.addEventListener('close', function(ev){ if (armed()) cap({ kind: 'ws', id: id, ev: 'end', code: ev.code, bytes: bytes }); });
          var osend = ws.send;
          ws.send = function(x){ try { if (armed()) cap({ kind: 'ws', id: id, ev: 'sent', data: text(x).slice(0, 2000) }); } catch(e){} return osend.apply(ws, arguments); };
        } catch(e){}
        return ws;
      };
      W.prototype = OWS.prototype;
      ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED'].forEach(function(k){ try { W[k] = OWS[k]; } catch(e){} });
      window.WebSocket = W;
    }
  } catch(e){}

  // EventSource.
  try {
    var OES = window.EventSource;
    if (OES) {
      var E = function(u, c){
        var es = new OES(u, c), id = ++seq;
        try {
          cap({ kind: 'es', id: id, ev: 'open', url: short(u) });
          es.addEventListener('message', function(ev){ if (armed()) cap({ kind: 'es', id: id, ev: 'chunk', data: String(ev.data).slice(0, PER_CHUNK) }); });
          es.addEventListener('error', function(){ if (armed()) cap({ kind: 'es', id: id, ev: 'end' }); });
        } catch(e){}
        return es;
      };
      E.prototype = OES.prototype;
      window.EventSource = E;
    }
  } catch(e){}

  // Workers: a stream opened inside a worker is invisible to the hooks above; at least record that
  // the page started one after our send, so a provider with no captured stream can be explained.
  try {
    ['Worker', 'SharedWorker'].forEach(function(name){
      var O = window[name]; if (!O) return;
      var F = function(u, o){ try { if (armed()) cap({ kind: 'worker', ev: 'open', type: name, url: short(u) }); } catch(e){} return (o === undefined) ? new O(u) : new O(u, o); };
      F.prototype = O.prototype;
      window[name] = F;
    });
  } catch(e){}
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
  // The answer is over when the LAST stream opened after our send has closed, not the first one. Captured
  // on Perplexity (15/09/2026): the answer stream stays open while a short related-queries stream opens and
  // closes next to it, and firing on that close harvested the page 1s into the answer ("2:02 AM").
  // A stream belongs to the injection that was current when it OPENED (`window.__ktBid`). When a second message
  // is sent while the first answer is still streaming, the first stream closing must not end the second one: it
  // did, and the second card showed the first message's answer (measured on Gemini, 27/09/2026).
  function ended(bid){
    try {
      if (bid !== undefined && window.__ktBid !== bid) return;
      if ((window.__ktStreamOpen || 0) > 0) return;
      if (window.__ktStreamEnd) window.__ktStreamEnd();
    } catch(e){}
  }
  // ---- Network reading (netread.rs): copies of the ANSWER stream only, forwarded in small batches. The URL
  // pattern comes from Rust per provider (__ktNetUrl); every other request of the page is ignored. IPC only.
  var NET_URL = null;
  try { if (window.__ktNetUrl) NET_URL = new RegExp(window.__ktNetUrl); } catch(e){}
  var netSeq = 0, netQueue = {}, netTimer = null;
  // Stream ids carry a per-document tag: the counters restart at 1 in every document, and a reload in the middle of
  // a send made the new page's first stream take the id of the old page's one, so the reader of the old stream
  // (half a frame in its buffer) was fed the new stream's bytes (measured 15/10/2026 on Claude).
  var DOC = Math.random().toString(36).slice(2, 7);
  function netWanted(u){ return !!(NET_URL && window.__ktSentAt && NET_URL.test(String(u || ''))); }
  function netFlush(){
    netTimer = null;
    var ids = Object.keys(netQueue);
    for (var i = 0; i < ids.length; i++) {
      var q = netQueue[ids[i]]; delete netQueue[ids[i]];
      // The send the stream belongs to, fixed when it OPENED: a stopped answer's stream can keep arriving while
      // the next message is already out, and under the page-wide current id its frames were counted as the NEW
      // answer (measured 15/10/2026 on Gemini: the essay stopped with Ferma came back as the answer to the next
      // question). Rust ignores frames of a send that is no longer owed.
      try { window.__TAURI__.core.invoke('kotodama_push', { b: q.bid || __kt_bid, k: __kt_key, st: 'net', d: JSON.stringify({ id: ids[i], url: q.url, data: q.data, end: q.end }) }).catch(function(){}); } catch(e){}
    }
  }
  function netSend(id, url, data, end, bid){
    var q = netQueue[id] || (netQueue[id] = { url: String(url || ''), data: '', end: false, bid: bid || __kt_bid });
    if (data) q.data += data;
    if (end) { q.end = true; if (netTimer) clearTimeout(netTimer); netFlush(); return; }
    if (!netTimer) netTimer = setTimeout(netFlush, 60);
  }
  // Claude's timeline stream (CLAUDE_TIMELINE_TAP_JS, installed with the page): binary protobuf, so its bytes
  // travel as base64, ONE LINE PER CHUNK (a padded base64 string cannot be glued to the next one), and the stream
  // is not a "streaming response" for the counters above -- it stays open between answers, so its closing means
  // nothing about ours. Only what arrives after our send is forwarded (netWanted).
  function b64(u8){
    var s = '';
    for (var i = 0; i < u8.length; i += 0x8000) s += String.fromCharCode.apply(null, u8.subarray(i, i + 0x8000));
    return btoa(s);
  }
  window.__ktTlSink = function(sid, bytes, end, url){
    try { if (netWanted(url)) netSend('t' + DOC + sid, url, bytes ? b64(bytes) + '\n' : '', !!end); } catch(e){}
  };
  try {
    var of = window.fetch;
    if (typeof of !== 'function') return;
    window.fetch = function(input){
      var p = of.apply(this, arguments);
      var arguments_ = arguments;
      try {
        if (!window.__ktSentAt) return p;              // nothing sent yet: not our stream
        var reqUrl = ''; try { reqUrl = (input && typeof input === 'object' && 'url' in input) ? input.url : String(input || ''); } catch(e){}
        try { noteCaptcha((input && typeof input === 'object' && 'url' in input) ? input.url : input); } catch(e){}
        return p.then(function(res){
          try {
            // The page's own API refused the session (captured on Kimi signed out: 401 on its member
            // endpoints). One of the signals that make a visible login control mean "signed out".
            if (res.status === 401) { try { window.__ktAuthFail = Date.now(); } catch(e){} }
            // 429 after our send: the provider's own rate limit refused the message (captured on Mistral: "Messages
            // limit reached" over the composer). A limit, not a window to close and not a sign-in.
            // Only a request that carries data can be the refusal of our message. Measured 14/10/2026 on ChatGPT: its
            // history list answered 429 right after the send ("Unable to load history"), the card closed as a limit in
            // 2 s, and the answer then arrived on the page.
            var reqMethod = String((arguments_ && arguments_[1] && arguments_[1].method) || (input && typeof input === 'object' && input.method) || 'GET').toUpperCase();
            if (res.status === 429 && reqMethod !== 'GET' && reqMethod !== 'HEAD') { try { window.__ktRateLimited = Date.now(); } catch(e){} }
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
            var sb = window.__ktBid;
            try { window.__ktStreamOpen = (window.__ktStreamOpen || 0) + 1; } catch(e){}
            try { window.__ktStreamEver = (window.__ktStreamEver || 0) + 1; } catch(e){}
            var fwd = netWanted(reqUrl), nid = fwd ? 'f' + DOC + (++netSeq) : '', dec = fwd ? new TextDecoder() : null, nbid = __kt_bid;
            // The answer stream of a provider we read from the network, counted apart: the harvest must not call a
            // silent page "finished" while it is open (see `netOpen` in the harvest).
            if (fwd) { try { window.__ktNetOpen = (window.__ktNetOpen || 0) + 1; } catch(e){} }
            function closed(){ try { window.__ktStreamOpen = Math.max(0, (window.__ktStreamOpen || 1) - 1); } catch(e){} if (fwd) { try { window.__ktNetOpen = Math.max(0, (window.__ktNetOpen || 1) - 1); } catch(e){} try { netSend(nid, reqUrl, dec.decode(), true, nbid); } catch(e){} } ended(sb); }
            (function pump(){
              mine.read().then(function(r){
                if (r.done) { closed(); return; }
                if (fwd) { try { netSend(nid, reqUrl, dec.decode(r.value, { stream: true }), false, nbid); } catch(e){} }
                pump();
              }, function(){ closed(); });
            })();
            return window.__ktAdoptBody(res, pair[1], function(){ try { mine.cancel(); } catch(e){} });
          } catch(e) { return res; }
        });
      } catch(e) { return p; }
    };
  } catch(e){}
  // Same counters and end signal for the transports fetch does not cover (captured live, 15/09/2026):
  // DeepSeek streams its answer over XMLHttpRequest (text/event-stream), Gemini over XMLHttpRequest with
  // a JSON content type on StreamGenerate, Grok over a WebSocket opened with the page. Without these the
  // three providers could only conclude by DOM stability, seconds after the answer was complete.
  // opened() returns the injection the stream belongs to; closedOne() takes it back (see ended()).
  function opened(){
    try { window.__ktStreamOpen = (window.__ktStreamOpen || 0) + 1; } catch(e){}
    try { window.__ktStreamEver = (window.__ktStreamEver || 0) + 1; } catch(e){}
    return window.__ktBid;
  }
  function closedOne(bid){ try { window.__ktStreamOpen = Math.max(0, (window.__ktStreamOpen || 1) - 1); } catch(e){} ended(bid); }
  // Streaming endpoints whose content type does not say "stream". Kept to exact answer endpoints: a
  // generic JSON match would treat every background call as the end of the answer.
  var STREAM_URL = /\/StreamGenerate\b/;
  // A human-check service contacted after our send (captured on Z.ai: its puzzle loads over XHR from
  // *.captcha-*.aliyuncs.com before the message is allowed out). Recorded, not acted on here: the harvest
  // decides, and only when no answer and no response stream came.
  var CAPTCHA_URL = /captcha|turnstile|arkoselabs|funcaptcha|hcaptcha/i;
  function noteCaptcha(u){ try { if (window.__ktSentAt && CAPTCHA_URL.test(String(u || ''))) window.__ktCaptchaAt = window.__ktCaptchaAt || Date.now(); } catch(e){} }
  try {
    var XO = XMLHttpRequest.prototype.open, XS = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.open = function(m, u){ try { this.__ktUrl = String(u || ''); this.__ktMethod = String(m || 'GET').toUpperCase(); } catch(e){} return XO.apply(this, arguments); };
    XMLHttpRequest.prototype.send = function(){
      var xhr = this;
      try {
        noteCaptcha(xhr.__ktUrl);
        if (window.__ktSentAt) {
          var counted = false, xfwd = netWanted(xhr.__ktUrl), xid = xfwd ? 'x' + DOC + (++netSeq) : '', xseen = 0, xbid = __kt_bid;
          xhr.addEventListener('readystatechange', function(){
            try {
              if (xfwd && xhr.readyState >= 3 && (xhr.responseType === '' || xhr.responseType === 'text')) {
                var all = xhr.responseText || '';
                if (all.length > xseen || xhr.readyState === 4) { var part = all.slice(xseen); xseen = all.length; netSend(xid, xhr.__ktUrl, part, xhr.readyState === 4, xbid); }
              }
              if (xhr.readyState === 2 && xhr.status === 401) { try { window.__ktAuthFail = Date.now(); } catch(e){} }
              if (xhr.readyState === 2 && xhr.status === 429 && xhr.__ktMethod !== 'GET' && xhr.__ktMethod !== 'HEAD') { try { window.__ktRateLimited = Date.now(); } catch(e){} }
              if (xhr.readyState === 2 && !counted) {
                var ct = xhr.getResponseHeader('content-type') || '';
                if (STREAMY.test(ct) || STREAM_URL.test(xhr.__ktUrl || '')) { counted = true; xhr.__ktOpenBid = opened(); }
              }
              if (xhr.readyState === 4 && counted) { counted = false; closedOne(xhr.__ktOpenBid); }
            } catch(e){}
          });
        }
      } catch(e){}
      return XS.apply(this, arguments);
    };
  } catch(e){}
  try {
    var OWS = window.WebSocket;
    if (OWS) {
      var W = function(u, pr){
        var ws = (pr === undefined) ? new OWS(u) : new OWS(u, pr);
        try {
          // Message-level protocol markers of a response lifecycle on a long-lived socket (Grok): the socket
          // itself stays open between answers, so only these events say "started" and "finished".
          var wid = 'w' + DOC + (++netSeq);
          ws.addEventListener('message', function(ev){
            try {
              if (!window.__ktSentAt || typeof ev.data !== 'string') return;
              if (netWanted(u)) netSend(wid, u, ev.data, false);
              // The provider's own limit signal inside the stream (Grok, measured 15/10/2026: `"stream_error":{"kind":
              // "usage_limit_reached"`, then `response.done` with status "incomplete"). A protocol constant, not a
              // visible text. Unlike a 429 it arrives AFTER a stream opened, so it gets its own flag.
              if (ev.data.indexOf('usage_limit_reached') >= 0) { try { window.__ktLimitHit = Date.now(); } catch(e){} }
              if (ev.data.indexOf('"type":"response.created"') >= 0) ws.__ktOpenBid = opened();
              if (ev.data.indexOf('"type":"response.done"') >= 0) closedOne(ws.__ktOpenBid);
            } catch(e){}
          });
        } catch(e){}
        return ws;
      };
      W.prototype = OWS.prototype;
      ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED'].forEach(function(k){ try { W[k] = OWS[k]; } catch(e){} });
      window.WebSocket = W;
    }
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

/// Finds a provider's action control under the LAST answer: the last match of a CSS selector (resolved to its
/// button), or the button whose icon path starts with a given prefix. Structure only, never the visible label.
const FIND_ACTION_JS: &str = r##"
if (!window.__ktFindAction) {
  // One alternative of the selector: see below.
  window.__ktFindOne = function(sel){
    // "row-last:<selector>": the LAST button of the row that holds that element -- for a control with no stable
    // attribute of its own sitting at the end of an action bar (ChatGPT's former "more actions").
    if (sel.indexOf('row-last:') === 0) {
      var anchor = document.querySelectorAll(sel.slice(9));
      if (!anchor.length) return null;
      var row = anchor[anchor.length - 1].closest('button, [role="button"]');
      row = row && row.parentElement;
      if (!row) return null;
      var bs = row.querySelectorAll('button, [role="button"]');
      return bs.length ? bs[bs.length - 1] : null;
    }
    var l = document.querySelectorAll(sel);
    return l.length ? l[l.length - 1].closest('button, [role="button"]') : null;
  };
  // `sel` may list alternatives separated by " || ", tried in order: a provider's page changes under us and the
  // old and the new layout are both in use for a while (ChatGPT, October 2026: the speaker went from a menu item
  // to a button of its own, and its data-testid attributes disappeared).
  window.__ktFindAction = function(sel, path){
    try {
      if (sel) {
        var alts = sel.split(' || ');
        for (var k = 0; k < alts.length; k++) { var f = window.__ktFindOne(alts[k]); if (f) return f; }
        return null;
      }
      if (path) {
        var ps = document.querySelectorAll('svg path');
        for (var i = ps.length - 1; i >= 0; i--) {
          if ((ps[i].getAttribute('d') || '').indexOf(path) === 0) return ps[i].closest('button, [role="button"]');
        }
      }
    } catch(e){}
    return null;
  };
}
"##;

/// Regenerate in the provider page, after HARVEST_JS has taken its snapshot: marks the send instant for the stream
/// watcher and presses the button (then the menu item, when the button opens a menu). No button or no item: the
/// harvest is ended with an error at once instead of waiting out its budget.
const REGEN_CLICK_JS: &str = r##"
(function(){
  var BID = __kt_bid, KEY = __kt_key;
  function fail(why){
    try { if (window.__ktDiag) window.__ktPush({ b: BID, k: KEY, st: 'diag', d: 'REGEN ' + why }); } catch(e){}
    window.__ktBid = null;
    window.__ktPush({ b: BID, k: KEY, st: 'error', s: 0, n: 1, d: '' });
  }
  var b = window.__ktFindAction(__kt_regen_sel, __kt_regen_path);
  if (!b) { fail('no button'); return; }
  if (!__kt_regen_item) { try { window.__ktSentAt = Date.now(); } catch(e){} b.click(); return; }
  b.click();
  var t0 = Date.now();
  (function pick(){
    var it = null;
    try { var l = document.querySelectorAll(__kt_regen_item); it = l.length ? l[l.length - 1] : null; } catch(e){}
    if (!it) { if (Date.now() - t0 < 3000) { setTimeout(pick, 100); } else { fail('no menu item'); } return; }
    // The item's own button: a click on a wrapper element does not reach the handler inside it.
    var host = it.closest('[role="menuitem"], button') || it.parentElement;
    var target = (host && host.tagName !== 'BUTTON' && host.querySelector('button')) || host || it;
    try { window.__ktSentAt = Date.now(); } catch(e){}
    target.click();
  })();
})();
"##;

/// Read aloud in the provider page. Installs (once) a WebSocket hook that forwards the binary frames of the NEXT
/// speech socket (URL pattern from Rust) after our click, then presses the provider's own read-aloud button.
/// Only sockets opened within 15 s of our click are taken: a page's own later playback is not recorded.
/// Frames go base64 in batches every 200 ms, in arrival order (Blob reads are chained), over IPC only.
const TTS_JS: &str = r##"
(function(){
  var KEY = __kt_key, BID = __kt_bid, REQ = __kt_req;
  function send(obj){
    try { window.__TAURI__.core.invoke('kotodama_push', { b: BID, k: KEY, st: 'audio', d: JSON.stringify(obj) }).catch(function(){}); } catch(e){}
  }
  function findButton(){ return window.__ktFindAction(__kt_tts_sel, __kt_tts_path); }
  // Providers that answer with a finished audio file (ChatGPT): copy the response body, the page keeps its own.
  if (!window.__ktTtsFetchHook && window.fetch) {
    window.__ktTtsFetchHook = true;
    var of = window.fetch;
    window.fetch = function(input){
      var p = of.apply(this, arguments);
      try {
        var u = ''; try { u = (input && typeof input === 'object' && 'url' in input) ? input.url : String(input || ''); } catch(e){}
        var t = window.__ktTts;
        if (!t || t.claimed || Date.now() >= t.until || !t.re.test(u)) return p;
        return p.then(function(res){
          try {
            var ct = (res.headers && res.headers.get('content-type')) || '';
            if (!/^audio\//i.test(ct) || !res.body || typeof res.body.tee !== 'function') return res;
            t.claimed = true;
            var req = t.req, pair = res.body.tee(), rd = pair[0].getReader(), q = [];
            t.send({ r: req, ev: 'open' });
            (function pump(){
              rd.read().then(function(r){
                if (r.done) {
                  t.send({ r: req, ev: 'blob', ct: ct, f: q });
                  t.send({ r: req, ev: 'end', code: 1000 });
                  return;
                }
                var u8 = new Uint8Array(r.value), s = '';
                for (var i = 0; i < u8.length; i++) s += String.fromCharCode(u8[i]);
                q.push(btoa(s));
                pump();
              }, function(){ t.send({ r: req, ev: 'end', code: 1006 }); });
            })();
            return (window.__ktAdoptBody ? window.__ktAdoptBody(res, pair[1]) : res);
          } catch(e) { return res; }
        });
      } catch(e){}
      return p;
    };
  }
  if (!window.__ktTtsHook && window.WebSocket) {
    window.__ktTtsHook = true;
    var OWS = window.WebSocket;
    var W = function(u, pr){
      var ws = (pr === undefined) ? new OWS(u) : new OWS(u, pr);
      try {
        var t = window.__ktTts;
        if (t && !t.claimed && Date.now() < t.until && t.re.test(String(u || ''))) {
          t.claimed = true;
          var req = t.req, q = [], timer = null, chain = Promise.resolve();
          var flush = function(){ timer = null; if (q.length) { t.send({ r: req, ev: 'frames', f: q }); q = []; } };
          var take = function(buf){
            var u8 = new Uint8Array(buf), s = '';
            for (var i = 0; i < u8.length; i++) s += String.fromCharCode(u8[i]);
            q.push(btoa(s));
            if (!timer) timer = setTimeout(flush, 200);
          };
          t.send({ r: req, ev: 'open' });
          ws.addEventListener('message', function(ev){
            var d = ev.data;
            if (d instanceof ArrayBuffer) chain = chain.then(function(){ take(d); });
            else if (typeof Blob !== 'undefined' && d instanceof Blob) chain = chain.then(function(){ return d.arrayBuffer().then(take); });
          });
          ws.addEventListener('close', function(ev){
            chain = chain.then(function(){ if (timer) clearTimeout(timer); flush(); t.send({ r: req, ev: 'end', code: ev.code }); });
          });
        }
      } catch(e){}
      return ws;
    };
    W.prototype = OWS.prototype;
    ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED'].forEach(function(k){ try { W[k] = OWS[k]; } catch(e){} });
    window.WebSocket = W;
  }
  var b = findButton();
  if (__kt_mode === 'stop') {
    if (b) b.click();
    send({ r: REQ, ev: 'stopped' });
    return;
  }
  if (!b) { send({ r: REQ, ev: 'nobutton' }); return; }
  var t = { req: REQ, re: new RegExp(__kt_tts_url), until: Date.now() + 15000, claimed: false, send: send };
  window.__ktTts = t;
  b.click();
  setTimeout(function(){ if (!t.claimed) send({ r: REQ, ev: 'noaudio' }); }, 15000);
  // DISCOVERY (debug only): providers whose read-aloud lives in a "more actions" menu -- describe what the click opened.
  if (window.__ktDiag) setTimeout(function(){
    try {
      var its = document.querySelectorAll('[role="menuitem"], [mat-menu-item], .mat-mdc-menu-item'), out = [];
      for (var i = 0; i < its.length; i++) {
        var it = its[i];
        out.push('testid=' + (it.getAttribute('data-testid') || it.getAttribute('data-test-id') || '-') + ' text=' + (it.innerText || '').trim().slice(0, 20).replace(/\s+/g, ' '));
      }
      if (out.length) window.__ktPush({ b: BID, k: KEY, st: 'diag', d: ('TTS-MENU n=' + out.length + ' ' + out.join(' || ')).slice(0, 1500) });
    } catch(e){}
  }, 800);
  // A toggle that reads aloud by itself (ChatGPT, October 2026: `aria-pressed`, no menu) has no item to pick; any
  // other control may open the menu that holds it (the former "more actions").
  if (!__kt_tts_item || (b.hasAttribute('aria-pressed') && !b.getAttribute('aria-haspopup'))) return;
  // The control opened a menu: the read-aloud is one of its items.
  var t0 = Date.now();
  (function pick(){
    var it = null;
    try { var l = document.querySelectorAll(__kt_tts_item); it = l.length ? l[l.length - 1] : null; } catch(e){}
    if (!it) { if (Date.now() - t0 < 3000) { setTimeout(pick, 100); } else { send({ r: REQ, ev: 'nobutton' }); } return; }
    var host = it.closest('[role="menuitem"], button') || it;
    ((host.tagName !== 'BUTTON' && host.querySelector('button')) || host).click();
  })();
})();
"##;

const HARVEST_JS: &str = r##"
(function(){
  var BID = __kt_bid, KEY = __kt_key, ANS_SEL = __kt_ans, BUSY_SEL = __kt_busy, FRESH = __kt_fresh;
  var FAST_DONE = __kt_fast;   // trust the provider's own "generating" marker, see below
  // Set by Rust on a re-injection after the page navigated: this message is already out.
  var KNOWN_SENT = (typeof __kt_sent !== 'undefined') && !!__kt_sent;
  // Set by kotodama_regenerate, consumed here: the global outlives this script, a later send must not inherit it.
  var REGEN = (typeof __kt_regen !== 'undefined') && !!__kt_regen;
  try { __kt_regen = false; } catch(e){}
  // The prompt we just sent (from the fill script): never harvest our own message back
  // (the generic selector chain can match the USER bubble on providers without a
  // dedicated assistant selector).
  var SENT = (typeof __apb_text === 'string') ? __apb_text.trim() : '';
  // Our own message, however the page lays it out: the bubble can differ from what was sent in spaces and
  // line breaks (measured on ChatGPT, 27/09/2026: a conversation reopened from the history sent a long text
  // with the earlier turns, its bubble matched a generic selector, and the exact comparison let it through as
  // the "answer"). Compared with whitespace collapsed. Narrow on purpose: an answer that repeats our text and
  // then adds something real is an answer (a cleanup rule must never empty one).
  // written by character code, like the line breaks elsewhere in this script (String.fromCharCode(10))
  function squash(x){
    x = String(x || ''); var o = '';
    for (var i = 0; i < x.length; i++){ var c = x.charCodeAt(i); o += (c <= 32 || c === 160) ? ' ' : x.charAt(i); }
    return o.split(' ').filter(Boolean).join(' ');
  }
  var SENT_SQ = squash(SENT);
  // A message-row clock ("7:14 PM") printed BEFORE or after the text: Perplexity, signed out and refused, shows only
  // our own question under its time, and "7:14 PM <question>" was delivered as the answer (reported 14/10/2026).
  // Same digits-and-colon rule as the trailing-timestamp cleanup, so it holds in every language.
  var CLOCK_RX = /\d{1,2}:\d{2}(:\d{2})?\s*(am|pm)?/gi;
  function isOwnText(t){
    if (!SENT_SQ) return false;
    var q = squash(t);
    if (q === SENT_SQ) return true;
    var nq = squash(q.replace(CLOCK_RX, ' '));
    if (nq === SENT_SQ) return true;
    // our whole text plus a few interface words around it (edit, copy labels)
    return q.length <= SENT_SQ.length + 30 && q.slice(0, SENT_SQ.length) === SENT_SQ;
  }
  window.__ktBid = BID;               // a newer injection overwrites; older loops self-terminate
  var t0 = Date.now();
  // Only RENDERED matches: a node inside a closed or display:none ancestor still matches querySelectorAll, and
  // its own computed display does not change, so it could win over the real answer. Measured on ChatGPT's
  // logged-out mobile page (27/09/2026, inspection port): a closed <dialog> (display:none) holds an <article> with
  // an "Interactive content / Details could not be loaded" placeholder; the generic chain took it as the answer
  // and Ctrl+Alt+C pasted it over the user's selection. Such a node has no client rects at all. The SIZE is not
  // the test: a provider tab can measure 0x0 while Kotodama is minimized, and Claude's answer node is
  // display:contents (always a zero box, never any rects), so it is judged by its parent instead.
  function isPainted(el){
    try {
      var cs = getComputedStyle(el);
      if (cs.visibility === 'hidden' || cs.opacity === '0') return false;
      if (cs.display === 'contents') return !el.parentElement || isPainted(el.parentElement);
      return el.getClientRects().length > 0;
    } catch(e){ return false; }
  }
  function lastMatch(sel, outermost){
    if (!sel) return null;
    try {
      var all = document.querySelectorAll(sel), els = [];
      for (var k = 0; k < all.length; k++) { if (isPainted(all[k])) els.push(all[k]); }
      if (!outermost) return els.length ? els[els.length-1] : null;
      // The last match that is not INSIDE another match: a provider's answer selector can also match pieces
      // of that answer. Measured on Mistral: `[class*="markdown"]` also matched each table cell, and the last
      // cell ("Dato B") was delivered as the whole answer. Only for the provider's own verified selector:
      // on the generic fallbacks below an outer match can be the whole conversation list.
      for (var i = els.length - 1; i >= 0; i--) {
        var up = els[i].parentElement;
        if (!up || !up.closest(sel)) return els[i];
      }
      return els.length ? els[els.length-1] : null;
    }
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
      // Rejected whenever the candidate and the real composer are the SAME element, or one sits
      // inside the other's subtree -- never trimmed/split, the whole candidate is thrown away.
      // `composerEl.contains(el)` catches a fallback selector that matched something INSIDE the
      // composer itself (e.g. its own placeholder span). `el.contains(composerEl)` is the mirror
      // case, a candidate that wraps the composer. Both are about the composer ELEMENT precisely,
      // never a wrapper found by `closest()` on an ancestor class/role -- that broader form was
      // tried and reverted (see the note above): a real answer bubble commonly lives inside the
      // SAME outer `<form>` as the input box (observed on ChatGPT), so rejecting anything under
      // that shared wrapper throws real answers away too. Neither containment check ever fires
      // for ChatGPT's writing-block/canvas: `findComposerEl` already excludes anything inside
      // `[data-testid="chatgpt-writing-block"]` from the composer search, so `composerEl` is
      // never part of that subtree and `el.contains(composerEl)` is false there (checked live
      // 27/09/2026 on both writing-block variants, plus Claude and Gemini's real answers) -- the
      // writing-block's OWN chrome is handled separately, in `CHROME_SEL` below.
      if (composerEl && (el === composerEl || el.contains(composerEl) || composerEl.contains(el))) return true;
      return el.matches('form, [role="textbox"], [class*="query-bar" i]');
    } catch(e){ return false; }
  }
  // Claude: its selector is verified and the generic chain below is worse than nothing there. While the model
  // reasons there is no reply block yet, and `[class*="assistant" i]` then matched the chat list in the sidebar:
  // "ora" / "1 minuto fa" was delivered as the answer (measured 01/10 and 05/10/2026, voce 59).
  // Gemini too (14/10/2026, signed out): with the page still showing its welcome ("Meet Gemini, your personal AI
  // assistant") the generic chain matched `[class*="assistant" i]` (the welcome's own container) and the harvest
  // delivered the greeting by stability while the fill was still held. Its own selector (`message-content`) is verified.
  var STRICT_ANS = KEY === 'anthropic' || KEY === 'gemini';
  function getAnswerEl(){
    if (STRICT_ANS) { var s = lastMatch(ANS_SEL, true); return isInputArea(s) ? null : s; }
    var el = lastMatch(ANS_SEL, true)
        || lastMatch('[data-message-author-role="assistant"]')
        || lastMatch('[class*="assistant" i]')
        || lastMatch('[class*="answer" i]')
        || lastMatch('[class*="response" i]')
        || lastMatch('.markdown, .prose')
        || lastMatch('article')
        || lastMatch('[class*="bubble" i]');
    return isInputArea(el) ? null : el;
  }
  // Interactive chrome a provider renders INSIDE the answer container: toolbars, buttons and their
  // labels are interface, never content. Measured on ChatGPT, which wraps some answers in a
  // "writing block" surface carrying its own controls -- the "Edit" label was landing at the head of
  // every inline-transform result the user pasted back. Matched on STRUCTURE (tag/role), never on
  // the label text, which changes with the interface language.
  // Claude's reasoning step is not content either: `.font-claude-response` nests a `[data-testid="TurnStatus"]`
  // pill ahead of the real text (data-step-key="thinking-0"...) with a free-form caption of the step, plus an
  // sr-only copy of it. The text match on "Thought for..." in sanitizeAnswer cannot recognise such a caption in
  // every language, so it landed at the head of the answer, doubled. Read live on 27/09/2026 through the
  // WebView2 inspection port; excluding the pill removes both copies.
  // ChatGPT's "writing block" canvas carries its OWN title/toolbar bar in a `<header>`, a sibling of the
  // body editor inside the same answer container -- e.g. the document's title ("Nuovo progetto") or, for
  // the email variant, "Email" plus an in-header "Connect email" button. The button was already excluded
  // above; the plain title text next to it was not, and rode along at the head of the answer (measured
  // live 27/09/2026, both variants, WebView2 inspection port: ANSWER-SHAPE logged PRIMO-TESTO under a
  // `header.flex.min-w-0.select-none` ancestor that is itself inside `[data-testid="chatgpt-writing-block"]`).
  // Scoped to that testid, not a bare `header`: this selector is shared by every provider via CHROME_SEL,
  // and a bare tag name would exclude any `<header>` ANY provider might render as real content -- unverified
  // for providers other than ChatGPT. `matches()` accepts a descendant-combinator selector like this one:
  // it checks the element's real ancestry in the document, not just a scope rooted at the element itself.
  // Grok nests its reasoning summary (`.thinking-container`, a "Working for 1s" label while it works) INSIDE the
  // assistant bubble, ahead of the text. It is reasoning, never the answer: left in, the label came out in front
  // of the reply and made the page text look longer than the network text (TODO 49). Scoped to the bubble's own
  // testid like the writing block above, and matched by structure, never by the label (it is localised).
  var CHROME_SEL = 'button, [role="button"], [role="toolbar"], [role="menu"], [role="menuitem"],'
    + ' [role="tab"], [role="tablist"], select, input, textarea, [data-testid="TurnStatus"],'
    + ' [data-testid="chatgpt-writing-block"] header, [data-testid="assistant-message"] .thinking-container';
  // Is this node a control whose text is NOT part of the answer? A plain control carries one label ("Copy",
  // "Monthly", "Reset"). A generated interface also builds its cards and section headers out of pressable
  // boxes (measured 14/10/2026 on ChatGPT: plan cards with name, price and size; checklist area headers with
  // their "0 / 5" counter): two or more separate blocks of text inside one control is content, and is kept.
  // Structure only, no label is read.
  var CONTROL_SEL = 'button, [role="button"], [role="tab"], [role="menuitem"]';
  function isChrome(n){
    try {
      if (!n.matches(CHROME_SEL)) return false;
      if (!n.matches(CONTROL_SEL)) return true;
      var blocks = n.querySelectorAll('p,div,h1,h2,h3,h4,h5,h6,li'), withText = 0;
      for (var i = 0; i < blocks.length && withText < 2; i++) {
        if (blocks[i].querySelector('p,div,h1,h2,h3,h4,h5,h6,li')) continue;   // a leaf block only
        if ((blocks[i].textContent || '').trim()) withText++;
      }
      return withText < 2;
    } catch(e){ return false; }
  }
  // Reads the answer's text while SKIPPING that chrome. Hiding the controls and re-reading
  // innerText does not work -- measured: with three controls hidden the string came back byte for
  // byte identical, because innerText serves a value the engine had already computed. So the text
  // is walked here instead: text nodes are collected, controls and anything computed-hidden are
  // stepped over, and block-level elements insert the line breaks innerText would have produced.
  // Used at DELIVERY only; the polling loop keeps the cheap innerText, which just has to be stable.
  var BLOCK_TAGS = ' p div li tr h1 h2 h3 h4 h5 h6 blockquote pre section article header footer ul ol table figure ';
  function cleanAnswerText(el){
    if (!el) return '';
    // Built from char codes on purpose: this JS is embedded in Rust and passed through more than
    // one layer of quoting, and a backslash escape here has already come out the other side as a
    // REAL line break, splitting the string literals and killing the whole harvest script.
    var LF = String.fromCharCode(10), TAB = String.fromCharCode(9);
    var out = '';
    function nl(){ if (out && out.slice(-1) !== LF) out += LF; }
    function walk(node){
      for (var i = 0; i < node.childNodes.length; i++) {
        var c = node.childNodes[i];
        if (c.nodeType === 3) { out += c.textContent; continue; }
        if (c.nodeType !== 1) continue;
        if (isChrome(c)) continue;
        var cs = null;
        try { cs = window.getComputedStyle(c); } catch(e){}
        if (cs && (cs.display === 'none' || cs.visibility === 'hidden')) continue;
        var tag = c.tagName.toLowerCase();
        if (tag === 'br') { out += LF; continue; }
        var isBlock = BLOCK_TAGS.indexOf(' ' + tag + ' ') !== -1
          || (cs && (cs.display === 'block' || cs.display === 'flex' || cs.display === 'grid'
                     || cs.display === 'list-item' || cs.display === 'table'));
        if (isBlock) nl();
        walk(c);
        if (isBlock) nl();
        // table cells on one row: a tab between them, as a copy from the page would give (they came out glued
        // together, "StileFrasePunti di forza", in the text used for previews, copies and inline results)
        if ((tag === 'td' || tag === 'th') && c.nextElementSibling) out += TAB;
      }
    }
    try { walk(el); } catch(e){ return ''; }
    var trailing = new RegExp('[ ' + TAB + ']+' + LF, 'g');
    var runs = new RegExp(LF + '{3,}', 'g');
    return out.replace(trailing, LF).replace(runs, LF + LF).trim();
  }
  // Every cleanup the harvested text needs, in ONE place. It must be applied to whatever string is
  // handed over, not just to the innerText path: delivering the DOM-walked text while these lived
  // only in answerTxt() let a provider's row timestamp back into the answer (measured on Poe:
  // "OK" came out as "OK<newline>1:28 PM") -- the very defect fixed earlier the same day.
  function sanitizeAnswer(t){
    t = (t || '').trim();
    // strip private-use icon glyphs (UI font icons)
    t = t.replace(new RegExp('[' + String.fromCharCode(57344) + '-' + String.fromCharCode(63743) + ']', 'g'), '').trim();
    if (t && SENT && isOwnText(t)) return '';   // that's our own message, not an answer
    // Drop a leading collapsed-thinking header ("Ha pensato per 2s" / "Thought for 2s"):
    // Claude nests it inside the answer container with no stable class to hide via CSS.
    var lines = t.split(String.fromCharCode(10));
    if (lines.length > 1 && lines[0].trim().length < 40
        && /^(ha pensato|thought|pensato|processo di ragionamento|reasoning|ragionamento|r[ée]fl[ée]ch|pens[óé]|dachte|thinking)/i.test(lines[0].trim())) {
      lines.shift();
      t = lines.join(String.fromCharCode(10)).trim();
    }
    // Trailing timestamp: some providers print it inside the message row (measured on Mistral:
    // "OK" + blank line + "1:16pm"). Stripped ONLY in the hour:minute form with optional am/pm --
    // digits and a colon, hence language-independent -- and ONLY when it sits ON ITS OWN LINE, which
    // is how a message-row timestamp is printed. An earlier version matched a trailing time anywhere
    // and ate real content: an answer that IS a time ("17:25") was erased down to nothing and the
    // harvest reported a failure for an answer sitting in plain sight. The final guard makes the
    // rule unable to empty an answer under any input.
    var noStamp = t.replace(/\n\s*\d{1,2}:\d{2}(:\d{2})?\s*(am|pm|AM|PM)?\s*$/, '').trim();
    if (noStamp) t = noStamp;
    return t;
  }
  // Our message is held back (a provider's temporary-chat toggle is being looked for before the fill may type): a
  // FRESH page has no answer yet, whatever it shows is its own welcome. The hold lasts up to ~9 s and a harvest
  // that started inside it used to count that text as stable (measured 14/10/2026, Gemini signed out).
  function heldBeforeSend(){
    return FRESH && !KNOWN_SENT && !window.__ktEnterPressed && !window.__ktStreamEver && !!(window.__ktHoldFill || window.__ktHoldAttach);
  }
  // The composer still holds exactly our message, no response stream ever opened: the provider did NOT take it
  // (a sign-up sheet in the way, a refused request), and whatever the page shows is not the reply. Measured
  // 14/10/2026 on Perplexity with its request refused: the fill marked the send, the field kept our text, and the
  // page's own sr-only heading ("What should we work on?") was delivered as "done". Only after the first snapshot
  // (initialAnswer), so a warm page's previous answer is still recorded as it is.
  var guardReady = false;
  function ownTextStillInComposer(){
    try {
      if (!guardReady || KNOWN_SENT || window.__ktStreamEver || REGEN || !SENT_SQ) return false;
      var v = composerVal();
      if (v === null) return false;
      var q = squash(v);
      return q.length > 0 && q.slice(0, 60) === SENT_SQ.slice(0, 60);
    } catch(e){ return false; }
  }
  // The send is marked as out (Rust's mark after the fill pressed Enter, or a re-injection that knows it) but the
  // page we are on never shows our message and no response stream ever opened: the send was lost with the page
  // it went from (measured 15/10/2026 on Perplexity: the tab was taken to a sign-in site and back 3.5 s after the
  // send; the fresh page held no conversation and its own heading, "You're incognito", was delivered as "done").
  // Whatever such a page shows is not a reply. The message is looked for by its first 40 characters, whitespace
  // collapsed (a long message can be shown cut, never from another place than its start), anywhere in the page.
  // It only counts as lost after LOST_SEND_MS of the page being loaded and in this state, so a conversation that
  // takes its time to render its history is not mistaken for it. Never resent: Rust's "one send" rule stays.
  var LOST_SEND_MS = 10000, ownSeen = false, lostSince = 0, lostCheckedAt = 0, lostState = '';
  // Letters and digits only, lower case: the page may show our message with its Markdown rendered (`# `, `**`,
  // `1.`, code fences) or its punctuation changed, which a literal comparison of the first characters mistook for
  // "not there" (a visible answer was then discarded). A ~20-character slice from the MIDDLE of the message,
  // normalised the same way, is looked for in the page's text.
  function alnum(x){ return String(x || '').toLowerCase().replace(/[^\p{L}\p{N}]+/gu, ''); }
  function ownProbe(){
    var n = alnum(SENT);
    if (n.length <= 20) return n;
    var at = Math.floor((n.length - 20) / 2);
    return n.slice(at, at + 20);
  }
  // 'na' = the guard does not apply, 'ok' = our message is on the page, 'box' = still in the composer (the page is
  // taking its time: the old 30 s path decides, this guard never counts), 'missing' = nowhere.
  function ownMsgState(){
    try {
      if (ownSeen) return 'ok';
      if (!guardReady || REGEN || !SENT_SQ || window.__ktStreamEver || (window.__ktStreamOpen || 0) > 0
          || !(KNOWN_SENT || window.__ktEnterPressed)) { lostSince = 0; return 'na'; }
      var now = Date.now();
      if (now - lostCheckedAt < 500) return lostState;
      lostCheckedAt = now;
      var st = 'missing';
      if (document.readyState !== 'complete') st = 'na';
      else {
        var probe = ownProbe(), cv = composerVal();
        // our message still sitting in the composer is not "on the page" (a rich composer is part of body.innerText)
        if (cv !== null && probe && alnum(cv).indexOf(probe) !== -1) st = 'box';
        else if (probe && alnum(document.body ? document.body.innerText : '').indexOf(probe) !== -1) { ownSeen = true; st = 'ok'; }
        // A sign-in wall or control on the page is the better explanation: the message is not lost "somewhere", the
        // provider wants a sign-in. Reported as that (see lostLogin), never as a failed send, and never before the
        // login detection's own checks have had their time.
        else if (loginHintPresent() || authWallPresent()) st = 'login';
      }
      if (st !== 'missing' && st !== 'login') lostSince = 0; else if (!lostSince) lostSince = now;
      if (st !== lostState) { try { fdiagArm('OWNMSG ' + lostState + '->' + st + ' known=' + KNOWN_SENT + ' enter=' + !!window.__ktEnterPressed + ' ever=' + (window.__ktStreamEver || 0)); } catch(e){} }
      return (lostState = st);
    } catch(e){ return 'na'; }
  }
  function ownMessageMissing(){ var s = ownMsgState(); return s === 'missing' || s === 'box' || s === 'login'; }
  // The message is nowhere on a page that shows a sign-in: that is a missing login (the block the user is shown).
  function lostLogin(){ return ownMsgState() === 'login' && lostSince > 0 && Date.now() - lostSince >= LOST_SEND_MS; }
  // Lost only when it has been nowhere for LOST_SEND_MS. Never while the message sits in the composer: the fill loop
  // may still be pressing Enter and the provider may yet take it.
  function sendLost(){ return ownMsgState() === 'missing' && lostSince > 0 && Date.now() - lostSince >= LOST_SEND_MS; }
  function answerTxt(){
    if (heldBeforeSend() || ownTextStillInComposer() || ownMessageMissing()) return '';
    var el = getAnswerEl();
    // innerText keeps the reasoning summary (see CHROME_SEL), so when the bubble holds one the text is read
    // by the chrome-skipping walk instead. Only then: the cheap innerText stays the hot path everywhere else.
    var withReasoning = false;
    try { var tc = el && el.querySelector('.thinking-container'); withReasoning = !!tc && isChrome(tc); } catch(e){}
    return sanitizeAnswer(el ? (withReasoning ? cleanAnswerText(el) : (el.innerText || '')) : '');
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
    // Only what the Markdown renderer reads as markup is escaped: an underscore inside a word (snake_case) is
    // plain text for it, and a backslash there would show in every copy of the answer.
    function esc(s){ return (s||'').replace(/[*`]/g, '\\$&'); }
    // Text as the page SHOWS it: whitespace in the HTML source (indentation, line breaks between tags) is
    // not content and collapses to one space, unless the element preserves it (white-space: pre*), where a
    // line break is a real line break. The style is read once per parent element.
    var wsCache = new Map();
    function visText(n){
      var p = n.parentElement, keep = false;
      if (p) {
        if (wsCache.has(p)) keep = wsCache.get(p);
        else { try { keep = /^pre/.test(getComputedStyle(p).whiteSpace); } catch(e){} wsCache.set(p, keep); }
      }
      return keep ? n.textContent : n.textContent.replace(/\s+/g, ' ');
    }
    var INLINE_TAGS = /^(a|b|strong|i|em|code|span|sup|sub|mark|u|s|small|abbr|time|label)$/;
    // What the page does not show is not answer: Claude hides its in-answer buttons with CSS, and a control with two
    // text blocks (isChrome lets those through as content) would otherwise end up in the text.
    function hiddenEl(n){
      if (n.hidden) return true;
      try { var cs = getComputedStyle(n); return cs.display === 'none' || cs.visibility === 'hidden'; } catch(e){ return false; }
    }
    function hasBlock(n){
      try { return !!n.querySelector('p,div,pre,table,ul,ol,blockquote,h1,h2,h3,h4,h5,h6,li'); } catch(e){ return false; }
    }
    // Bold and italic markers hug the words: "**Pixel **" is not bold in Markdown, so spaces move outside.
    function wrapMd(mark, inner){
      var m = inner.match(/^(\s*)([\s\S]*?)(\s*)$/);
      return m[2] ? m[1] + mark + m[2] + mark + m[3] : inner;
    }
    // ONE node. Taking the node's children instead (as the loop below does for a parent) dropped the node's own
    // bold, italic and link whenever it sat directly under a paragraph (measured 14/10/2026: "a **b** [c](x)"
    // came out "a b c").
    function inlineOne(n){
      if (n.nodeType === 3) return esc(visText(n));
      if (n.nodeType !== 1 || hiddenEl(n)) return '';
      var tag = n.tagName.toLowerCase();
      // A figure the reader typed or the page filled in a table cell (a generated interface's editable weights)
      // is the cell's content; every other control is chrome.
      if (tag === 'input' && /^(text|number)$/i.test(n.type || 'text') && n.value && n.closest && n.closest('td,th')) return esc(n.value);
      // Same rule as the block path: a control's label is not part of the answer.
      if (isChrome(n)) return '';
      if (tag === 'br') return '\n';
      if (tag === 'code') return '`' + n.textContent + '`';
      // ChatGPT's generated interfaces mark bold with an attribute on a span (no <strong>)
      if (tag === 'strong' || tag === 'b' || (n.hasAttribute && n.hasAttribute('data-d-default-strong'))) return wrapMd('**', inlineMd(n));
      if (tag === 'em' || tag === 'i') return wrapMd('*', inlineMd(n));
      if (tag === 'a') { var href = n.getAttribute('href') || ''; return '[' + inlineMd(n) + '](' + href + ')'; }
      var inner = inlineMd(n);
      // Spans laid out as boxes (a flex row of "name" and "1200 g") have no space between them in the markup and
      // came out glued ("Zaino 40-50 L1200 g"). Checked for plain spans/divs only, and never for the word
      // spans of a streaming answer: the style lookup would run for every word on every preview.
      if (inner && (tag === 'span' || tag === 'div') && !n.hasAttribute('data-d-text-flow')) {
        try { if (/^(block|flex|grid|list-item|table|inline-block|inline-flex|inline-grid)/.test(getComputedStyle(n).display)) return ' ' + inner + ' '; } catch(e){}
      }
      return inner;
    }
    function inlineMd(node){
      var out = '';
      node.childNodes.forEach(function(n){ out += inlineOne(n); });
      return out;
    }
    // Consecutive inline content (text, links, bold, <br>) is gathered into ONE paragraph and flushed when a
    // block starts. Pushing each piece as its own block split "see the [page](url) ." into three
    // paragraphs and turned every <br> of a poem into a blank line.
    function blockMd(node, depth){
      var out = [], buf = '';
      function flush(){
        var t = buf.replace(/[ \t]*\n[ \t]*/g, '\n').replace(/ {2,}/g, ' ').trim();
        if (t) out.push(t);
        buf = '';
      }
      node.childNodes.forEach(function(n){
        if (n.nodeType === 3) { buf += esc(visText(n)); return; }
        if (n.nodeType !== 1 || hiddenEl(n)) return;
        // Same rule as the plain-text path: a control's label is not part of the answer.
        if (isChrome(n)) return;
        var tag = n.tagName.toLowerCase();
        if (tag === 'br') { buf += '\n'; return; }
        // Mistral renders tables as an interactive card (sortable headers, no <table> in the painted DOM) and
        // keeps the real table as HTML in an attribute. Parsed into an inert <template> (no script runs, no
        // resource loads) and converted like any table; the painted card itself is skipped.
        var rich = n.getAttribute && n.getAttribute('data-rich-table-inner-html');
        if (rich) {
          try {
            var tpl = document.createElement('template');
            tpl.innerHTML = rich;
            if (tpl.content.querySelector('table')) { flush(); var tmd = blockMd(tpl.content, depth); if (tmd.trim()) out.push(tmd); return; }
          } catch(e){}
        }
        // Inline content stays in the current paragraph, unless it wraps blocks (custom elements such as
        // Gemini's wrappers around a table), which must be walked as blocks or the table is flattened.
        var isBlock = /^(h[1-6]|pre|blockquote|ul|ol|table|p|div)$/.test(tag) || (!INLINE_TAGS.test(tag) && hasBlock(n));
        if (!isBlock) { buf += inlineOne(n); return; }
        flush();
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
          // Real numbers: some providers render every step as its own <ol start="N">, and restarting
          // the count at 1 turned "1. 2. 3." into "1. 1. 1.".
          var i = (tag === 'ol' && n.start > 0) ? n.start - 1 : 0;
          n.querySelectorAll(':scope > li').forEach(function(li){
            i++;
            var marker = tag === 'ol' ? (i + '. ') : '- ';
            out.push('  '.repeat(depth) + marker + inlineMd(li).replace(/ {2,}/g, ' ').trim());
          });
          return;
        }
        if (tag === 'table') {
          var rows = n.querySelectorAll('tr'), lines = [];
          rows.forEach(function(tr, ri){
            var cells = tr.querySelectorAll('th,td');
            var cellTxt = Array.prototype.map.call(cells, function(c){ return inlineMd(c).replace(/\s*\n\s*/g, ' ').replace(/ {2,}/g, ' ').trim().replace(/\|/g, '\\|'); });
            lines.push('| ' + cellTxt.join(' | ') + ' |');
            if (ri === 0) lines.push('| ' + cellTxt.map(function(){ return '---'; }).join(' | ') + ' |');
          });
          out.push(lines.join('\n'));
          return;
        }
        var s = blockMd(n, depth); if (s.trim()) out.push(s);
      });
      flush();
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
    // Same VISIBLE-only pick as the fill script (ChatGPT keeps a hidden legacy textarea), and the
    // same canvas exclusion as pickComposer() in browser.rs -- see its comment for the live
    // measurement (ChatGPT's "writing block" panel has its own title textarea and body editor,
    // both matching these selectors; `data-testid="chatgpt-writing-block"` marks the whole panel
    // and is absent near the real composer). Kept in sync: this lookup and the fill's must agree
    // on which element is the composer, or delivered()/composerVal() reads the wrong box.
    var el = null;
    var sels = ['textarea:not([readonly]):not([aria-hidden="true"])', '[contenteditable="true"]', 'div[role="textbox"]'];
    for (var i=0;i<sels.length && !el;i++){
      var els = document.querySelectorAll(sels[i]);
      for (var j=0;j<els.length;j++){
        var e = els[j];
        if (e.offsetParent === null) continue;
        if (e.closest('[data-testid="chatgpt-writing-block"]')) continue;
        el = e; break;
      }
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
      // Z.ai sends a signed-out visitor whose message was refused back to its own sign-in page (reported
      // 14/10/2026: the card sat on /auth until the 180 s budget ran out). Prefix match, path only. Not yet seen
      // live in this session (the profile used here is served by the captcha instead): hypothesis from the report.
      if (KEY === 'zai' && (p === '/auth' || p.indexOf('/auth/') === 0)) return true;
    } catch(e){}
    return false;
  }
  // Consulted ONLY when the harvest has already failed, never on the success path: it decides the
  // MESSAGE, not the outcome, so it cannot cost a working answer. `authWallPresent` deliberately
  // requires the composer to be ABSENT before trusting a login control, because a logged-in page
  // can carry a stray "sign in" link. ChatGPT signed out breaks that assumption: it shows a working
  // composer AND the login buttons, lets the message through, answers anonymously, and the harvest
  // then found nothing and reported "no answer received" -- true, and useless to the user, who only
  // needed to be told to sign in. Here the composer guard is dropped: we are already failing.
  // English text is a valid signal for providers: browser.rs pins their UI language to English.
  function fdiagArm(m){ try { if (window.__ktDiag && window.__ktPush) window.__ktPush({ b: BID, k: KEY, st: 'diag', d: String(m).slice(0,300) }); } catch(e){} }
  function loginHintPresent(){
    try {
      var marked = document.querySelectorAll('[data-testid*="login" i],[data-testid*="signup" i],[data-testid*="sign-in" i],[id*="login-button" i],[id*="signup-button" i]');
      for (var i = 0; i < marked.length; i++) { if (marked[i].offsetParent !== null) return true; }
      var els = document.querySelectorAll('button, a, [role="button"]');
      for (var j = 0; j < els.length && j < 400; j++) {
        var t = (els[j].innerText || '').trim();
        if (t.length > 22) continue;
        if (!/^(log ?in|sign ?in|sign ?up)/i.test(t)) continue;
        if (els[j].offsetParent !== null) return true;
      }
    } catch(e){}
    return false;
  }
  function visibleMatch(sel){
    try {
      var els = document.querySelectorAll(sel);
      for (var i = 0; i < els.length; i++) {
        // Size + computed style, not offsetParent: captcha overlays are position:fixed, and a fixed element
        // has no offsetParent even while it covers the whole page (Z.ai's slider puzzle was missed that way).
        var r = els[i].getBoundingClientRect(), cs = getComputedStyle(els[i]);
        if (r.width > 0 && r.height > 0 && cs.visibility !== 'hidden' && cs.display !== 'none' && cs.opacity !== '0') return true;
      }
    } catch(e){}
    return false;
  }
  // Why the wall was detected, for the debug log: a wrong "sign in" is only fixable when we know which
  // signal fired.
  var authWallWhy = '';
  // Time the page gets, after our send, to open its response stream before ChatGPT's login buttons count as a wall.
  var OPENAI_WALL_MS = 10000;
  var LOGIN_SHEET_MS = 5000;
  function authWallPresent(){
    authWallWhy = '';
    // A response stream opened after our send means the provider accepted the message from a working
    // session: whatever login or captcha markup the page carries, this is not a signed-out page.
    // Captured on Z.ai (15/09/2026): the whole answer arrived on the network and the card said "sign in".
    if (window.__ktStreamEver) return false;
    try {
      if (loginUrlRedirected()) { authWallWhy = 'login-url'; return true; }
      // VISIBLE only: pages keep hidden password inputs and captcha containers mounted in advance (Z.ai
      // mounts its captcha host on every page), and their mere presence says nothing about the session.
      if (visibleMatch('input[type="password"]')) { authWallWhy = 'password'; return true; }
      if (visibleMatch('[class*="captcha" i], [id*="captcha" i], [data-testid*="captcha" i], iframe[src*="captcha" i], iframe[src*="turnstile" i]')) { authWallWhy = 'captcha'; return true; }
      // A sign-in SHEET: a login control marked by the developer (testid) INSIDE a visible modal dialog. Perplexity,
      // signed out and refused, covers the page with `[data-testid="login-modal"]` (inside a role=dialog, measured
      // 14/10/2026) and the answer never starts; its composer is still on the page, so the "no composer" rule below
      // never fired. No "sent" condition: the sheet swallows the fill's submit, so no Enter is ever recorded. Counts
      // only once the page has had LOGIN_SHEET_MS to open a response stream: the same sheet also shows over a thread that does answer (a stream then ends the
      // wait, see the top of this function), and an immediate verdict would flash "sign in" on a working turn.
      if (Date.now() - Math.max(window.__ktSentAt || 0, t0) > LOGIN_SHEET_MS) {   // max: __ktSentAt survives from the previous turn
        var sheets = document.querySelectorAll('[data-testid*="login" i], [data-testid*="sign-in" i], [data-testid*="signup" i]');
        for (var si = 0; si < sheets.length; si++) {
          if (!sheets[si].closest('[role="dialog"], [role="alertdialog"], [aria-modal="true"], dialog[open]')) continue;
          var sr = sheets[si].getBoundingClientRect();
          if (sr.width > 0 && sr.height > 0) { authWallWhy = 'login-sheet'; return true; }
        }
      }
      // Some providers (observed: Meta AI) show NEITHER a password field nor a captcha when
      // signed out -- just a visible "log in"/"sign in" control and no composer anywhere on
      // the page. Matched by testid/id substring (developer-set, language-independent, same
      // principle as the captcha check above) + composer absence, so a login link that's
      // merely present-but-irrelevant (e.g. "sign in with another account" while already
      // logged in, composer working fine) doesn't false-positive.
      var loginEls = document.querySelectorAll('[data-testid*="login" i], [id*="login-button" i], [data-testid*="sign-in" i], [id*="sign-in-button" i]');
      if (loginEls.length && composerVal() === null) {
        for (var i=0;i<loginEls.length;i++){ if (loginEls[i].offsetParent !== null) { authWallWhy = 'login-control-no-composer'; return true; } }
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
          if ((gt === 'Log in' || gt === 'Sign up') && els[gi].offsetParent !== null) { authWallWhy = 'grok-login-text'; return true; }
        }
      }
      // Same dead end on ChatGPT's logged-out mobile page (chatgpt.com/unauth-mweb): its "Log in" / "Sign up for free"
      // buttons carry no data-testid or id (seen 27/09/2026), so the text is read. It is English on Windows, where the
      // webview asks for English (--accept-lang, browser.rs); elsewhere a localized page simply never matches and the
      // turn ends on the usual timeout. The buttons are there from the first load and ChatGPT lets an anonymous user
      // through for ONE message, so they only count once the message has gone out, no new answer is on the page and
      // no response stream has opened for OPENAI_WALL_MS (the stream test is at the top of this function).
      if (KEY === 'openai' && (KNOWN_SENT || window.__ktEnterPressed)
          && Date.now() - (window.__ktSentAt || t0) > OPENAI_WALL_MS
          && answerTxt() === (typeof initialAnswer === 'string' ? initialAnswer : '')) {
        var oels = document.querySelectorAll('button, a');
        for (var oi=0; oi<oels.length; oi++){
          var ot = (oels[oi].innerText || '').trim();
          if ((ot === 'Log in' || ot === 'Sign up for free' || ot === 'Sign up') && oels[oi].offsetParent !== null) { authWallWhy = 'openai-login-text'; return true; }
        }
      }
    } catch(e){}
    return false;
  }
  /* ---- Blocks that need the user: a human check, a sign-in, a window over the composer. They are NOT an
     outcome: the card waits ("blocked"), the page is shown to the user, and the moment the block goes away
     (check solved, signed in, window closed) the send proceeds and the answer arrives as usual. Before
     this, each of them ended the card, and the user had to retry after fixing it. ---- */
  var blockPushed = '', blockSince = 0, BLOCK_MAX_MS = 20 * 60 * 1000;
  // Returns true when the block has lasted too long and the card was closed with it as the outcome.
  // A NEW block reason must hold for BLOCK_DEBOUNCE_MS before it is announced: the sign-in sheet or a 401 of an
  // optional call can be on screen for a second while the provider is already answering (measured 14/10/2026:
  // a 0.5-2 s "sign in" flash on Gemini and Perplexity before real answers). A stream opening ends the reason
  // (currentBlock returns '' then), so the flash never reaches the card; a real wall costs this much delay.
  var BLOCK_DEBOUNCE_MS = 2500, pendingReason = '', pendingSince = 0;
  function syncBlock(reason){
    if (!reason) pendingReason = '';   // every empty reading restarts the clock: two short flashes are not one block
    if (reason !== blockPushed) {
      if (reason) {
        if (pendingReason !== reason) { pendingReason = reason; pendingSince = Date.now(); return false; }
        if (Date.now() - pendingSince < BLOCK_DEBOUNCE_MS) return false;
      }
      blockPushed = reason;
      blockSince = reason ? Date.now() : 0;
      fdiagArm((reason ? 'BLOCKED ' + reason + (authWallWhy ? ' why=' + authWallWhy : '') : 'UNBLOCKED'));
      // IPC only: the navigation fallback of __ktPush would reload the very page the user has to act on.
      try { window.__TAURI__.core.invoke('kotodama_push', { b: BID, k: KEY, st: reason ? 'blocked' : 'unblocked', d: reason || '' }).catch(function(){}); } catch(e){}
    }
    if (reason && Date.now() - blockSince > BLOCK_MAX_MS) {
      deliver(reason === 'login' ? 'login' : (reason === 'captcha' ? 'captcha' : 'error'), '');
      return true;
    }
    return false;
  }
  // A dialog sitting on top of the composer: what is painted at the composer's position is not the
  // composer but something inside a modal. Structure only (role/aria-modal/<dialog>), never its text.
  function composerCovered(){
    try {
      var c = findComposerEl(); if (!c) return false;
      var r = c.getBoundingClientRect(); if (!r.width || !r.height) return false;
      var x = r.left + r.width / 2, y = r.top + Math.min(r.height / 2, 20);
      if (x < 0 || y < 0 || x > window.innerWidth || y > window.innerHeight) return false;
      var top = document.elementFromPoint(x, y); if (!top) return false;
      if (top === c || c.contains(top) || top.contains(c)) return false;
      return !!(top.closest && top.closest('[role="dialog"], [role="alertdialog"], [aria-modal="true"], dialog[open]'));
    } catch(e){ return false; }
  }
  // The dialog covering the composer is a sign-in sheet: it holds a developer-marked login control, a password
  // field or an e-mail field (Perplexity's "Sign up below" sheet asks for an e-mail). Structure only, any language.
  function coverIsSignIn(){
    try {
      var c = findComposerEl(); if (!c) return false;
      var r = c.getBoundingClientRect();
      var top = document.elementFromPoint(r.left + r.width / 2, r.top + Math.min(r.height / 2, 20));
      var dlg = top && top.closest && top.closest('[role="dialog"], [role="alertdialog"], [aria-modal="true"], dialog[open]');
      return !!(dlg && dlg.querySelector('[data-testid*="login" i], [data-testid*="sign-in" i], [data-testid*="signup" i], input[type="password"], input[type="email"], input[autocomplete="email"], input[autocomplete="username"]'));
    } catch(e){ return false; }
  }
  var noComposerTicks = 0;
  var CAPTCHA_SEL = '[class*="captcha" i], [id*="captcha" i], [data-testid*="captcha" i], iframe[src*="captcha" i], iframe[src*="turnstile" i]';
  // Only while nothing has come back: once a response stream opened, the provider has the message.
  function currentBlock(clockTicks){
    if (window.__ktStreamEver) return '';
    // Hysteresis: a block, once announced, lasts while its own sign is still on the page. Entering and
    // leaving on different conditions made Kimi flap blocked/unblocked every couple of seconds.
    if (blockPushed === 'login' && (loginHintPresent() || authWallPresent())) return 'login';
    if (blockPushed === 'captcha' && (visibleMatch(CAPTCHA_SEL) || window.__ktCaptchaAt)) return 'captcha';
    if (blockPushed === 'overlay' && composerCovered()) return coverIsSignIn() ? 'login' : 'overlay';
    if (blockPushed === 'setup' && composerVal() === null) return 'setup';
    if (authWallPresent()) return authWallWhy === 'captcha' ? 'captcha' : 'login';
    if (clockTicks > 6 && (visibleMatch(CAPTCHA_SEL) || (window.__ktCaptchaAt && Date.now() - window.__ktCaptchaAt > 15000))) return 'captcha';
    // Signed out, recognised in seconds instead of minutes: a visible login control AND the page's own
    // API refusing the session. Either alone is not enough (logged-in pages carry "sign in with another
    // account" links; a 401 can come from an optional feature).
    if (window.__ktAuthFail && loginHintPresent()) return 'login';
    if (clockTicks > 10 && composerCovered()) return coverIsSignIn() ? 'login' : 'overlay';
    // A loaded page that offers no composer at all and never took our message: a welcome, consent or setup
    // screen that needs a click from the user. Measured on Copilot: no editable element anywhere, no dialog,
    // only its privacy notice -- which was then delivered as the "answer".
    // Not while the page may still be loading: cold pages next to many others take well over 10s to show a
    // composer (measured on Grok: a false "setup" block, then an automatic retry that broke the answer).
    if (!KNOWN_SENT && !window.__ktEnterPressed && document.readyState === 'complete' && composerVal() === null
        && (window.performance ? performance.now() : 0) > 20000) {
      noComposerTicks++;
      // A welcome page offering "sign in" is a missing login, and the fix the user needs is the login page
      // (captured on Copilot signed out: "Welcome to Copilot" + Sign in, no composer).
      if (noComposerTicks > 20) return loginHintPresent() ? 'login' : 'setup';
    } else { noComposerTicks = 0; }
    return '';
  }
  function deliver(st, txt, md){
    if (window.__ktBid !== BID) return;
    liveStop();   // before clearing __ktBid: the final answer supersedes any pending preview
    window.__ktBid = null;
    // The send is over: nothing the page streams from here on belongs to us. `__ktSentAt` is what the stream
    // observers test before copying anything to Rust, so left set it kept forwarding every chunk of Claude's
    // long-lived timeline for as long as the page lived (measured 15/10/2026: still set 28 s after the delivery),
    // and it made a later Ferma think a send was out. The next send sets it again when it presses Enter.
    try { window.__ktSentAt = 0; } catch(e){}
    // A send declared failed must not go out afterwards: the fill loop presses Enter up to 25 times and stops only
    // on its own run id, which a delivery did not touch (the message could leave 12-30 s later while the card said
    // "failed", and a retry then posted it twice).
    if (st === 'sendfail') { try { window.__ktFillRun = '__ktCancelled'; } catch(e){} }
    txt = txt || '';
    // The text of a page-read answer IS its Markdown (tables as tables, headings, lists, bold): the walk that
    // joins text nodes (cleanAnswerText) puts every table cell on a line of its own (measured 14/10/2026 on a
    // ChatGPT price table). It wins only if it LOSES NO WORDS the plain reading has: every word of the plain text,
    // counted with repetitions, must be in the Markdown, with an allowance of 1% and never more than 2 words (a lost
    // card title is one or two words: a percentage alone hid it on a long answer). Otherwise the plain text stays
    // and `md` goes as before.
    if (md) {
      var mdTxt = sanitizeAnswer(md);
      if (mdTxt && txt) {
        var wordsOf = function(x){ return x.match(/[\p{L}\p{N}]+/gu) || []; };
        var have = Object.create(null), plainWords = wordsOf(txt), lost = 0;
        wordsOf(mdTxt).forEach(function(w){ have[w] = (have[w] || 0) + 1; });
        plainWords.forEach(function(w){ if (have[w] > 0) have[w]--; else lost++; });
        if (lost <= 2 && lost * 100 <= plainWords.length) { txt = mdTxt; md = mdTxt; }
      } else if (mdTxt) { txt = mdTxt; md = mdTxt; }
    }
    var MAXC = 150000, trunc = 0;
    if (txt.length > MAXC) { txt = txt.slice(0, MAXC); trunc = 1; }
    var hasIpc = !!(window.__TAURI__ && window.__TAURI__.core && typeof window.__TAURI__.core.invoke === 'function');
    if (hasIpc) {
      // Direct IPC: no URL-length or navigation-coalescing constraints -> the whole answer goes
      // in ONE call, no artificial delay. `md` (elToMd() output) travels ONLY on this path -- the
      // chunked nav fallback below never carries it, degrading gracefully to plain text.
      // cp: a human-check service was contacted after our send (answered or not), for the per-provider
      // record of how often checks are asked (see record_human_check in kotodama.rs).
      // se: a response stream was seen after our send -- evidence the provider received the message, used by
      // Rust to refuse a "done" for a message that never went out (see kotodama_push).
      window.__ktPush({ b: BID, k: KEY, st: st, s: 0, n: 1, tr: !!trunc, d: txt, md: md || '', cp: !!window.__ktCaptchaAt, se: !!window.__ktStreamEver });
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
  guardReady = true;
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
      // Cookie NAMES only (which language/consent cookies exist): the values include session tokens,
      // and secrets never go into a log, not even a local diagnostic one.
      try { out.push('cookies='+(document.cookie||'').split(';').map(function(c){ return c.split('=')[0].trim(); }).filter(Boolean).join(',').slice(0,300)); } catch(e){}
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
    // The time spent verifying an attachment is not the provider's: the clock waits for the attach hold too.
    if (composerVal() !== null && !window.__ktHoldAttach) armTries++;
    // Absolute ceiling, so a page that never produces a composer cannot wait forever.
    // A message we KNOW went out and that produced nothing in 180s is a timeout, not a failed send.
    if (Date.now() - armT0 > 180000) { clearInterval(armIv); fdiagArm('EXIT arm-180s hint=' + loginHintPresent() + ' streamEver=' + (window.__ktStreamEver||0) + ' armTries=' + armTries); census(); setTimeout(function(){ deliver((loginHintPresent() && !window.__ktStreamEver) ? 'login' : (KNOWN_SENT || window.__ktStreamEver ? 'timeout' : 'sendfail'), ''); }, 300); return; }
    authCensus();
    // A visible captcha is not a missing login: the session is fine, the provider wants a human check
    // before it takes this message. Saying "sign in" sent the user to the wrong fix (captured on Z.ai).
    // Blocked: tell the UI, keep watching, and stop the clocks -- the time the user spends solving a check
    // or signing in must not run out the budgets below.
    var blk = currentBlock(armTries);
    if (!blk && lostLogin()) blk = 'login';
    if (syncBlock(blk)) { clearInterval(armIv); return; }
    if (blk) { armT0 = Date.now(); if (blk !== 'login') lostSince = 0; if (armTries > 40) armTries = 40; return; }
    // The message we sent is not on this page and nothing streamed: the send was lost (see ownMessageMissing).
    if (sendLost()) { clearInterval(armIv); fdiagArm('EXIT send-lost (arming) known=' + KNOWN_SENT + ' enter=' + !!window.__ktEnterPressed + ' url=' + location.pathname); setTimeout(function(){ deliver('sendfail', ''); }, 300); return; }
    // Arms the stream watcher as soon as the message is known to be out. Needed because on `?q=`
    // providers the URL itself sends, so our fill script exits without ever pressing Enter and never
    // sets `__ktSentAt` -- which left exactly the fastest providers falling back to text stability
    // (measured: ChatGPT decided by stability at poll 5 while its stream had long since closed). The
    // answer's stream is still open at this point, so its close is captured.
    function armWatcher(){ try { if (!window.__ktSentAt) window.__ktSentAt = Date.now(); } catch(e){} }
    var cur = answerTxt();
    if (cur && cur !== initialAnswer) { armWatcher(); clearInterval(armIv); harvest(); return; }  // answer streaming
    // The provider refused the message for its usage limit (429 after our send). The harvest loop already
    // says so, but it is never reached when the refusal comes INSTEAD of an answer: the arming loop ran out
    // its budget and reported a failed send, which is false and sends the user looking for the wrong fix
    // (captured on Claude, 429 `rate_limit_error` on the completion endpoint -> `sendfail` after 33s).
    if (window.__ktLimitHit && window.__ktLimitHit >= (window.__ktSentAt || 0) && !cur) {
      clearInterval(armIv); fdiagArm('EXIT limit-in-stream (arming)');
      setTimeout(function(){ deliver('limit', ''); }, 300);
      return;
    }
    if (window.__ktRateLimited && !window.__ktStreamEver && !cur) {
      clearInterval(armIv); fdiagArm('EXIT rate-limit (arming)');
      setTimeout(function(){ deliver('limit', ''); }, 300);
      return;
    }
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
    // Signed out: say so after the arming budget instead of waiting out the whole 180s. Three
    // independent conditions have to hold together, and together they describe only that case:
    // no answer text has appeared, NO response stream has ever opened (a provider that is
    // generating always opens one), and a login control is visible. Measured on ChatGPT signed
    // out: zero streams, the "Log in" button on screen, and the user told to sign in after 182
    // seconds -- correct, and three minutes too late.
    if (armTries > 60 && !window.__ktStreamEver && loginHintPresent()) { if (syncBlock('login')) { clearInterval(armIv); return; } armT0 = Date.now(); armTries = 40; return; }
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
  // DISCOVERY probe (debug only, KOTO_MENUPROBE=1): opens every menu control in the answer's action row and
  // lists what is inside, so controls that live in a menu (ChatGPT's regenerate, Perplexity's rewrite) can be
  // addressed by structure. Only buttons that declare a popup are touched, never the plain actions.
  function menuCensus(){
    try {
      var ael = getAnswerEl(); if (!ael) return;
      var ar = ael.getBoundingClientRect();
      var all = document.querySelectorAll('button[aria-haspopup], [role="button"][aria-haspopup]'), opens = [];
      for (var i = 0; i < all.length; i++) {
        var r = all[i].getBoundingClientRect();
        if (!r.width || r.top < ar.top || r.top > ar.bottom + 90) continue;
        opens.push(all[i]);
      }
      var idx = 0;
      (function next(){
        if (idx >= opens.length) return;
        var b = opens[idx++], label = (b.getAttribute('aria-label') || b.getAttribute('data-testid') || '?').slice(0, 24);
        try { b.click(); } catch(e){}
        setTimeout(function(){
          var its = document.querySelectorAll('[role="menuitem"], [role="option"], [mat-menu-item], .mat-mdc-menu-item');
          if (!its.length) {
            // Popovers that do not use menu roles (ChatGPT's model switch): read the floating layer's own controls.
            var pops = document.querySelectorAll('[data-radix-popper-content-wrapper], [role="menu"], [role="listbox"], [role="dialog"]');
            if (pops.length) its = pops[pops.length - 1].querySelectorAll('button, [role="button"], a');
          }
          var out = [];
          for (var k = 0; k < its.length && k < 14; k++) {
            var it = its[k];
            out.push('testid=' + (it.getAttribute('data-testid') || it.getAttribute('data-test-id') || '-')
              + ' text=' + (it.innerText || '').trim().slice(0, 22).replace(/\s+/g, ' '));
          }
          window.__ktPush({ b: BID, k: KEY, st: 'diag', d: ('MENU[' + label + '] n=' + its.length + ' ' + out.join(' || ')).slice(0, 1600) });
          // Also: every action control the page exposes for a turn, by test id. This is what says whether a
          // regenerate exists at all on this provider's current layout.
          try {
            var ids = document.querySelectorAll('[data-testid*="turn-action"], [data-testid*="retry"], [data-testid*="regenerate"]'), names = [];
            for (var z = 0; z < ids.length && z < 20; z++) names.push(ids[z].getAttribute('data-testid'));
            if (names.length) window.__ktPush({ b: BID, k: KEY, st: 'diag', d: ('TESTIDS ' + names.join(' | ')).slice(0, 1200) });
          } catch(e){}
          try { document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); } catch(e){}
          try { document.body.click(); } catch(e){}
          setTimeout(next, 600);
        }, 800);
      })();
    } catch(e){}
  }
  // DISCOVERY probe (debug only, KOTO_ACTIONPROBE=1): the action buttons under the last answer (copy, read
  // aloud, regenerate...), described by structure only: data-testid, aria-label, svg icon signature, position.
  // Used to find each provider's read-aloud and regenerate controls without matching visible text.
  function actionCensus(){
    try {
      var ael = getAnswerEl(); if (!ael) return;
      var ar = ael.getBoundingClientRect();
      var all = document.querySelectorAll('button, [role="button"]'), out = [];
      for (var i = 0; i < all.length && out.length < 25; i++) {
        var b = all[i], r = b.getBoundingClientRect();
        if (!r.width || !r.height) continue;
        // Below the answer's top and within a short distance under its bottom: the answer's own action bar.
        if (r.top < ar.top || r.top > ar.bottom + 90) continue;
        if (r.left < ar.left - 60 || r.left > ar.right + 60) continue;
        var svg = b.querySelector('svg'), sig = '';
        if (svg) {
          var use = svg.querySelector('use'); var path = svg.querySelector('path');
          sig = use ? ('use=' + (use.getAttribute('href') || use.getAttribute('xlink:href') || '')) : (path ? ('d=' + (path.getAttribute('d') || '').slice(0, 24)) : 'svg');
        } else {
          var mi = b.querySelector('mat-icon');
          if (mi) sig = 'mat-icon=' + (mi.getAttribute('fonticon') || mi.getAttribute('data-mat-icon-name') || (mi.textContent || '').trim()).slice(0, 30);
        }
        out.push('[' + Math.round(r.left - ar.left) + ',' + Math.round(r.top - ar.bottom) + '] testid=' + (b.getAttribute('data-testid') || '-')
          + ' aria=' + (b.getAttribute('aria-label') || '-').slice(0, 30) + ' cls=' + String(b.className || '').slice(0, 40) + ' ' + sig);
      }
      window.__ktPush({ b: BID, k: KEY, st: 'diag', d: ('ACTIONS ' + out.join(' || ')).slice(0, 3000) });
    } catch(e){}
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
  // LIVE PREVIEW: the answer is shown while it is being written, instead of all at once at the end.
  // Driven by DOM MUTATIONS, not by polling: when the page is not changing, nothing runs and nothing is
  // sent. The timer below only COALESCES a burst of mutations into one push every LIVE_MS, so a fast
  // streaming answer cannot flood IPC with a message per token.
  // IPC ONLY, never __ktPush: its fallback NAVIGATES the page, and a single navigation in the middle of
  // an answer tears the conversation down (measured on Qwen with a diagnostic probe that did exactly
  // that). No bridge means no preview; the final answer still arrives through the normal delivery.
  var LIVE_MS = 150, liveTimer = null, liveLast = '', liveObs = null, liveN = 0, liveMs = 0, liveMax = 0;
  function liveFlush(){
    liveTimer = null;
    if (window.__ktBid !== BID) { liveStop(); return; }
    try {
      // Same test the harvest uses, on the same cheap reading: still the PREVIOUS answer means the new one
      // has not started, and a warm follow-up must not flash the old reply.
      if (answerTxt() === initialAnswer) return;
      var t0 = performance.now();
      var el = getAnswerEl();
      if (!el) return;
      var txt = sanitizeAnswer(cleanAnswerText(el));
      if (!txt || txt === liveLast) return;
      var md = elToMd(el);
      var dt = performance.now() - t0;
      liveN++; liveMs += dt; if (dt > liveMax) liveMax = dt;
      liveLast = txt;
      window.__TAURI__.core.invoke('kotodama_push', { b: BID, k: KEY, st: 'partial', d: txt, md: md }).catch(function(){});
    } catch(e){}
  }
  function liveSchedule(){ if (!liveTimer) liveTimer = setTimeout(liveFlush, LIVE_MS); }
  function liveStart(){
    if (liveObs || typeof MutationObserver !== 'function') return;
    if (!(window.__TAURI__ && window.__TAURI__.core && typeof window.__TAURI__.core.invoke === 'function')) return;
    try {
      liveObs = new MutationObserver(liveSchedule);
      liveObs.observe(document.body, { childList: true, subtree: true, characterData: true });
      liveSchedule();   // the answer can already hold text when the harvest starts
    } catch(e){ liveObs = null; }
  }
  function liveStop(){
    if (liveTimer) { clearTimeout(liveTimer); liveTimer = null; }
    if (!liveObs) return;
    try { liveObs.disconnect(); } catch(e){}
    liveObs = null;
    // Cost of the hot path, measured instead of estimated: extraction time per push (debug log only).
    if (window.__ktDiag && liveN) {
      try { window.__ktPush({ b: BID, k: KEY, st: 'diag', d: 'LIVE pushes=' + liveN + ' avgMs=' + (liveMs / liveN).toFixed(1) + ' maxMs=' + liveMax.toFixed(1) }); } catch(e){}
    }
  }
  function harvest(){
    harvesting = true;
    liveStart();
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
      // Still showing the previous answer, new one not in DOM yet. Except after a regenerate whose stream has
      // closed: the provider did answer again, and an identical wording is still the new answer.
      if (txt === initialAnswer && !(REGEN && streamEnded)) txt = '';
      // Text on a page that has no composer, never took our message and never streamed is not an answer
      // (Copilot's welcome notice): ignore it, so the setup block below gets its chance.
      if (txt && !KNOWN_SENT && !window.__ktEnterPressed && !window.__ktStreamEver && composerVal() === null) txt = '';
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
      // ...and it must not still be PAINTING. The stream closing says the server finished sending;
      // the provider can keep revealing the text afterwards with an animation, and its container
      // carries a marker while it does. Delivering in that window truncates the answer: measured on
      // ChatGPT's writing-block UI, "OK" came out as "O" on two runs out of two, while the same
      // build had been fine that morning -- the provider had changed how it renders, not us.
      // Structural markers only, scoped to the answer element. If one ever lingers after the end,
      // nothing hangs: the stability rule below still completes the harvest, just without the
      // shortcut.
      function stillPainting(){
        try {
          var el = getAnswerEl();
          if (!el) return false;
          var sel = '[class*="streaming" i],[data-is-streaming="true"]';
          if (el.matches && el.matches(sel)) return true;
          return !!el.querySelector(sel);
        } catch(e){ return false; }
      }
      // "Unchanged for one poll" counts POLLS, and the stream-end event can call this step a few milliseconds after
      // a timed poll (measured on Grok, TODO 49: 72 ms). Two reads that close together prove nothing about a page
      // that paints in bursts (Grok: about 400 ms apart, measured 18/10/2026: the text still grew 287 -> 321
      // characters after the provider's own end markers had flipped), so the text must also have been unchanged
      // for a full poll's worth of TIME. When the event lands too early the regular 1 s poll completes it.
      var settled = stable >= 1 && (Date.now() - lastChangeAt) >= 900;
      var doneByEvent = streamEnded && !!txt && !busy && !stillPainting() && settled;
      // The provider's answer stream is still open: its text can pause for many seconds between bursts while the
      // model works (measured on ChatGPT, 05/10/2026: no frame for 6.7 s in the middle of a 600-character answer, the
      // page looked "stable" for 3 polls and the answer was delivered cut at 417 B while the stream went on to
      // finish it). Stability is only evidence of an end when nothing is still arriving, so while the stream is open
      // only its close (`doneByEvent`) or a 45-poll stall may end the harvest. Providers with no readable stream
      // never count here (`__ktNetOpen` stays 0), so their behaviour is unchanged.
      var netOpen = (window.__ktNetOpen || 0) > 0;
      var stableEnough = netOpen ? stable >= 45 : ((stable >= neededStable && !busy) || stable >= 10);
      if (doneByEvent || stableEnough) {
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
        // DISCOVERY (debug only): the structure of the delivered answer. A UI control's label can
        // ride along inside the answer container -- measured: ChatGPT's "Edit" landing at the head
        // of every inline-transform result. Dumping the head of the markup identifies the element
        // to exclude structurally, instead of filtering a word that changes with the UI language.
        if (window.__ktDiag) {
          try {
            var ael = getAnswerEl();
            if (ael) {
              var kids = [];
              for (var ci = 0; ci < ael.children.length && ci < 6; ci++) {
                var ch = ael.children[ci];
                var cls = (typeof ch.className === 'string' ? ch.className : '').trim().split(/\s+/).slice(0,3).join('.');
                kids.push(ch.tagName.toLowerCase() + (cls ? '.' + cls.slice(0,34) : '')
                  + '("' + (ch.innerText||'').trim().replace(/\s+/g,' ').slice(0,26) + '")');
              }
              // Identity of the FIRST element that contributes text: that is where a stray label
              // like ChatGPT's "Edit" sits, and its tag/attributes are what a structural rule can key on.
              var firstTxt = '';
              try {
                var all = ael.querySelectorAll('*');
                for (var k = 0; k < all.length; k++) {
                  var e = all[k];
                  var own = '';
                  for (var c = 0; c < e.childNodes.length; c++) { if (e.childNodes[c].nodeType === 3) own += e.childNodes[c].textContent; }
                  if (!own.trim()) continue;
                  var chain = [];
                  var cur = e;
                  for (var up = 0; up < 4 && cur && cur !== ael; up++) {
                    var at = [];
                    for (var ai = 0; ai < cur.attributes.length; ai++) {
                      var a = cur.attributes[ai];
                      if (a.name === 'class') { at.push('.' + String(a.value).trim().split(/\s+/).slice(0,3).join('.').slice(0,44)); }
                      else if (a.name !== 'style') { at.push(a.name + '=' + String(a.value).slice(0,22)); }
                    }
                    chain.push(cur.tagName.toLowerCase() + at.join(''));
                    cur = cur.parentElement;
                  }
                  firstTxt = 'own="' + own.trim().replace(/\s+/g,' ').slice(0,24) + '" ' + chain.join('  <  ');
                  break;
                }
              } catch(e){}
              var rawTxt = (ael.innerText||'').trim().replace(/\s+/g,' ').slice(0,50);
              var cleanTxt = cleanAnswerText(ael).replace(/\s+/g,' ').slice(0,50);
              var nChrome = 0; try { nChrome = ael.querySelectorAll(CHROME_SEL).length; } catch(e){}
              window.__ktPush({ b: BID, k: KEY, st: 'diag', d: ('ANSWER-SHAPE comandi=' + nChrome
                + ' GREZZO="' + rawTxt + '" PULITO="' + cleanTxt + '" PRIMO-TESTO ' + firstTxt).slice(0,1400) });
            }
          } catch(e){}
        }
        if (window.__ktThinkProbe) thinkCensus();
        if (window.__ktActionProbe) setTimeout(actionCensus, 2500);   // action bars appear once the answer settled
        if (window.__ktMenuProbe) setTimeout(menuCensus, 3000);
        // Hand over the chrome-free text; fall back to the raw one if the walk yields nothing,
        // so a provider whose markup defeats it degrades to today's behaviour instead of silence.
        deliver('done', sanitizeAnswer(cleanAnswerText(getAnswerEl())) || txt, elToMd(getAnswerEl()));
        return;
      }
      // The provider refused the message for its usage limit: say so at once (the user can only wait or upgrade,
      // so this is an outcome, not a block to queue).
      if (!txt && !window.__ktStreamEver && window.__ktRateLimited) { clearInterval(iv); fdiagArm('EXIT rate-limit'); deliver('limit', ''); return; }
      // The limit was announced inside a stream that did open (Grok): no text, and the provider said why.
      if (!txt && window.__ktLimitHit && window.__ktLimitHit >= (window.__ktSentAt || 0)) { clearInterval(iv); fdiagArm('EXIT limit-in-stream'); deliver('limit', ''); return; }
      // Same block handling as the arming loop, for blocks that appear after the message was handed over
      // (measured on Z.ai: the slider puzzle shows about 4s after Enter). Clock frozen while blocked.
      // A page without a composer that never took our message can show text that is not an answer (Copilot's
      // notice), so for that case the block check runs even with text on the page.
      var hblk = (txt && (window.__ktEnterPressed || window.__ktStreamEver || composerVal() !== null)) ? '' : currentBlock(polls);
      if (!hblk && lostLogin()) hblk = 'login';
      if (!hblk && polls > 45 && !txt && !window.__ktStreamEver && loginHintPresent()) hblk = 'login';
      if (syncBlock(hblk)) { clearInterval(iv); return; }
      if (hblk) { t0 = Date.now(); if (hblk !== 'login') lostSince = 0; return; }
      if (sendLost()) { clearInterval(iv); fdiagArm('EXIT send-lost (harvest) known=' + KNOWN_SENT + ' enter=' + !!window.__ktEnterPressed + ' url=' + location.pathname); deliver('sendfail', ''); return; }
      // Signed out: say so early instead of waiting out the whole budget. Measured: the decision
      // is taken HERE, not in the arming loop (EXIT harvest-180s txtLen=0 hint=true), because the
      // page does carry some text that hands the arming loop over before the answer exists. Three
      // conditions must hold together and together they describe only that case: no answer text,
      // no response stream EVER opened (a provider that is generating always opens one), and a
      // login control visible on the page. It only chooses the MESSAGE of an outcome that is
      // already a failure, so it cannot cost a working answer.

      if (Date.now() - t0 > 180000) { clearInterval(iv); fdiagArm('EXIT harvest-180s txtLen=' + (txt||'').length + ' hint=' + loginHintPresent()); deliver(txt ? 'timeout' : ((loginHintPresent() && !window.__ktStreamEver) ? 'login' : 'error'), txt ? (sanitizeAnswer(cleanAnswerText(getAnswerEl())) || txt) : txt, txt ? elToMd(getAnswerEl()) : ''); return; }
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
    if(ctl){{ clearInterval(iv); try{{ ctl.click(); }}catch(e){{}} window.__ktTempDone=true; diag('TEMPCLICK-OK'); setTimeout(function(){{ window.__ktHoldFill=false; }},1500); return; }}
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
        "var __kt_bid = {}; var __kt_key = {}; var __kt_ans = {}; var __kt_busy = {}; var __kt_fresh = {fresh}; var __kt_fast = {fast}; var __kt_sent = false; window.__ktDiag = {diag}; window.__ktStreamEver = 0; window.__ktLimitHit = 0; window.__ktRateLimited = 0; window.__ktSentAt = 0; window.__ktEnterPressed = false; window.__ktTrustedInput = {trusted}; window.__ktNetUrl = {net_url}; window.__ktFreshBaseline = {fresh_baseline};",
        serde_json::to_string(broadcast_id).map_err(|e| e.to_string())?,
        serde_json::to_string(key).map_err(|e| e.to_string())?,
        serde_json::to_string(ans).map_err(|e| e.to_string())?,
        serde_json::to_string(busy).map_err(|e| e.to_string())?,
        fast = fast_done_for(key),
        diag = crate::debug::enabled(),
        trusted = browser::needs_trusted_input(key),
        net_url = net_url_js(key),
        fresh_baseline = unreliable_prewarm(key),
    );
    // incognito/temporary trigger (holds the fill until done), only on fresh turns of providers
    // that have an in-page trigger AND the user enabled it for this provider.
    let temp_part = if fresh && temp { temp_trigger_js(key).unwrap_or_default() } else { String::new() };
    // The INCOG diagnostic probe only runs under KOTODAMA_DEBUG (used to discover a provider's
    // incognito URL/selector); never in production.
    let probe = if fresh && crate::debug::enabled() { TEMP_PROBE_JS } else { "" };
    // Reasoning discovery probe: only sets a flag; the census itself runs at delivery time.
    let think = String::new()
        + if crate::debug::enabled() && std::env::var("KOTO_THINKPROBE").is_ok() { "window.__ktThinkProbe = true;" } else { "" }
        + if crate::debug::enabled() && std::env::var("KOTO_ACTIONPROBE").is_ok() { "window.__ktActionProbe = true;" } else { "" }
        + if crate::debug::enabled() && std::env::var("KOTO_MENUPROBE").is_ok() { "window.__ktMenuProbe = true;" } else { "" };
    // Network discovery probe: BEFORE the fill, or the send request itself is missed.
    let net = if crate::debug::enabled() && std::env::var("KOTO_NETPROBE").is_ok() {
        NET_PROBE_JS
    } else {
        ""
    };
    // STREAM_WATCH_JS goes BEFORE the fill: it has to be in place before the send opens the answer's
    // stream, otherwise the one request that matters is the one it misses.
    Ok(prelude
        + &think
        + PUSH_HELPER_JS
        + SR_HIDE_JS
        + RESPONSE_ADOPT_JS
        + net
        + STREAM_WATCH_JS
        + &temp_part
        + &attach_js(broadcast_id)
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
    temp: bool,
) -> Result<String, String> {
    let (ans, busy) = selectors_for(key);
    // Whitespace-collapsed head of the message for a robust "is it on the page?" check.
    let head: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect();
    let prelude = format!(
        // `window.__ktDiag` must be set HERE too: without it, all the fill-loop diagnostics stayed
        // silent in exactly the path where they are needed -- the resume after a navigation (providers
        // whose temporary chat is a click DO navigate).
        "var __apb_text = {}; var __kt_head = {}; var __apb_send = true; var __kt_bid = {}; var __kt_key = {}; var __kt_ans = {}; var __kt_busy = {}; var __kt_fresh = true; var __kt_fast = {fast}; var __kt_sent = {sent}; window.__ktDiag = {diag}; window.__ktStreamEver = 0; window.__ktLimitHit = 0; window.__ktRateLimited = 0; window.__ktTrustedInput = {trusted}; window.__ktNetUrl = {net_url}; window.__ktFreshBaseline = {fresh_baseline};",
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
        trusted = browser::needs_trusted_input(key),
        net_url = net_url_js(key),
        fresh_baseline = unreliable_prewarm(key),
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
            + RESPONSE_ADOPT_JS
            + net_probe_js()
            + STREAM_WATCH_JS
            + HARVEST_JS);
    }
    // This resume can follow Grok's OWN private-chat toggle (a real `<a href>`: its click fires the same
    // navigation events as a page load, even for an in-page route change). `window.__ktTempDone` survives
    // exactly when the page's script realm survived: re-click only when it is gone (a real reload lost the
    // private state), never a second time on a page already private, which would switch it back OFF.
    // Grok only: it is the one provider measured (27/09/2026). On a provider whose toggle click reloads into a page
    // that is already private, the flag is lost with the old page and a second click would switch privacy OFF;
    // others join this list only after checking two things on their own page: does the toggle click reload the
    // page, and does the private page show the same toggle icon (so that a second click would hit it again)?
    let temp_part = if temp && key == "grok" {
        temp_trigger_js(key).map(|js| format!("if(!window.__ktTempDone){{{}}}", js)).unwrap_or_default()
    } else {
        String::new()
    };
    let fill = browser::fill_js(text, true)?;
    // The stream watcher goes in here too, before the fill, as in build_inject_js: without it a resend after a
    // navigation could only end by DOM stability, about 3 s later than the stream's own end (seen on ChatGPT,
    // 27/09/2026). It installs itself once per document, so a page whose script realm survived keeps its own.
    Ok(prelude
        + PUSH_HELPER_JS
        + SR_HIDE_JS
        + RESPONSE_ADOPT_JS
        + net_probe_js()
        + STREAM_WATCH_JS
        + &temp_part
        + &attach_js(broadcast_id)
        + &fill
        + HARVEST_JS)
}

/// Marks (bid, key) answered: removes it from the broadcast, emits `app://kotodama-answer`
/// and, when the broadcast empties, `app://kotodama-finished`. Duplicate calls are no-ops.
fn finish_key(window: &Window, bid: &str, key: &str, status: &str, text: &str, truncated: bool, md: &str) {
    let attach_failed = attach_fail_marks().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
    if attach_failed {
        // The page may get the file late: it is not reused (no warm tab, no follow-up into it).
        dirty_pages().lock().unwrap().insert(key.to_string());
        prewarmed().lock().unwrap().remove(key);
    }
    // An outcome ends any block on this provider for this broadcast, and its network readers.
    blocked_marks().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
    login_marks().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
    offsite_blocks().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
    net_readers().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
    net_partials().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
    net_work().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
    net_work_owner().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
    net_work_dbg().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
    net_finish_armed().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
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
        if ah.get(key).map(|(b, _, _)| b == bid).unwrap_or(false) {
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
    // The provider page now ends with this answer: the one its read-aloud button would read.
    set_conv(window, key, (status == "done").then_some(bid));
    let _ = window.emit(
        "app://kotodama-answer",
        serde_json::json!({ "broadcastId": bid, "key": key, "status": status, "text": text, "truncated": truncated, "md": md, "reason": if attach_failed { "attach" } else { "" } }),
    );
    if all_done {
        let _ = window.emit("app://kotodama-finished", serde_json::json!({ "broadcastId": bid }));
    }
}

/// Delivers the answer from the provider's own stream if the page has not delivered it within
/// `NET_FINISH_GRACE` of the stream's end. `finish_key` hands out one outcome per (broadcast, provider),
/// so whichever comes first wins and the other is dropped; the text is read again at the deadline because
/// the stream may have grown since it ended (a closing frame after the end marker).
fn schedule_net_finish(window: Window, bid: String, key: String, grace: std::time::Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(grace);
        let still_pending = broadcasts().lock().unwrap().get(&bid).map(|bc| bc.pending.contains(&key)).unwrap_or(false);
        if !still_pending {
            return; // the page delivered it: the normal path
        }
        let text = net_readers()
            .lock()
            .unwrap()
            .get(&(bid.clone(), key.clone()))
            // a reader whose end is explicit counts only while its answer is still finished: Claude can go busy
            // again between two steps of the same answer, and a text taken then would be cut
            .and_then(|rs| {
                rs.values()
                    .filter(|r| !r.done_is_explicit() || r.answer().done)
                    .map(|r| r.answer().text.clone())
                    .max_by_key(|t| t.len())
            })
            .unwrap_or_default();
        if text.trim().is_empty() {
            return;
        }
        debug::log(format!(
            "kotodama NET-FINISH key={key} bid={bid}: stream ended, page did not deliver within {} ms, answer taken from the stream",
            grace.as_millis()
        ));
        finish_key(&window, &bid, &key, "done", &text, false, &text);
    });
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

/// Providers whose network reader does not carry the cards the page renders around an answer.
const NET_WITHOUT_CARDS: &[&str] = &["gemini"];

/// Does the page carry text the stream lacks (a card: a title, a note)? Both texts lose the Markdown syntax, the
/// case, the spaces and the punctuation, and are compared in groups of 4 characters rather than words, so it works
/// in languages written without spaces (Japanese, Chinese) and the stream's `**bold**` against the page's plain
/// text is no difference. The page has a card when at least `CARD_MISSING_MIN` groups are missing AND they are
/// 8% or more of the page's text: a title or a note of a translation is far over both, a code block's one-word
/// language label inside a long answer is not.
fn page_has_cards(net: &str, dom: &str) -> bool {
    const N: usize = 4;
    let (lost, total) = text_missing(net, dom, N);
    lost >= CARD_MISSING_MIN && lost * 100 >= total * 8
}
/// `(groups of the page's text missing from the stream's, groups of the page's text)`.
fn text_missing(net: &str, dom: &str, n: usize) -> (usize, usize) {
    // Markdown syntax characters are not alphanumeric, so this already drops asterisks, backticks, # and |. The
    // numbers of a numbered list ("1. ", "2) ") are in the stream and not in the page's text: dropped from both, or
    // every item of a list would look like text the stream lacks.
    let norm = |t: &str| -> Vec<char> {
        let mut out = Vec::new();
        for line in t.lines() {
            let l = line.trim_start();
            let digits = l.chars().take_while(|c| c.is_ascii_digit()).count();
            let rest = &l[digits..];
            let body = if digits > 0 && digits <= 3 && (rest.starts_with(". ") || rest.starts_with(") ")) { &rest[2..] } else { line };
            out.extend(body.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()));
        }
        out
    };
    let grams = |v: &[char]| -> Vec<String> { v.windows(n).map(|w| w.iter().collect::<String>()).collect() };
    let mut have: HashMap<String, usize> = HashMap::new();
    for g in grams(&norm(net)) {
        *have.entry(g).or_insert(0) += 1;
    }
    let (mut lost, mut total) = (0, 0);
    for g in grams(&norm(dom)) {
        total += 1;
        match have.get_mut(&g) {
            Some(c) if *c > 0 => *c -= 1,
            _ => lost += 1,
        }
    }
    (lost, total)
}
const CARD_MISSING_MIN: usize = 5;

/// Length of an answer as it reads, without the addresses of its Markdown links: the page's text now comes as
/// Markdown, and `[name](https://long/address)` is not more answer than `name`. The network-vs-page comparison
/// (a network answer much shorter than the page is an incomplete one) must not be fooled by them.
fn visible_len(md: &str) -> usize {
    let mut n = md.len();
    let mut rest = md;
    while let Some(at) = rest.find("](") {
        let after = &rest[at + 2..];
        match after.find(')') {
            Some(end) => {
                n -= end + 2; // the "(", the address and the ")" after the "]"
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    n
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
    cp: Option<bool>,
    se: Option<bool>,
) {
    // A "done" needs evidence that the message reached the provider: our send was marked (the fill pressed
    // Enter), or a response stream opened (providers that send from the URL never press Enter). Without
    // either, the text is whatever the page already showed. Measured on Copilot: the field was never found,
    // nothing was sent, and its privacy notice was delivered as the answer.
    let mut st = st;
    let mut d = d;
    let mut md = md;
    // An attachment that never reached the composer: no text travels, the card says why from its own wording.
    // (the marking itself is in `handle_push`, shared with the navigation fallback)
    let attach_failed = s == Some(0) && is_attach_failure(&st, d.as_deref());
    // The answer read from the network wins when it is complete: the provider's own Markdown, exact, with
    // reasoning and sources kept apart. The DOM reading stays the fallback for everything else.
    let mut from_net = false;
    // "login" and "captcha" are in the list because they are guesses made from the page: a provider whose own response
    // stream delivered a complete answer did take the message (Gemini, 14/10/2026: the card closed with "login" after
    // 180 s although the network held the whole text). The network answer wins; with none, the verdict stands.
    if !attach_failed && s == Some(0) && matches!(st.as_str(), "done" | "timeout" | "error" | "sendfail" | "login" | "captcha") {
        if crate::debug::enabled() {
            if let Some(rs) = net_readers().lock().unwrap().get(&(b.clone(), k.clone())) {
                for (id, r) in rs {
                    let a = r.answer();
                    debug::log(format!("kotodama NET state key={k} id={id} done={} text={}B skipped={}", a.done, a.text.len(), a.skipped));
                }
            }
        }
        // Safety net: a network answer much SHORTER than what the page shows is incomplete (a reader that saw only
        // part of a multi-request answer -- measured on Claude with web search: 811 B from the network, 1553 B in
        // the page). Markdown makes the network text longer, never shorter, so below 70% the page wins.
        let dom_len = d.as_deref().map(visible_len).unwrap_or(0);
        let net = net_answer(&b, &k).filter(|a| {
            let ok = st != "done" || dom_len == 0 || a.text.len() * 10 >= dom_len * 7;
            if !ok {
                debug::log(format!("kotodama NETREAD skipped key={k}: net={}B shorter than dom={dom_len}B", a.text.len()));
            }
            // ChatGPT can answer with a small generated interface (October 2026: checklists with buttons, `{#each}`
            // loops over data, cards with favicons): the network carries its SOURCE, and only the page has the text
            // it renders. Then the page wins, whatever the lengths.
            // Gemini's stream does not carry its cards (a translation's "Translation" title and its notes, measured
            // 15/10/2026: net 424 B against 552 B on the page, 77%, so the length rule above let the stream win and
            // the card was lost). Words the page shows and the stream lacks mean the page has more: it wins. Only for
            // providers whose reader is known to miss cards: elsewhere the words missing from the stream are the
            // source names and chips the page adds around an answer that is complete.
            let cards = st == "done" && NET_WITHOUT_CARDS.contains(&k.as_str()) && d.as_deref().map(|dom| page_has_cards(&a.text, dom)).unwrap_or(false);
            if cards {
                debug::log(format!("kotodama NETREAD skipped key={k}: the page shows words the stream lacks (cards), the page wins"));
            }
            let ui = k == "openai" && dom_len > 0 && crate::netread::carries_generated_ui(&a.text);
            if ui {
                debug::log(format!("kotodama NETREAD skipped key={k}: generated interface, the page wins (dom={dom_len}B)"));
            }
            ok && !ui && !cards
        });
        if let Some(a) = net {
            from_net = true;
            debug::log(format!(
                "kotodama NETREAD key={k} status={st}->done net={}B dom={}B reasoning={}B sources={} skipped={}",
                a.text.len(), d.as_deref().map(str::len).unwrap_or(0), a.reasoning.len(), a.sources.len(), a.skipped
            ));
            st = "done".to_string();
            d = Some(a.text.clone());
            md = Some(a.text.clone());
            if !a.sources.is_empty() || !a.reasoning.is_empty() {
                let _ = window.emit(
                    "app://kotodama-extras",
                    serde_json::json!({ "broadcastId": b, "key": k, "sources": a.sources, "reasoning": a.reasoning }),
                );
            }
        }
    }
    if st == "done" && s == Some(0) && se == Some(false) && !from_net && !already_sent(&b, &k) {
        debug::log(format!("kotodama done REFUSED key={k} bid={b}: never sent, no response stream -> sendfail"));
        st = "sendfail".to_string();
        d = Some(String::new());
        md = Some(String::new());
    }
    // One line per delivered answer: was a human check involved. Only the final delivery (s = 0) counts,
    // never diagnostics or previews.
    if s == Some(0) && st != "diag" && st != "partial" && st != "progress" {
        record_human_check(&window, &k, &st, cp.unwrap_or(false) || st == "captcha");
    }
    if crate::debug::enabled() && (st == "diag" || s == Some(0)) {
        // Log-once-per-delivery confirmation that the direct-IPC path is actually being used (vs.
        // the navigation-sentinel fallback) — useful to know per-provider if this ever needs
        // diagnosing (e.g. a provider whose CSP blocks the Tauri bridge would silently fall back).
        debug::log(format!("kotodama_push (IPC) key={k} st={st}"));
    }
    handle_push(&window, b, k, st, s, n, d, len, tr.unwrap_or(false), md);
}

/// Appends `{ts, key, status, check}` to `provider-checks.jsonl` in the app config dir: how often each
/// provider puts a human check in front of our sends. The data to reason about the cause (is it asked to
/// everyone, or triggered by how the message is sent) instead of guessing. Small and local; never sent
/// anywhere. Failures are ignored: a statistics line must never affect an answer.
fn record_human_check(window: &Window, key: &str, status: &str, check: bool) {
    use std::io::Write;
    let Ok(dir) = window.app_handle().path().app_config_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = serde_json::json!({ "ts": ts, "key": key, "status": status, "check": check });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("provider-checks.jsonl")) {
        let _ = writeln!(f, "{line}");
    }
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
    let mut data = data;
    let mut md = md;
    if take_attach_failure(&bid, &key, &st, &mut data) {
        md = Some(String::new());
    }
    // Network capture from the discovery probe (KOTO_NETPROBE): one JSON record per line, per provider.
    if st == "netcap" {
        if debug::enabled() {
            debug::netcap(&key, &bid, data.as_deref().unwrap_or_default());
        }
        return;
    }
    if st == "audio" {
        handle_audio_push(window, &key, data.as_deref().unwrap_or_default());
        return;
    }
    if st == "diag" {
        // DOM census from a stuck harvest: log-only, this is how provider selectors get tuned.
        debug::log(format!("kotodama DIAG key={key}: {}", data.unwrap_or_default()));
        return;
    }
    // The fill loop announces that it pressed Enter. From here on NOBODY may send the same message
    // again, not even if the page navigates and the script is re-injected.
    // A block that needs the user, or its end: not an outcome, the card keeps waiting (see syncBlock in
    // HARVEST_JS). The UI shows it and queues the pages to open.
    // A batch of the answer stream copied by the page observer: fed to the provider's reader.
    if st == "net" {
        let Some(raw) = data else { return };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { return };
        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("");
        let chunk = v.get("data").and_then(|x| x.as_str()).unwrap_or("");
        let end = v.get("end").and_then(|x| x.as_bool()).unwrap_or(false);
        let still_pending = broadcasts().lock().unwrap().get(&bid).map(|bc| bc.pending.contains(&key)).unwrap_or(false);
        if !still_pending {
            return; // answer already delivered: a long-lived socket keeps sending, nothing to read any more
        }
        let mut map = net_readers().lock().unwrap();
        let readers = map.entry((bid.clone(), key.clone())).or_default();
        if !readers.contains_key(&id) {
            match crate::netread::reader_for(&key, url) {
                Some(r) => {
                    debug::log(format!("kotodama NET reader key={key} id={id} url={}", url.chars().take(90).collect::<String>()));
                    readers.insert(id.clone(), r);
                }
                None => {
                    debug::log(format!("kotodama NET no reader key={key} url={}", url.chars().take(90).collect::<String>()));
                    return;
                }
            }
        }
        let mut orphan_finish = false;
        if let Some(r) = readers.get_mut(&id) {
            let before = r.working();
            if !chunk.is_empty() {
                r.feed(chunk);
            }
            if end {
                r.end();
                debug::log(format!("kotodama NET stream closed key={key} id={id} text={}B done={}", r.answer().text.len(), r.answer().done));
            }
            let after = r.working();
            if after.is_some() && after != before {
                net_work_owner().lock().unwrap().insert((bid.clone(), key.clone()), id.clone());
            }
            // A stream without an end marker of its own closed with an answer: if the page never delivers it,
            // the stream does, much later (see NET_ORPHAN_GRACE).
            if end && !r.done_is_explicit() && r.answer().done && !r.answer().text.trim().is_empty()
                && NET_ORPHAN_KEYS.contains(&key.as_str())
            {
                orphan_finish = true;
            }
        }
        // Live preview straight from the provider's own stream: the text as the model writes it, in its own
        // Markdown, without waiting for the page to paint it. The page observer keeps feeding its own preview
        // (providers whose stream we cannot read still depend on it) and the interface keeps whichever is
        // longer, so the two cannot fight. Throttled: a stream produces far more frames than a screen can show.
        let text = readers.values().map(|r| r.answer().text.clone()).max_by_key(|t| t.len()).unwrap_or_default();
        // Only a stream that says "finished" by itself may close the answer (today ChatGPT's, whose page no
        // longer exposes the answer): readers that mark `done` when the connection closes would hand out a
        // truncated answer as complete.
        let stream_done =
            readers.values().any(|r| r.done_is_explicit() && r.answer().done && !r.answer().text.trim().is_empty());
        let owner = net_work_owner().lock().unwrap().get(&(bid.clone(), key.clone())).cloned();
        let states: Vec<(String, Option<bool>)> = readers.iter().map(|(i, r)| (i.clone(), r.working())).collect();
        let work = pick_working(&states, owner.as_deref());
        if debug::enabled() && readers.values().any(|r| r.working().is_some()) {
            let mut v: Vec<String> = readers.iter().map(|(i, r)| format!("{i}={:?}/{}", r.working(), r.answer().done)).collect();
            v.sort();
            let v = v.join(" ");
            let mut last = net_work_dbg().lock().unwrap();
            if last.get(&(bid.clone(), key.clone())) != Some(&v) {
                debug::log(format!("kotodama NET readers key={key} {v}"));
                last.insert((bid.clone(), key.clone()), v);
            }
        }
        drop(map);
        tell_page_work(window, &bid, &key, work);
        // Armed only while the answer is still owed: a stream frame arriving after delivery would otherwise leave
        // an entry that nothing removes (finish_key, which clears it, has already run).
        let still_owed = broadcasts().lock().unwrap().get(&bid).map(|bc| bc.pending.contains(&key)).unwrap_or(false);
        if stream_done && still_owed && net_finish_armed().lock().unwrap().insert((bid.clone(), key.clone())) {
            schedule_net_finish(window.clone(), bid.clone(), key.clone(), NET_FINISH_GRACE);
        } else if orphan_finish && still_owed && net_finish_armed().lock().unwrap().insert((bid.clone(), key.clone())) {
            schedule_net_finish(window.clone(), bid.clone(), key.clone(), NET_ORPHAN_GRACE);
        }
        if !text.trim().is_empty() {
            let now = std::time::Instant::now();
            let send = {
                let mut seen = net_partials().lock().unwrap();
                let e = seen
                    .entry((bid.clone(), key.clone()))
                    .or_insert((now - std::time::Duration::from_secs(1), 0));
                let ok = text.len() > e.1 && now.duration_since(e.0) >= std::time::Duration::from_millis(120);
                if ok {
                    // One line the first time only: says whether this provider's answer is being previewed from
                    // the network at all, without logging the hot path.
                    if e.1 == 0 {
                        debug::log(format!("kotodama NETLIVE key={key} first preview {}B", text.len()));
                    }
                    *e = (now, text.len());
                }
                ok
            };
            if send {
                let _ = window.emit(
                    "app://kotodama-partial",
                    serde_json::json!({ "broadcastId": bid, "key": key, "text": text, "md": text }),
                );
            }
        }
        return;
    }
    // Genuine input requested by the fill script (see browser::trusted_input).
    if st == "trusted-fill" || st == "trusted-enter" {
        debug::log(format!("kotodama {st} key={key}"));
        browser::trusted_input(window, &key, &st, &data.unwrap_or_default());
        return;
    }
    if st == "blocked" || st == "unblocked" {
        let reason = data.unwrap_or_default();
        {
            let mut marks = blocked_marks().lock().unwrap();
            let mut logins = login_marks().lock().unwrap();
            if st == "blocked" {
                marks.insert((bid.clone(), key.clone()));
                if reason == "login" {
                    logins.insert((bid.clone(), key.clone()));
                } else {
                    logins.remove(&(bid.clone(), key.clone()));
                }
            } else {
                marks.remove(&(bid.clone(), key.clone()));
                logins.remove(&(bid.clone(), key.clone()));
            }
        }
        debug::log(format!("kotodama {st} key={key} bid={bid} reason={reason}"));
        let _ = window.emit(
            "app://kotodama-blocked",
            serde_json::json!({ "broadcastId": bid, "key": key, "blocked": st == "blocked", "reason": reason }),
        );
        return;
    }
    if st == "sent" {
        debug::log(format!("kotodama SENT key={key} bid={bid} -- no further send allowed"));
        // The UI times "sent -> first words" from this instant: the real send in the provider page, so a
        // cold page still loading does not count against the provider.
        let _ = window.emit("app://kotodama-sent", serde_json::json!({ "broadcastId": bid, "key": key }));
        sent_marks().lock().unwrap().insert((bid, key));
        return;
    }
    // Live preview of an answer still being written: straight to the UI, no buffering and no logging
    // (this is the hot path, several pushes a second per provider). It never finishes a key and never
    // touches the delivery state; the frontend ignores it once the card holds its final answer.
    if st == "partial" {
        let _ = window.emit(
            "app://kotodama-partial",
            serde_json::json!({ "broadcastId": bid, "key": key, "text": data.unwrap_or_default(), "md": md.unwrap_or_default() }),
        );
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
                forget_incognito_conversation(&wv, &key, &url);
                if wv.navigate(parsed).is_ok() {
                    set_conv(&window, &key, None);
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

/// The broadcast (if any) that still waits for an answer from this provider's tab.
fn owed_broadcast(key: &str) -> Option<String> {
    let waiting = |bid: &str| broadcasts().lock().unwrap().get(bid).map(|bc| bc.pending.contains(key)).unwrap_or(false);
    if let Some((bid, _, _)) = active_harvests().lock().unwrap().get(key).cloned() {
        if waiting(&bid) {
            return Some(bid);
        }
    }
    let queued = pending_injections().lock().unwrap().get(key).map(|i| i.broadcast_id.clone());
    queued.filter(|bid| waiting(bid))
}

/// Blocks announced by Rust (the tab left the provider's site), as opposed to the page's own `blocked` push. Only
/// these are lifted when the tab is back on the site.
fn offsite_blocks() -> &'static Mutex<HashSet<(String, String)>> {
    static S: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// How long a tab must stay off the provider's site before it counts as a sign-in wall. Technical hops (Google's
/// account step for a connected Gemini, a session renewal, a payment page) are back within a second or two and
/// must not show "sign in" nor, worse, make the resume send the message again.
const OFFSITE_LOGIN_GRACE: std::time::Duration = std::time::Duration::from_secs(4);

/// The tab left the provider's site while a broadcast waits on it: after `OFFSITE_LOGIN_GRACE`, if it is still
/// away, the broadcast is marked blocked by a sign-in (same state and event as the page's own `blocked` push,
/// so the card and the watchdog treat it alike). Once per block. The "already sent" mark is NOT touched: if the
/// message went out before the hop it must not go out again (Mistral keeps the prompt itself across its
/// sign-in); `login_marks` is set only for a message that never left, so the resume sends it, once.
fn announce_login_block<R: Runtime>(app: &tauri::AppHandle<R>, key: &str) {
    let Some(bid) = owed_broadcast(key) else { return };
    let app = app.clone();
    let key = key.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(OFFSITE_LOGIN_GRACE);
        let still_away = app
            .get_webview(&browser::provider_label(&key))
            .map(|w| !on_provider_site(w.url().ok().as_ref()))
            .unwrap_or(false);
        // the same broadcast must still be the one waiting
        if !still_away || owed_broadcast(&key).as_deref() != Some(bid.as_str()) {
            return;
        }
        let pair = (bid.clone(), key.clone());
        if !offsite_blocks().lock().unwrap().insert(pair.clone()) {
            return;
        }
        blocked_marks().lock().unwrap().insert(pair.clone());
        if !already_sent(&bid, &key) {
            login_marks().lock().unwrap().insert(pair);
        }
        debug::log(format!("kotodama blocked key={key} bid={bid} reason=login (tab left for a sign-in page)"));
        let _ = app.emit(
            "app://kotodama-blocked",
            serde_json::json!({ "broadcastId": bid, "key": key, "blocked": true, "reason": "login" }),
        );
    });
}

/// The tab is back on the provider's site: lifts a block announced by `announce_login_block`.
fn lift_offsite_block<R: Runtime>(app: &tauri::AppHandle<R>, bid: &str, key: &str) {
    let pair = (bid.to_string(), key.to_string());
    if !offsite_blocks().lock().unwrap().remove(&pair) {
        return;
    }
    blocked_marks().lock().unwrap().remove(&pair);
    login_marks().lock().unwrap().remove(&pair);
    debug::log(format!("kotodama unblocked key={key} bid={bid} reason=login (tab back on the provider site)"));
    let _ = app.emit(
        "app://kotodama-blocked",
        serde_json::json!({ "broadcastId": bid, "key": key, "blocked": false, "reason": "login" }),
    );
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
    // Off the provider's own site (a sign-in step on another domain): nothing of ours runs here. A queued send or
    // an owed harvest stays as it is and resumes on the page-load that brings the tab back.
    if !on_provider_site(webview.url().ok().as_ref()) {
        debug::log(format!("kotodama page_finished key={key}: off the provider site, sends and harvests held"));
        // A broadcast is waiting on this provider and its tab left for another site: that is a sign-in page (Mistral,
        // measured 15/10/2026: the send bounced to v2.auth.mistral.ai/login and the card stayed silent for 200 s, then
        // "error"). Say "login" for every provider, but only if the tab is still away after a few seconds (a technical
        // hop must not look like a sign-in), and without ever sending a message again that had already gone out;
        // coming back to the provider's site lifts it (see the resume below).
        announce_login_block(&webview.app_handle(), key);
        return;
    }
    let inj = pending_injections().lock().unwrap().remove(key);
    if let Some(inj) = inj {
        lift_offsite_block(&webview.app_handle(), &inj.broadcast_id, key);
        debug::log(format!("kotodama inject (on load) key={key} bid={}", inj.broadcast_id));
        match build_inject_js(&inj.broadcast_id, key, &inj.text, inj.fresh, inj.temp) {
            Ok(js) => {
                let _ = webview.eval(&js);
                active_harvests()
                    .lock()
                    .unwrap()
                    .insert(key.to_string(), (inj.broadcast_id.clone(), inj.text.clone(), inj.temp));
            }
            Err(e) => debug::log(format!("kotodama inject build error: {e}")),
        }
        return;
    }
    // No queued injection: if a harvest is still owed for this key, the page must have
    // NAVIGATED after the send (Qwen landing -> chat, ChatGPT /?q= -> /c/<id>), killing the
    // injected script. Resume with a harvest-only script in the new document.
    let resume = active_harvests().lock().unwrap().get(key).cloned();
    if let Some((bid, text, temp)) = resume {
        let still_pending = broadcasts()
            .lock()
            .unwrap()
            .get(&bid)
            .map(|bc| bc.pending.contains(key))
            .unwrap_or(false);
        if still_pending {
            // Back on the provider's site after a sign-in wall: the block is over as far as we know (a wall
            // still there is pushed again by the resumed script), and the message goes out again, once.
            let pair = (bid.clone(), key.to_string());
            lift_offsite_block(&webview.app_handle(), &bid, key);
            if login_marks().lock().unwrap().remove(&pair) {
                blocked_marks().lock().unwrap().remove(&pair);
                sent_marks().lock().unwrap().remove(&pair);
                debug::log(format!("kotodama unblocked key={key} bid={bid} reason=login (page back after sign-in)"));
                let _ = webview.app_handle().emit(
                    "app://kotodama-blocked",
                    serde_json::json!({ "broadcastId": bid, "key": key, "blocked": false, "reason": "login" }),
                );
            }
            let allow_send = !already_sent(&bid, key);
            debug::log(format!(
                "kotodama RESUME after nav key={key} bid={bid} resend={}",
                if allow_send { "YES (never went out)" } else { "NO (already sent)" }
            ));
            if let Ok(js) = build_resume_js(&bid, key, &text, allow_send, temp) {
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
        if !here.is_empty() && !unreliable_prewarm(key) {
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
        // A page left dirty by an attachment failure is never typed into again: this send navigates for real.
        let new_chat = new_chat || dirty_pages().lock().unwrap().remove(key.as_str());
        // The page is about to move on from the answer it shows: read aloud is no longer about that answer.
        set_conv(&window, key, None);
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
        // The tab is on another site, typically the user signing in: neither type into that page nor navigate it away
        // (that would throw the sign-in away). Queue the send; it runs when the page is back on the provider's site.
        if let Some(webview) = &existing {
            if !on_provider_site(webview.url().ok().as_ref()) {
                debug::log(format!("kotodama send key={key} held: the tab is off the provider site"));
                pending_injections().lock().unwrap().insert(
                    key.clone(),
                    PendingInjection { broadcast_id: broadcast_id.clone(), text: text.clone(), fresh: new_chat, temp: new_chat && temp_for(key) },
                );
                continue;
            }
        }
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
                            .insert(key.clone(), (broadcast_id.clone(), text.clone(), false));
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
                                .insert(key.clone(), (broadcast_id.clone(), text.clone(), false));
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
            forget_incognito_conversation(&webview, key, base);
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
                // Same rule as on_page_finished: a tab that went to a sign-in page keeps its send queued for the
                // page-load that brings it back (measured: this fallback injected into accounts.x.ai).
                let off_site = win
                    .get_webview(&browser::provider_label(&key))
                    .map(|w| !on_provider_site(w.url().ok().as_ref()))
                    .unwrap_or(false);
                if off_site {
                    debug::log(format!("kotodama inject (fallback 8s) key={key} held: off the provider site"));
                    return;
                }
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
                            .insert(key.clone(), (inj.broadcast_id.clone(), inj.text.clone(), inj.temp));
                    } else {
                        finish_key(&win, &inj.broadcast_id, &key, "error", "", false, "");
                    }
                }
            });
        }
    }
    // Watchdog: whatever is still pending for this bid after 200s becomes an error card
    // (covers pages that never load, harvest scripts killed by an unload...). A provider that is
    // BLOCKED waiting for the user is spared while the block lasts, and gets 200s more once it clears;
    // 25 minutes is the absolute ceiling (the page script gives up on a block at 20).
    {
        let win = window.clone();
        let bid = broadcast_id.clone();
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut deadline: HashMap<String, Instant> = HashMap::new();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let pending: Vec<String> = broadcasts()
                    .lock()
                    .unwrap()
                    .get(&bid)
                    .map(|bc| bc.pending.iter().cloned().collect())
                    .unwrap_or_default();
                if pending.is_empty() {
                    break;
                }
                let now = Instant::now();
                let hard_stop = now.duration_since(start) > std::time::Duration::from_secs(25 * 60);
                for key in pending {
                    let blocked = blocked_marks().lock().unwrap().contains(&(bid.clone(), key.clone()));
                    let due = deadline
                        .entry(key.clone())
                        .or_insert(start + std::time::Duration::from_secs(200));
                    if blocked {
                        *due = now + std::time::Duration::from_secs(200);
                    }
                    if now >= *due || hard_stop {
                        debug::log(format!("kotodama watchdog: bid={bid} key={key} silent"));
                        pending_injections().lock().unwrap().remove(&key);
                        chunk_bufs().lock().unwrap().remove(&(bid.clone(), key.clone()));
                        blocked_marks().lock().unwrap().remove(&(bid.clone(), key.clone()));
                        finish_key(&win, &bid, &key, "error", "", false, "");
                    }
                }
            }
        });
    }
    Ok(())
}

/// Cancels a broadcast: every still-pending key gets a `cancelled` card; the injected
/// JS loops self-expire on their own timeouts (their deliveries will find nothing here).
#[tauri::command]
pub fn kotodama_cancel(window: Window, broadcast_id: String) -> Result<(), String> {
    attachments().lock().unwrap().remove(&broadcast_id);
    let stuck: Vec<String> = broadcasts()
        .lock()
        .unwrap()
        .get(&broadcast_id)
        .map(|bc| bc.pending.iter().cloned().collect())
        .unwrap_or_default();
    for key in stuck {
        pending_injections().lock().unwrap().remove(&key);
        chunk_bufs().lock().unwrap().remove(&(broadcast_id.clone(), key.clone()));
        stop_provider_page(&window, &key);
        finish_key(&window, &broadcast_id, &key, "cancelled", "", false, "");
    }
    Ok(())
}

/// Structural CSS selectors of a provider's own "stop generating" control, tried in order before the generic
/// search in `STOP_JS`. Measured live on 14/10/2026 (never the visible label, which follows the UI language):
/// Claude `data-testid="chat-input-stop"`; Gemini's send button host gets the class `stop` while it writes.
/// ChatGPT dropped `data-testid="stop-button"` that day (the control has only an aria-label), Grok and Perplexity
/// have no stable attribute at all: those are found by the solid-square icon, in `STOP_JS`.
fn stop_selectors(key: &str) -> &'static str {
    match key {
        "openai" => r#"button[data-testid="stop-button"]"#,
        // (Claude's other layout has no test id: STOP_JS looks for its `aria-keyshortcuts="Escape"` button NEXT TO
        // THE COMPOSER, see `find()`.)
        "anthropic" => r#"[data-testid="chat-input-stop"]"#,
        "gemini" => r#".send-button.stop button"#,
        "zai" => r#"button.rounded-full.bg-black"#,
        "kimi" => r#".send-button-container.stop"#,
        "meta" => r#"[data-testid="composer-stop-button"]"#,
        _ => "",
    }
}

/// Presses the provider's own stop control on its page and disarms our scripts there. Without this a "Stop" in
/// Kotodama only closed Kotodama's card: the provider kept writing, and the next message's harvest saw the
/// previous answer still changing and delivered that text as its own (measured: 19764 bytes for "reply only OK").
/// A provider with no stop control found is logged, not guessed at.
fn stop_provider_page(window: &Window, key: &str) {
    let Some(wv) = window.get_webview(&browser::provider_label(key)) else { return };
    browser::resume_provider(window, key, true);
    let js = format!(
        "var __kt_stop_sel = {}; var __kt_stop_key = {}; var __kt_stop_diag = {};{}",
        serde_json::to_string(stop_selectors(key)).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into()),
        crate::debug::enabled(),
        STOP_JS
    );
    if wv.eval(&js).is_err() {
        debug::log(format!("kotodama stop key={key}: eval failed"));
    }
}

/// Runs in the provider page. (1) Disarms our own loops: the harvest and the fill loop each stop when their id is
/// no longer the current one, so a send that has not gone out yet cannot go out after Stop. (2) Clicks the
/// provider's stop control: the provider's structural selector, then any button whose test id names "stop", then
/// the icon next to the composer: a sprite whose id names "stop" (Perplexity), or a single filled square (ChatGPT,
/// Grok; copy/menu icons are drawn with several shapes, a send arrow is not filled).
const STOP_JS: &str = r##"
(function(){
  try { window.__ktBid = '__ktCancelled'; window.__ktFillRun = '__ktCancelled'; } catch(e){}
  function ok(b){
    if (!b || b.offsetParent === null || b.disabled || b.getAttribute('aria-disabled') === 'true') return false;
    var r = b.getBoundingClientRect(); return r.width > 0 && r.height > 0;
  }
  function firstVisible(sel){
    if (!sel) return null;
    try { var l = document.querySelectorAll(sel); for (var i = 0; i < l.length; i++) if (ok(l[i])) return l[i]; } catch(e){}
    return null;
  }
  function composer(){
    var sels = ['textarea:not([readonly]):not([aria-hidden="true"])', '[contenteditable="true"]', 'div[role="textbox"]'], best = null;
    for (var i = 0; i < sels.length; i++) {
      var l = document.querySelectorAll(sels[i]);
      for (var j = 0; j < l.length; j++) if (l[j].offsetParent !== null) best = l[j];
      if (best) return best;
    }
    return null;
  }
  // Sprite icons (`<use href="#...-stop-filled">`, Perplexity) carry no shape to measure, but the symbol id is an
  // internal name, not UI text, so it does not change with the language.
  function stopNamedIcon(btn){
    var u = btn.querySelectorAll('svg use');
    for (var i = 0; i < u.length; i++) {
      var h = (u[i].getAttribute('href') || u[i].getAttribute('xlink:href') || '');
      if (/stop/i.test(h)) return true;
    }
    return false;
  }
  // True when the button's icon is ONE shape that fills (almost) its whole square box.
  function solidSquare(b){
    var svg = b.querySelector('svg'); if (!svg) return false;
    var shapes = svg.querySelectorAll('path,rect,circle,polygon');
    if (shapes.length !== 1) return false;
    var e = shapes[0];
    if (e.tagName === 'rect') { var w = +e.getAttribute('width'), h = +e.getAttribute('height'); return !!(w && h && Math.abs(w / h - 1) < .1); }
    if (e.tagName !== 'path') return false;
    try {
      var bb = e.getBBox(); if (!bb.width || Math.abs(bb.width / bb.height - 1) > .15) return false;
      var cv = document.createElement('canvas').getContext('2d'), P = new Path2D(e.getAttribute('d') || ''), n = 0;
      for (var a = 0; a < 20; a++) for (var c = 0; c < 20; c++)
        if (cv.isPointInPath(P, bb.x + (a + .5) * bb.width / 20, bb.y + (c + .5) * bb.height / 20)) n++;
      return n / 400 > .88;
    } catch(x){ return false; }
  }
  function find(){
    var how = 'provider-selector', b = firstVisible(__kt_stop_sel);
    if (!b) { how = 'testid'; b = firstVisible('button[data-testid*="stop" i], [role="button"][data-testid*="stop" i]'); }
    // Claude's layout without a test id (15/10/2026, served side by side with the other): the interrupt button
    // carries `aria-keyshortcuts="Escape"` (a protocol attribute, not a label). Only in the composer's own band,
    // beside the field, and never inside a dialog or panel: other controls advertise Escape too (close, deny).
    if (!b) {
      how = 'shortcut';
      var cz = composer(), czr = cz ? cz.getBoundingClientRect() : null;
      if (czr) {
        var ks = document.querySelectorAll('button[aria-keyshortcuts="Escape"]');
        for (var ki = 0; ki < ks.length && !b; ki++) {
          var kb = ks[ki]; if (!ok(kb)) continue;
          if (kb.closest('[role="dialog"], [role="alertdialog"], [aria-modal="true"], dialog, aside, nav')) continue;
          var kr = kb.getBoundingClientRect(), ky = kr.top + kr.height / 2, kx = kr.left + kr.width / 2;
          if (ky < czr.top - 40 || ky > czr.bottom + 60 || kx < czr.left - 20 || kx > czr.right + 20) continue;
          b = kb;
        }
      }
    }
    // The icon search only once our message has gone out (or something is being written): at rest the slot holds
    // other controls (voice, attach), and a square-ish one there must not be pressed (review of 14/10/2026).
    if (!b && busy()) {
      how = 'icon';
      // The send/stop slot is often outside the editable element itself (ChatGPT: 166px to its right), so the box is
      // the enclosing form when there is one, else the editable grown sideways.
      var c = composer(), f = c && c.closest('form'), cr = c ? (f || c).getBoundingClientRect() : null;
      if (cr) {
        var all = document.querySelectorAll('button,[role="button"]');
        for (var i = 0; i < all.length && !b; i++) {
          var x = all[i]; if (!ok(x)) continue;
          var r = x.getBoundingClientRect(), cx = r.left + r.width / 2, cy = r.top + r.height / 2;
          // Near the composer only: the page has other square-ish icons (sidebar, message actions).
          if (cy < cr.top - 100 || cy > cr.bottom + 100 || cx < cr.left - 60 || cx > cr.right + (f ? 60 : 300)) continue;
          if (stopNamedIcon(x) || solidSquare(x)) b = x;
        }
      }
    }
    return b ? { b: b, how: how } : null;
  }
  // a send in the last 20 s counts as "out": __ktSentAt stays from earlier turns, an old one must not open the search
  function busy(){ return (window.__ktStreamOpen || 0) > 0 || (!!window.__ktSentAt && Date.now() - window.__ktSentAt < 20000); }
  function report(msg){
    try { if (__kt_stop_diag && window.__ktPush) window.__ktPush({ b: '__ktCancelled', k: __kt_stop_key, st: 'diag', d: 'STOP ' + msg }); } catch(e){}
  }
  function press(h){ try { h.b.click(); window.__ktStopHold = Date.now(); window.__ktStopHoldLogged = false; } catch(e){} }
  var hit = find();
  if (hit) press(hit);
  report(hit ? 'clicked via ' + hit.how : 'no stop control found yet');
  // A click is not always taken: Gemini ignores it while it is still "thinking" (measured 14/10/2026, Stop pressed
  // ~10 s in: the control stayed and the whole essay arrived anyway). So the control is looked for again, every
  // second, and pressed again while it is still there. It ends when it is gone, after 10 s, or as soon as another
  // send/harvest takes over the page (it must never stop the NEXT answer).
  // Pressed in the second between our send and the provider's first words, the control may not be there yet (measured
  // on ChatGPT 14/10/2026, Stop at 1.5 s: nothing found, the essay arrived anyway): it is looked for again every
  // second while our message is out, until it appears.
  var tries = 0, iv = setInterval(function(){
    if (window.__ktBid !== '__ktCancelled') { clearInterval(iv); report('next send took the page over after ' + tries + ' retries'); return; }
    if (++tries > 10) { clearInterval(iv); report('gave up after ' + (tries - 1) + ' retries'); return; }
    var again = find();
    if (!again) {
      if (hit) { clearInterval(iv); report('stopped after ' + tries + ' retries'); }
      else if (!busy()) { clearInterval(iv); report('nothing was sent: nothing to stop'); }
      return;
    }
    if (!hit) { hit = again; press(again); report('clicked via ' + again.how + ' after ' + tries + ' s'); return; }
    // an icon match is pressed again only while something is still being written: a control that merely looks like a
    // stop button does not vanish after a click, and must not be pressed ten times
    if (again.how === 'icon' && !((window.__ktStreamOpen || 0) > 0)) { clearInterval(iv); report('stream closed after ' + tries + ' retries'); return; }
    try { again.b.click(); } catch(e){}
  }, 1000);
})();
"##;

#[cfg(test)]
mod provider_hosts_tests {
    /// PROVIDER_HOSTS and the IPC grant must name the same hosts: a host missing from either side is a provider
    /// whose results can never arrive, or a page where our scripts would run without being able to deliver.
    #[test]
    fn hosts_match_capability() {
        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/capabilities/provider-push.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let mut cap: Vec<String> = v["remote"]["urls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u.as_str().unwrap().trim_start_matches("https://").trim_end_matches("/*").to_string())
            .collect();
        let mut ours: Vec<String> = super::PROVIDER_HOSTS.iter().map(|h| h.to_string()).collect();
        cap.sort();
        ours.sort();
        assert_eq!(ours, cap);
    }

    #[test]
    fn sign_in_pages_are_off_site() {
        let u = |s: &str| s.parse::<tauri::Url>().unwrap();
        assert!(super::on_provider_site(Some(&u("https://gemini.google.com/app"))));
        assert!(!super::on_provider_site(Some(&u("https://accounts.google.com/v3/signin/identifier"))));
        assert!(!super::on_provider_site(Some(&u("https://accounts.x.ai/sign-in"))));
        assert!(!super::on_provider_site(None));
    }
}

#[cfg(test)]
mod visible_len_tests {
    #[test]
    fn card_text_missing_from_the_stream_is_counted() {
        let net = "Tomorrow I'll go to the market to buy bread, cheese and two bottles of red wine for dinner.";
        // the page adds a title and a note the stream does not carry
        assert!(super::page_has_cards(net, &format!("Standard Translation

{net}
Formality: neutral, suitable for everyday use.")));
        // same text: nothing missing; case, markdown and spacing are not differences
        assert!(!super::page_has_cards(net, net));
        assert!(!super::page_has_cards("**Hello** World `again` | # ok", "hello world AGAIN ok"));
        // a one-word label (code block language) inside a long answer is not a card
        let long = "The function reads the file line by line and returns the number of words it found in it. ".repeat(6);
        assert!(!super::page_has_cards(&long, &format!("Python
{long}")));
    }

    #[test]
    fn numbered_lists_are_not_a_card() {
        // the stream carries "1. " at the start of each item, the page's text does not
        let net = "Ecco i passaggi:
1. Taglia le verdure in cubetti piccoli
2. Falle rosolare con olio e cipolla
3. Aggiungi il brodo e cuoci a lungo
4. Servi caldo con un filo d'olio
5. Conserva il resto in frigorifero";
        let page = "Ecco i passaggi:
Taglia le verdure in cubetti piccoli
Falle rosolare con olio e cipolla
Aggiungi il brodo e cuoci a lungo
Servi caldo con un filo d'olio
Conserva il resto in frigorifero";
        assert!(!super::page_has_cards(net, page));
        // and a card is still seen on top of a list
        assert!(super::page_has_cards(net, &format!("Ricetta della nonna, versione veloce
{page}
Nota: si conserva due giorni.")));
    }

    #[test]
    fn markdown_in_a_language_without_spaces_is_not_a_difference() {
        // the stream's bold and inline code against the page's plain text: the stream was right
        assert!(!super::page_has_cards("東京は**日本の首都**です。`API`を使います。", "東京は日本の首都です。APIを使います。"));
        // a card title the stream lacks is still caught
        assert!(super::page_has_cards("東京は日本の首都です。", "標準の翻訳
東京は日本の首都です。"));
    }

    #[test]
    fn link_addresses_do_not_count() {
        assert_eq!(super::visible_len("plain text"), 10);
        assert_eq!(super::visible_len("[Idealo](https://www.idealo.it/a/b.html) e"), "[Idealo] e".len());
        assert_eq!(super::visible_len("| [A](http://x) | [B](http://y) |"), "| [A] | [B] |".len());
        // an unclosed address is counted as it is
        assert_eq!(super::visible_len("[A](http://x"), "[A](http://x".len());
    }
}

#[cfg(test)]
mod stop_tests {
    /// The stop control must be found without reading the interface language: no selector may name a visible label,
    /// and the script may not look at the text of any element.
    #[test]
    fn stop_lookup_is_language_independent() {
        for key in ["openai", "anthropic", "gemini", "grok", "perplexity", "zai", "kimi", "meta", "deepseek", "unknown"] {
            let sel = super::stop_selectors(key);
            assert!(!sel.contains("aria-label"), "{key}: {sel}");
            assert!(!sel.contains(":contains"), "{key}: {sel}");
        }
        for banned in ["innerText", "textContent", "aria-label", "title="] {
            assert!(!super::STOP_JS.contains(banned), "STOP_JS reads {banned}");
        }
    }

    /// Stop must disarm the page's own loops (no send after Stop) before pressing the provider's control, and the
    /// retry must end when another send takes the page over (it may never stop the next answer).
    #[test]
    fn stop_disarms_then_presses_and_retry_yields() {
        let js = super::STOP_JS;
        let disarm = js.find("window.__ktBid = '__ktCancelled'").expect("disarms the harvest");
        assert!(js.contains("window.__ktFillRun = '__ktCancelled'"));
        assert!(disarm < js.find(".click()").expect("presses the control"));
        assert!(js.contains("window.__ktBid !== '__ktCancelled'"), "retry must yield to a newer injection");
    }

    /// Providers whose stop control has a stable structural hook keep it in the table (measured 14/10/2026).
    #[test]
    fn stop_table_has_measured_providers() {
        assert_eq!(super::stop_selectors("anthropic"), r#"[data-testid="chat-input-stop"]"#);
        assert!(super::STOP_JS.contains(r#"button[aria-keyshortcuts="Escape"]"#));
        assert_eq!(super::stop_selectors("gemini"), ".send-button.stop button");
    }
}

#[cfg(test)]
mod net_work_tests {
    use super::pick_working;
    fn st(v: &[(&str, Option<bool>)]) -> Vec<(String, Option<bool>)> {
        v.iter().map(|(i, w)| (i.to_string(), *w)).collect()
    }

    /// Two streams open together: the one that followed the answer says "stopped" and an older one never saw the idle
    /// state. The end must not wait for the old one.
    #[test]
    fn the_stream_that_spoke_last_decides() {
        assert_eq!(pick_working(&st(&[("t1", Some(true)), ("t2", Some(false))]), Some("t2")), Some(false));
        assert_eq!(pick_working(&st(&[("t1", Some(false)), ("t2", Some(true))]), Some("t2")), Some(true));
    }

    /// The old rule is the fallback: no owner yet, or the owner closed while writing (no state any more).
    #[test]
    fn without_a_live_owner_any_writing_stream_counts() {
        assert_eq!(pick_working(&st(&[("t1", Some(true)), ("t2", Some(false))]), None), Some(true));
        assert_eq!(pick_working(&st(&[("t1", Some(true)), ("t2", None)]), Some("t2")), Some(true));
        assert_eq!(pick_working(&st(&[("t1", Some(false)), ("t2", None)]), Some("t2")), Some(false));
        assert_eq!(pick_working(&st(&[("t1", None), ("t2", None)]), Some("t1")), None);
        assert_eq!(pick_working(&st(&[]), None), None);
    }
}

#[cfg(test)]
mod send_scope_tests {
    /// What the page streams after a send is delivered must not be forwarded as ours, and the next send must not
    /// inherit the previous one's instant: `__ktSentAt` is what every stream observer tests.
    #[test]
    fn delivery_and_new_injection_clear_the_send_instant() {
        let js = super::HARVEST_JS;
        let d = js.find("function deliver(").expect("deliver");
        let z = js[d..].find("window.__ktSentAt = 0").expect("deliver clears the send instant");
        let end = js[d..].find("window.__ktPush({ b: BID, k: KEY, st: st, s: 0").expect("the delivery push");
        assert!(z < end, "cleared before the result is pushed");
        let inject = super::build_inject_js("b9-test", "openai", "hello", false, false).unwrap();
        assert!(inject.contains("window.__ktSentAt = 0; window.__ktEnterPressed = false;"));
    }

    /// A stream belongs to the send that was current when it OPENED, and its id is unique across documents.
    #[test]
    fn streams_are_bound_to_their_send_and_document() {
        let js = super::STREAM_WATCH_JS;
        assert!(js.contains("var DOC = Math.random()"));
        for id in ["'f' + DOC +", "'x' + DOC +", "'w' + DOC +", "'t' + DOC +"] {
            assert!(js.contains(id), "stream id without the document tag: {id}");
        }
        assert!(js.contains("b: q.bid || __kt_bid"), "frames travel under the id of the send the stream opened in");
        assert!(js.contains("nbid = __kt_bid") && js.contains("xbid = __kt_bid"));
    }

    /// A send marked as out whose message never shows on the page ends as a lost send, in both loops.
    #[test]
    fn a_lost_send_is_an_outcome_not_page_text() {
        let js = super::HARVEST_JS;
        assert!(js.contains("function ownMessageMissing()"));
        assert!(js.contains("EXIT send-lost (arming)") && js.contains("EXIT send-lost (harvest)"));
        // never decided by the visible text of the interface: only by our own message
        let f = js.find("var LOST_SEND_MS").unwrap();
        let body = &js[f..f + 3600];
        assert!(body.contains("function ownProbe()") && body.contains("'box'"));
        // a send declared failed also stops the fill loop
        assert!(js.contains("if (st === 'sendfail') { try { window.__ktFillRun = '__ktCancelled'; }"));
    }
}

/// The attachment contract: nothing leaves without its files, and the page never carries user-facing words.
#[cfg(test)]
mod attach_tests {
    #[test]
    fn page_sends_a_code_not_a_sentence() {
        let js = super::ATTACH_JS;
        assert!(js.contains("d: '@attach'"));
        assert!(js.contains(&format!("'{}'", super::ATTACH_FAIL_CODE)) || js.contains("@attach"));
        assert!(!js.contains("did not reach") && !js.contains("Send it again"), "no English sentence in the page script");
        // the technical reason stays in the debug log
        assert!(js.contains("diag('FAILED ' + why"));
    }

    #[test]
    fn failure_marks_the_flag_stops_fill_and_harvest() {
        let js = super::ATTACH_JS;
        let f = js.find("function fail(why)").unwrap();
        let body = &js[f..f + 1100];
        assert!(body.contains("window.__ktFilesFailed = BID"));
        assert!(body.contains("window.__ktFillRun = '__ktCancelled'"));
        assert!(body.contains("window.__ktBid = null"));
        assert!(body.contains("st: 'sendfail'"));
        // a failure never declares itself on a message that already left, nor from a superseded cycle
        assert!(body.contains("if (!current()) return;") && body.contains("window.__ktEnterPressed"));
        // a finished or failed cycle is not run again for the same broadcast
        assert!(js.contains("window.__ktFilesFailed === __kt_bid"));
    }

    #[test]
    fn cycles_do_not_release_each_other() {
        let js = super::ATTACH_JS;
        assert!(js.contains("window.__ktAttachRun = RUN"));
        let f = js.find("function release(ms)").unwrap();
        assert!(js[f..f + 160].contains("if (current()) window.__ktHoldAttach = false"));
    }

    #[test]
    fn there_is_an_absolute_ceiling_and_an_attempt_cap() {
        let js = super::ATTACH_JS;
        assert!(js.contains("var CEILING_MS = 75000"));
        assert!(js.contains("now - t0 > CEILING_MS"));
        assert!(js.contains("var MAX_ATTEMPTS = 4"));
    }

    #[test]
    fn the_arming_clock_waits_for_the_attach_hold() {
        assert!(super::HARVEST_JS.contains("if (composerVal() !== null && !window.__ktHoldAttach) armTries++;"));
    }

    #[test]
    fn evidence_is_specific_and_scoped() {
        let js = super::ATTACH_JS;
        // blob counting stays inside the composer's region
        assert!(!js.contains("document.querySelectorAll('img[src^=\"blob:\"]')"));
        assert!(js.contains("function gained(before, now)"));
        assert!(js.contains("limit = 8, s = c") && js.contains("snapshot(cc, before && before.scope)"));
    }

    #[test]
    fn generic_names_prove_nothing_and_loading_images_do_not_count() {
        let js = super::ATTACH_JS;
        assert!(js.contains("var GENERIC = /^(image|img|immagine|screenshot"));
        assert!(js.contains("if (GENERIC.test(n.replace("));
        assert!(js.contains("!(e.complete && e.naturalWidth >= 24)"));
        // a re-injection after the failure keeps the fill cancelled
        assert!(js.contains("if (window.__ktFilesFailed === __kt_bid) { try { window.__ktFillRun = '__ktCancelled'; window.__ktBid = null;"));
    }

    #[test]
    fn both_delivery_paths_mark_the_failure_and_strip_the_code() {
        let mut d = Some("@attach".to_string());
        assert!(super::take_attach_failure("b-test", "k-test", "sendfail", &mut d));
        assert_eq!(d.as_deref(), Some(""));
        assert!(super::attach_fail_marks().lock().unwrap().remove(&("b-test".to_string(), "k-test".to_string())));
        let mut other = Some("hello".to_string());
        assert!(!super::take_attach_failure("b-test", "k-test", "sendfail", &mut other));
        assert_eq!(other.as_deref(), Some("hello"));
        assert!(super::attach_fail_marks().lock().unwrap().is_empty() || !super::attach_fail_marks().lock().unwrap().contains(&("b-test".to_string(), "k-test".to_string())));
        // the fallback path really goes through handle_push, where the check lives
        let src = include_str!("kotodama.rs");
        let hp = src.find("fn handle_push(").unwrap();
        assert!(src[hp..hp + 700].contains("take_attach_failure(&bid, &key, &st, &mut data)"));
    }

    #[test]
    fn a_dirty_page_is_taken_once() {
        super::dirty_pages().lock().unwrap().insert("k-dirty".to_string());
        assert!(super::dirty_pages().lock().unwrap().remove("k-dirty"));
        assert!(!super::dirty_pages().lock().unwrap().remove("k-dirty"));
    }

    #[test]
    fn attach_failure_detection() {
        assert!(super::is_attach_failure("sendfail", Some("@attach")));
        assert!(!super::is_attach_failure("sendfail", Some("")));
        assert!(!super::is_attach_failure("done", Some("@attach")));
        assert!(!super::is_attach_failure("sendfail", None));
    }

    /// The script must at least parse: wrapped in a function, checked by node when it is available.
    #[test]
    fn the_script_parses() {
        let dir = std::env::temp_dir();
        let path = dir.join("kt_attach_check.js");
        let src = format!("function __f(__kt_bid, __kt_key){{{}}}", super::ATTACH_JS);
        std::fs::write(&path, src).unwrap();
        match std::process::Command::new("node").arg("--check").arg(&path).output() {
            Ok(o) => assert!(o.status.success(), "node --check: {}", String::from_utf8_lossy(&o.stderr)),
            Err(_) => eprintln!("node not available, syntax check skipped"),
        }
        let _ = std::fs::remove_file(path);
    }
}
