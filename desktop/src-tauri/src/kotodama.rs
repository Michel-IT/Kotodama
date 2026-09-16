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
/// (broadcast, provider) pairs currently waiting on the user (human check, sign-in, window over the
/// composer). The watchdog leaves them alone: the time spent solving a check is not a silent page.
fn blocked_marks() -> &'static Mutex<HashSet<(String, String)>> {
    static S: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}
/// Network readers per (broadcast, provider), one per answer request the page made (netread.rs).
type NetReaders = HashMap<String, Box<dyn crate::netread::Reader>>;
fn net_readers() -> &'static Mutex<HashMap<(String, String), NetReaders>> {
    static S: OnceLock<Mutex<HashMap<(String, String), NetReaders>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}
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
        "var __apb_text = {}; var __kt_bid = {}; var __kt_key = {}; var __kt_ans = {}; var __kt_busy = {}; var __kt_fresh = false; var __kt_fast = {fast}; var __kt_sent = true; var __kt_regen = true; window.__ktDiag = {diag}; window.__ktStreamEver = 0; window.__ktNetUrl = {net_url}; var __kt_regen_sel = {}; var __kt_regen_path = {}; var __kt_regen_item = {};",
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
    active_harvests().lock().unwrap().insert(key.clone(), (new_bid, text));
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
                (cap.file.clone(), ext, 0u64)
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
/// input (as if picked in its dialog), or, when it has none, as a drop on its composer. Holds the text fill
/// until the upload had time to start; the page uploads with its own session as usual.
const ATTACH_JS: &str = r##"
(function(){
  var files = window.__ktFiles;
  if (!files || !files.length || window.__ktFilesDone === __kt_bid) return;
  window.__ktHoldAttach = true;
  function diag(m){ try { if (window.__ktDiag) window.__TAURI__.core.invoke('kotodama_push', { b: __kt_bid, k: __kt_key, st: 'diag', d: 'ATTACH ' + m }).catch(function(){}); } catch(e){} }
  function toFiles(){
    return files.map(function(f){
      var bin = atob(f.b64), u = new Uint8Array(bin.length);
      for (var i = 0; i < bin.length; i++) u[i] = bin.charCodeAt(i);
      return new File([u], f.name, { type: f.mime || 'application/octet-stream' });
    });
  }
  function composer(){
    var sels = ['textarea:not([readonly])', '[contenteditable="true"]', 'div[role="textbox"]'];
    for (var i = 0; i < sels.length; i++) { var e = document.querySelectorAll(sels[i]); for (var j = e.length - 1; j >= 0; j--) if (e[j].offsetParent !== null) return e[j]; }
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
  var t0 = Date.now();
  var iv = setInterval(function(){
    if (window.__ktBid && window.__ktBid !== __kt_bid) { clearInterval(iv); window.__ktHoldAttach = false; return; }
    var c = composer();
    if (!c) { if (Date.now() - t0 > 20000) { clearInterval(iv); window.__ktHoldAttach = false; diag('no composer, text sent without files'); } return; }
    clearInterval(iv);
    var list = toFiles(), dt = new DataTransfer();
    list.forEach(function(f){ dt.items.add(f); });
    var input = fileInput(), how = 'none';
    try {
      if (input) {
        input.files = dt.files;
        input.dispatchEvent(new Event('input', { bubbles: true }));
        input.dispatchEvent(new Event('change', { bubbles: true }));
        how = 'input';
      } else {
        // No file input on the page (captured on Gemini: its input only exists after opening the upload menu).
        // A paste into the composer is what the editor handles for files; a drop is the last resort.
        c.focus();
        var pasted = c.dispatchEvent(new ClipboardEvent('paste', { bubbles: true, cancelable: true, clipboardData: dt }));
        if (pasted) {
          ['dragenter', 'dragover', 'drop'].forEach(function(t){ c.dispatchEvent(new DragEvent(t, { bubbles: true, cancelable: true, dataTransfer: dt })); });
          how = 'drop';
        } else {
          how = 'paste';
        }
      }
    } catch(e) { how = 'error ' + e; }
    window.__ktFilesDone = __kt_bid;
    diag('files=' + list.length + ' via ' + how);
    // Time for the page to take the files and start uploading before the text goes in and Enter is pressed.
    setTimeout(function(){ window.__ktHoldAttach = false; }, 3000 + 1500 * list.length);
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
  window.__ktAdoptBody = function adopt(res, stream){
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
        if (!/event-stream|ndjson|octet-stream|stream|json|text\/plain/i.test(ct) || !res.body || !res.body.tee) return res;
        try {
          var pair = res.body.tee();
          var mine = pair[0].getReader(), bytes = 0, dec = new TextDecoder();
          (function pump(){
            mine.read().then(function(r){
              if (r.done) { cap({ kind: 'fetch', id: id, ev: 'end', bytes: bytes }); return; }
              bytes += (r.value && r.value.length) || 0;
              if (bytes <= PER_REQ) cap({ kind: 'fetch', id: id, ev: 'chunk', data: dec.decode(r.value, { stream: true }).slice(0, PER_CHUNK) });
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
  function ended(){ try { if ((window.__ktStreamOpen || 0) > 0) return; if (window.__ktStreamEnd) window.__ktStreamEnd(); } catch(e){} }
  // ---- Network reading (netread.rs): copies of the ANSWER stream only, forwarded in small batches. The URL
  // pattern comes from Rust per provider (__ktNetUrl); every other request of the page is ignored. IPC only.
  var NET_URL = null;
  try { if (window.__ktNetUrl) NET_URL = new RegExp(window.__ktNetUrl); } catch(e){}
  var netSeq = 0, netQueue = {}, netTimer = null;
  function netWanted(u){ return !!(NET_URL && window.__ktSentAt && NET_URL.test(String(u || ''))); }
  function netFlush(){
    netTimer = null;
    var ids = Object.keys(netQueue);
    for (var i = 0; i < ids.length; i++) {
      var q = netQueue[ids[i]]; delete netQueue[ids[i]];
      try { window.__TAURI__.core.invoke('kotodama_push', { b: __kt_bid, k: __kt_key, st: 'net', d: JSON.stringify({ id: ids[i], url: q.url, data: q.data, end: q.end }) }).catch(function(){}); } catch(e){}
    }
  }
  function netSend(id, url, data, end){
    var q = netQueue[id] || (netQueue[id] = { url: String(url || ''), data: '', end: false });
    if (data) q.data += data;
    if (end) { q.end = true; if (netTimer) clearTimeout(netTimer); netFlush(); return; }
    if (!netTimer) netTimer = setTimeout(netFlush, 60);
  }
  try {
    var of = window.fetch;
    if (typeof of !== 'function') return;
    window.fetch = function(input){
      var p = of.apply(this, arguments);
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
            if (res.status === 429) { try { window.__ktRateLimited = Date.now(); } catch(e){} }
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
            var fwd = netWanted(reqUrl), nid = fwd ? 'f' + (++netSeq) : '', dec = fwd ? new TextDecoder() : null;
            function closed(){ try { window.__ktStreamOpen = Math.max(0, (window.__ktStreamOpen || 1) - 1); } catch(e){} if (fwd) { try { netSend(nid, reqUrl, dec.decode(), true); } catch(e){} } ended(); }
            (function pump(){
              mine.read().then(function(r){
                if (r.done) { closed(); return; }
                if (fwd) { try { netSend(nid, reqUrl, dec.decode(r.value, { stream: true }), false); } catch(e){} }
                pump();
              }, function(){ closed(); });
            })();
            return window.__ktAdoptBody(res, pair[1]);
          } catch(e) { return res; }
        });
      } catch(e) { return p; }
    };
  } catch(e){}
  // Same counters and end signal for the transports fetch does not cover (captured live, 15/09/2026):
  // DeepSeek streams its answer over XMLHttpRequest (text/event-stream), Gemini over XMLHttpRequest with
  // a JSON content type on StreamGenerate, Grok over a WebSocket opened with the page. Without these the
  // three providers could only conclude by DOM stability, seconds after the answer was complete.
  function opened(){
    try { window.__ktStreamOpen = (window.__ktStreamOpen || 0) + 1; } catch(e){}
    try { window.__ktStreamEver = (window.__ktStreamEver || 0) + 1; } catch(e){}
  }
  function closedOne(){ try { window.__ktStreamOpen = Math.max(0, (window.__ktStreamOpen || 1) - 1); } catch(e){} ended(); }
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
    XMLHttpRequest.prototype.open = function(m, u){ try { this.__ktUrl = String(u || ''); } catch(e){} return XO.apply(this, arguments); };
    XMLHttpRequest.prototype.send = function(){
      var xhr = this;
      try {
        noteCaptcha(xhr.__ktUrl);
        if (window.__ktSentAt) {
          var counted = false, xfwd = netWanted(xhr.__ktUrl), xid = xfwd ? 'x' + (++netSeq) : '', xseen = 0;
          xhr.addEventListener('readystatechange', function(){
            try {
              if (xfwd && xhr.readyState >= 3 && (xhr.responseType === '' || xhr.responseType === 'text')) {
                var all = xhr.responseText || '';
                if (all.length > xseen || xhr.readyState === 4) { var part = all.slice(xseen); xseen = all.length; netSend(xid, xhr.__ktUrl, part, xhr.readyState === 4); }
              }
              if (xhr.readyState === 2 && xhr.status === 401) { try { window.__ktAuthFail = Date.now(); } catch(e){} }
              if (xhr.readyState === 2 && xhr.status === 429) { try { window.__ktRateLimited = Date.now(); } catch(e){} }
              if (xhr.readyState === 2 && !counted) {
                var ct = xhr.getResponseHeader('content-type') || '';
                if (STREAMY.test(ct) || STREAM_URL.test(xhr.__ktUrl || '')) { counted = true; opened(); }
              }
              if (xhr.readyState === 4 && counted) { counted = false; closedOne(); }
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
          var wid = 'w' + (++netSeq);
          ws.addEventListener('message', function(ev){
            try {
              if (!window.__ktSentAt || typeof ev.data !== 'string') return;
              if (netWanted(u)) netSend(wid, u, ev.data, false);
              if (ev.data.indexOf('"type":"response.created"') >= 0) opened();
              if (ev.data.indexOf('"type":"response.done"') >= 0) closedOne();
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
  window.__ktFindAction = function(sel, path){
    try {
      // "row-last:<selector>": the LAST button of the row that holds that element -- for a control with no stable
      // attribute of its own sitting at the end of an action bar (ChatGPT's "more actions").
      if (sel && sel.indexOf('row-last:') === 0) {
        var anchor = document.querySelectorAll(sel.slice(9));
        if (!anchor.length) return null;
        var row = anchor[anchor.length - 1].closest('button, [role="button"]');
        row = row && row.parentElement;
        if (!row) return null;
        var bs = row.querySelectorAll('button, [role="button"]');
        return bs.length ? bs[bs.length - 1] : null;
      }
      if (sel) { var l = document.querySelectorAll(sel); return l.length ? l[l.length - 1].closest('button, [role="button"]') : null; }
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
  if (!__kt_tts_item) return;
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
  window.__ktBid = BID;               // a newer injection overwrites; older loops self-terminate
  var t0 = Date.now();
  function lastMatch(sel, outermost){
    if (!sel) return null;
    try {
      var els = document.querySelectorAll(sel);
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
      if (composerEl && el === composerEl) return true;
      return el.matches('form, [role="textbox"], [class*="query-bar" i]');
    } catch(e){ return false; }
  }
  function getAnswerEl(){
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
  var CHROME_SEL = 'button, [role="button"], [role="toolbar"], [role="menu"], [role="menuitem"],'
    + ' [role="tab"], [role="tablist"], select, input, textarea';
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
        try { if (c.matches(CHROME_SEL)) continue; } catch(e){}
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
    if (t && SENT && t === SENT) return '';   // that's our own message, not an answer
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
  function answerTxt(){
    var el = getAnswerEl();
    return sanitizeAnswer(el ? (el.innerText || '') : '');
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
    function hasBlock(n){
      try { return !!n.querySelector('p,div,pre,table,ul,ol,blockquote,h1,h2,h3,h4,h5,h6,li'); } catch(e){ return false; }
    }
    function inlineMd(node){
      var out = '';
      node.childNodes.forEach(function(n){
        if (n.nodeType === 3) { out += esc(visText(n)); return; }
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
    // Consecutive inline content (text, links, bold, <br>) is gathered into ONE paragraph and flushed when a
    // block starts. Pushing each piece as its own block split "see the [page](url) ." into three
    // paragraphs and turned every <br> of a poem into a blank line.
    function blockMd(node, depth){
      var out = [], buf = '';
      function flush(){
        var t = buf.replace(/[ \t]*\n[ \t]*/g, '\n').trim();
        if (t) out.push(t);
        buf = '';
      }
      node.childNodes.forEach(function(n){
        if (n.nodeType === 3) { buf += esc(visText(n)); return; }
        if (n.nodeType !== 1) return;
        // Same rule as the plain-text path: a control's label is not part of the answer.
        try { if (n.matches && n.matches(CHROME_SEL)) return; } catch(e){}
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
        if (!isBlock) { buf += inlineMd(n); return; }
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
    } catch(e){}
    return false;
  }
  /* ---- Blocks that need the user: a human check, a sign-in, a window over the composer. They are NOT an
     outcome: the card waits ("blocked"), the page is shown to the user, and the moment the block goes away
     (check solved, signed in, window closed) the send proceeds and the answer arrives as usual. Before
     this, each of them ended the card, and the user had to retry after fixing it. ---- */
  var blockPushed = '', blockSince = 0, BLOCK_MAX_MS = 20 * 60 * 1000;
  // Returns true when the block has lasted too long and the card was closed with it as the outcome.
  function syncBlock(reason){
    if (reason !== blockPushed) {
      blockPushed = reason;
      blockSince = reason ? Date.now() : 0;
      fdiagArm((reason ? 'BLOCKED ' + reason : 'UNBLOCKED'));
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
  var noComposerTicks = 0;
  var CAPTCHA_SEL = '[class*="captcha" i], [id*="captcha" i], [data-testid*="captcha" i], iframe[src*="captcha" i], iframe[src*="turnstile" i]';
  // Only while nothing has come back: once a response stream opened, the provider has the message.
  function currentBlock(clockTicks){
    if (window.__ktStreamEver) return '';
    // Hysteresis: a block, once announced, lasts while its own sign is still on the page. Entering and
    // leaving on different conditions made Kimi flap blocked/unblocked every couple of seconds.
    if (blockPushed === 'login' && (loginHintPresent() || authWallPresent())) return 'login';
    if (blockPushed === 'captcha' && (visibleMatch(CAPTCHA_SEL) || window.__ktCaptchaAt)) return 'captcha';
    if (blockPushed === 'overlay' && composerCovered()) return 'overlay';
    if (blockPushed === 'setup' && composerVal() === null) return 'setup';
    if (authWallPresent()) return authWallWhy === 'captcha' ? 'captcha' : 'login';
    if (clockTicks > 6 && (visibleMatch(CAPTCHA_SEL) || (window.__ktCaptchaAt && Date.now() - window.__ktCaptchaAt > 15000))) return 'captcha';
    // Signed out, recognised in seconds instead of minutes: a visible login control AND the page's own
    // API refusing the session. Either alone is not enough (logged-in pages carry "sign in with another
    // account" links; a 401 can come from an optional feature).
    if (window.__ktAuthFail && loginHintPresent()) return 'login';
    if (clockTicks > 10 && composerCovered()) return 'overlay';
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
    txt = txt || '';
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
    if (Date.now() - armT0 > 180000) { clearInterval(armIv); fdiagArm('EXIT arm-180s hint=' + loginHintPresent() + ' streamEver=' + (window.__ktStreamEver||0) + ' armTries=' + armTries); census(); setTimeout(function(){ deliver(loginHintPresent() ? 'login' : (KNOWN_SENT ? 'timeout' : 'sendfail'), ''); }, 300); return; }
    authCensus();
    // A visible captcha is not a missing login: the session is fine, the provider wants a human check
    // before it takes this message. Saying "sign in" sent the user to the wrong fix (captured on Z.ai).
    // Blocked: tell the UI, keep watching, and stop the clocks -- the time the user spends solving a check
    // or signing in must not run out the budgets below.
    var blk = currentBlock(armTries);
    if (syncBlock(blk)) { clearInterval(armIv); return; }
    if (blk) { armT0 = Date.now(); if (armTries > 40) armTries = 40; return; }
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
      var doneByEvent = streamEnded && !!txt && !busy && !stillPainting() && stable >= 1;
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
      // Same block handling as the arming loop, for blocks that appear after the message was handed over
      // (measured on Z.ai: the slider puzzle shows about 4s after Enter). Clock frozen while blocked.
      // A page without a composer that never took our message can show text that is not an answer (Copilot's
      // notice), so for that case the block check runs even with text on the page.
      var hblk = (txt && (window.__ktEnterPressed || window.__ktStreamEver || composerVal() !== null)) ? '' : currentBlock(polls);
      if (!hblk && polls > 45 && !txt && !window.__ktStreamEver && loginHintPresent()) hblk = 'login';
      if (syncBlock(hblk)) { clearInterval(iv); return; }
      if (hblk) { t0 = Date.now(); return; }
      // Signed out: say so early instead of waiting out the whole budget. Measured: the decision
      // is taken HERE, not in the arming loop (EXIT harvest-180s txtLen=0 hint=true), because the
      // page does carry some text that hands the arming loop over before the answer exists. Three
      // conditions must hold together and together they describe only that case: no answer text,
      // no response stream EVER opened (a provider that is generating always opens one), and a
      // login control visible on the page. It only chooses the MESSAGE of an outcome that is
      // already a failure, so it cannot cost a working answer.

      if (Date.now() - t0 > 180000) { clearInterval(iv); fdiagArm('EXIT harvest-180s txtLen=' + (txt||'').length + ' hint=' + loginHintPresent()); deliver(txt ? 'timeout' : (loginHintPresent() ? 'login' : 'error'), txt ? (sanitizeAnswer(cleanAnswerText(getAnswerEl())) || txt) : txt, txt ? elToMd(getAnswerEl()) : ''); return; }
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
        "var __kt_bid = {}; var __kt_key = {}; var __kt_ans = {}; var __kt_busy = {}; var __kt_fresh = {fresh}; var __kt_fast = {fast}; var __kt_sent = false; window.__ktDiag = {diag}; window.__ktStreamEver = 0; window.__ktTrustedInput = {trusted}; window.__ktNetUrl = {net_url};",
        serde_json::to_string(broadcast_id).map_err(|e| e.to_string())?,
        serde_json::to_string(key).map_err(|e| e.to_string())?,
        serde_json::to_string(ans).map_err(|e| e.to_string())?,
        serde_json::to_string(busy).map_err(|e| e.to_string())?,
        fast = fast_done_for(key),
        diag = crate::debug::enabled(),
        trusted = browser::needs_trusted_input(key),
        net_url = net_url_js(key),
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
) -> Result<String, String> {
    let (ans, busy) = selectors_for(key);
    // Whitespace-collapsed head of the message for a robust "is it on the page?" check.
    let head: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect();
    let prelude = format!(
        // `window.__ktDiag` must be set HERE too: without it, all the fill-loop diagnostics stayed
        // silent in exactly the path where they are needed -- the resume after a navigation (providers
        // whose temporary chat is a click DO navigate).
        "var __apb_text = {}; var __kt_head = {}; var __apb_send = true; var __kt_bid = {}; var __kt_key = {}; var __kt_ans = {}; var __kt_busy = {}; var __kt_fresh = true; var __kt_fast = {fast}; var __kt_sent = {sent}; window.__ktDiag = {diag}; window.__ktStreamEver = 0; window.__ktTrustedInput = {trusted}; window.__ktNetUrl = {net_url};",
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
    let fill = browser::fill_js(text, true)?;
    Ok(prelude + PUSH_HELPER_JS + SR_HIDE_JS + &attach_js(broadcast_id) + &fill + HARVEST_JS)
}

/// Marks (bid, key) answered: removes it from the broadcast, emits `app://kotodama-answer`
/// and, when the broadcast empties, `app://kotodama-finished`. Duplicate calls are no-ops.
fn finish_key(window: &Window, bid: &str, key: &str, status: &str, text: &str, truncated: bool, md: &str) {
    // An outcome ends any block on this provider for this broadcast, and its network readers.
    blocked_marks().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
    net_readers().lock().unwrap().remove(&(bid.to_string(), key.to_string()));
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
    // The provider page now ends with this answer: the one its read-aloud button would read.
    set_conv(window, key, (status == "done").then_some(bid));
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
    // The answer read from the network wins when it is complete: the provider's own Markdown, exact, with
    // reasoning and sources kept apart. The DOM reading stays the fallback for everything else.
    let mut from_net = false;
    if s == Some(0) && matches!(st.as_str(), "done" | "timeout" | "error" | "sendfail") {
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
        let dom_len = d.as_deref().map(str::len).unwrap_or(0);
        let net = net_answer(&b, &k).filter(|a| {
            let ok = st != "done" || dom_len == 0 || a.text.len() * 10 >= dom_len * 7;
            if !ok {
                debug::log(format!("kotodama NETREAD skipped key={k}: net={}B shorter than dom={dom_len}B", a.text.len()));
            }
            ok
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
        if let Some(r) = readers.get_mut(&id) {
            if !chunk.is_empty() {
                r.feed(chunk);
            }
            if end {
                r.end();
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
            if st == "blocked" {
                marks.insert((bid.clone(), key.clone()));
            } else {
                marks.remove(&(bid.clone(), key.clone()));
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
        return;
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
                            .insert(key.clone(), (inj.broadcast_id.clone(), inj.text.clone()));
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
        finish_key(&window, &broadcast_id, &key, "cancelled", "", false, "");
    }
    Ok(())
}

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
