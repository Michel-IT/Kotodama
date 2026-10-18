//! Reading answers from the provider page's own network traffic.
//!
//! The page makes its requests exactly as for a person; an in-page observer (NET_READ_JS in kotodama.rs)
//! copies the bytes it RECEIVES and forwards them here. Nothing is ever sent, altered or repeated.
//!
//! Why: the DOM is the presentation of the answer, the stream is the answer itself. Read from the stream,
//! the text arrives as the provider's own Markdown (tables, lists, links intact), reasoning is a separate
//! channel instead of something to guess from class names, and source links come as data.
//!
//! Structure: a few generic framers (SSE, JSON objects glued together, Gemini's length-prefixed frames),
//! a small JSON-Patch applier, and one reader per provider that knows where the text lives. A reader never
//! fails loudly: whatever it cannot parse is skipped and counted, and the DOM harvest stays the fallback
//! (the caller only trusts a reader that saw the stream end with non-empty text).

use serde_json::Value;

/// A link the provider cited for its answer.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Source {
    pub title: String,
    pub url: String,
    pub image: Option<String>,
}

/// What a reader extracted so far.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct NetAnswer {
    /// The answer in the provider's own Markdown.
    pub text: String,
    /// Reasoning / thinking, kept apart from the answer.
    pub reasoning: String,
    pub sources: Vec<Source>,
    /// The provider signalled the end of this answer (or its stream closed).
    pub done: bool,
    /// Frames that could not be parsed: a format change shows up here first.
    pub skipped: usize,
}

pub trait Reader: Send {
    /// Feeds raw text received by the page, in arrival order.
    fn feed(&mut self, chunk: &str);
    /// The transport closed (fetch body ended, XHR finished, socket closed).
    fn end(&mut self);
    fn answer(&self) -> &NetAnswer;
    /// True when `done` can only come from the provider's own end-of-answer marker, never from the transport
    /// simply closing: only then may the stream stand in for the page and close the answer by itself
    /// (`schedule_net_finish` in kotodama.rs). A connection that drops mid-answer must not look finished.
    fn done_is_explicit(&self) -> bool {
        false
    }
    /// For a provider whose answer stream stays open between answers: `Some(true)` while the provider says it is
    /// writing, `Some(false)` once it said it stopped, `None` before it has said anything about this send. The
    /// caller tells the page (it has no other way to see it, no response is open or closed) so that the DOM
    /// harvest neither gives up on a long silence nor ends on a pause of the text.
    fn working(&self) -> Option<bool> {
        None
    }
}

/// The answer endpoint of a provider, as a JavaScript regular expression source: the page observer forwards
/// only requests whose URL matches it. Kept next to `reader_for`, which recognises the same URLs.
pub fn url_pattern(key: &str) -> Option<&'static str> {
    Some(match key {
        // Claude: the old `/completion` SSE, and since October 2026 the conversation's timeline stream (see
        // `ClaudeTimeline`): the answer no longer travels in the reply to the send request.
        "anthropic" => "/completion(\\?|$)|ConversationService/StreamTimeline",
        // ChatGPT: /backend-api/f/conversation (and the plain /backend-api/conversation), never the history list.
        "openai" => "/backend-api/(f/)?conversation(\\?|$)",
        "deepseek" => "/chat/completion",
        "gemini" => "StreamGenerate",
        "mistral" => "/api/chat(\\?|$)",
        "perplexity" => "perplexity_ask",
        "zai" => "/chat/completions",
        "qwen" => "/chat/completions",
        "grok" => "/ws/mgw",
        _ => return None,
    })
}

/// The reader for a provider's answer request, chosen by provider key and request URL. `None` for every
/// other request the page makes (telemetry, history, settings).
pub fn reader_for(key: &str, url: &str) -> Option<Box<dyn Reader>> {
    let r: Box<dyn Reader> = match key {
        "anthropic" if url.contains("ConversationService/StreamTimeline") => Box::new(ClaudeTimeline::default()),
        "anthropic" if url.contains("/completion") => Box::new(Claude::default()),
        "openai" if url.contains("/conversation") => Box::new(OpenAi::default()),
        "deepseek" if url.contains("/chat/completion") => Box::new(DeepSeek::default()),
        "gemini" if url.contains("StreamGenerate") => Box::new(Gemini::default()),
        "mistral" if url.ends_with("/api/chat") || url.contains("/api/chat?") => Box::new(Mistral::default()),
        "perplexity" if url.contains("perplexity_ask") => Box::new(Perplexity::default()),
        "zai" if url.contains("/chat/completions") => Box::new(Zai::default()),
        "qwen" if url.contains("/chat/completions") => Box::new(Qwen::default()),
        "grok" if url.contains("/ws/mgw") => Box::new(Grok::default()),
        _ => return None,
    };
    Some(r)
}

/* ---------------------------------------------------------------------------------------------------- */
/* Framers                                                                                               */
/* ---------------------------------------------------------------------------------------------------- */

/// Server-Sent Events: yields the `data:` payload of each complete event (multi-line data joined with \n).
#[derive(Default)]
struct Sse {
    buf: String,
}
impl Sse {
    fn push(&mut self, chunk: &str) -> Vec<String> {
        // CRLF can be split across chunks ("\r" ending one, "\n" starting the next): normalise the whole
        // buffer and keep a trailing lone "\r" for the next chunk. Captured on Perplexity, where a per-chunk
        // replace glued two events together and both were lost.
        self.buf.push_str(chunk);
        let trailing_cr = self.buf.ends_with('\r');
        if trailing_cr {
            self.buf.pop();
        }
        self.buf = self.buf.replace("\r\n", "\n");
        if trailing_cr {
            self.buf.push('\r');
        }
        let mut out = Vec::new();
        while let Some(i) = self.buf.find("\n\n") {
            let event: String = self.buf.drain(..i + 2).collect();
            let data: Vec<&str> = event
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            if !data.is_empty() {
                out.push(data.join("\n"));
            }
        }
        out
    }
    /// Whatever is left when the stream closes (a last event without the blank line).
    fn flush(&mut self) -> Vec<String> {
        if self.buf.trim().is_empty() {
            return Vec::new();
        }
        self.buf.push_str("\n\n");
        self.push("")
    }
}

/// Top-level JSON objects glued together (`{..}{..}` or one per line), as on Grok's socket and Mistral's
/// `15:{..}` lines. Tracks strings so braces inside text do not count.
#[derive(Default)]
struct JsonObjects {
    buf: String,
}
impl JsonObjects {
    fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        let bytes = self.buf.as_bytes();
        let (mut depth, mut in_str, mut esc, mut start, mut consumed) = (0i32, false, false, None, 0usize);
        for (i, &b) in bytes.iter().enumerate() {
            if in_str {
                if esc {
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == b'"' {
                    in_str = false;
                }
                continue;
            }
            match b {
                b'"' => in_str = true,
                b'{' => {
                    if depth == 0 {
                        start = Some(i);
                    }
                    depth += 1;
                }
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        if let Some(s) = start.take() {
                            out.push(self.buf[s..=i].to_string());
                            consumed = i + 1;
                        }
                    }
                    if depth < 0 {
                        depth = 0;
                    }
                }
                _ => {}
            }
        }
        self.buf.drain(..consumed);
        out
    }
}

/* ---------------------------------------------------------------------------------------------------- */
/* JSON helpers                                                                                          */
/* ---------------------------------------------------------------------------------------------------- */

/// Walks a JSON-Pointer-like path ("/a/0/b" or "a/0/b"), creating missing objects/arrays on the way (a null
/// becomes an array when the next segment is an index, an object otherwise); "-1" and "-" address the last
/// element of an array.
fn pointer_mut<'a>(root: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut cur = root;
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        let seg = seg.replace("~1", "/").replace("~0", "~");
        if cur.is_null() {
            *cur = if seg.parse::<usize>().is_ok() || seg == "-1" { Value::Array(Vec::new()) } else { Value::Object(Default::default()) };
        }
        cur = match cur {
            Value::Object(map) => map.entry(seg).or_insert(Value::Null),
            Value::Array(arr) => {
                let idx = if seg == "-1" || seg == "-" { arr.len().checked_sub(1)? } else { seg.parse::<usize>().ok()? };
                while arr.len() <= idx {
                    arr.push(Value::Null);
                }
                &mut arr[idx]
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// Applies one patch operation. Supports the RFC 6902 ops providers use (add, replace, remove) plus the
/// "append" extension (string concatenation / array push) used by Mistral and DeepSeek.
fn apply_patch(root: &mut Value, op: &str, path: &str, value: &Value) -> bool {
    let op = op.to_ascii_lowercase();
    if path.is_empty() || path == "/" {
        if op == "replace" || op == "add" {
            *root = value.clone();
            return true;
        }
    }
    // For "add" on an array index, insert rather than overwrite.
    if op == "add" {
        if let Some((parent, last)) = path.rsplit_once('/') {
            if let Some(Value::Array(arr)) = pointer_mut(root, parent) {
                if last == "-" {
                    arr.push(value.clone());
                    return true;
                }
                if let Ok(i) = last.parse::<usize>() {
                    if i <= arr.len() {
                        arr.insert(i, value.clone());
                        return true;
                    }
                }
            }
        }
    }
    if op == "remove" {
        if let Some((parent, last)) = path.rsplit_once('/') {
            match pointer_mut(root, parent) {
                Some(Value::Array(arr)) => {
                    if let Ok(i) = last.parse::<usize>() {
                        if i < arr.len() {
                            arr.remove(i);
                        }
                    }
                }
                Some(Value::Object(map)) => {
                    map.remove(last);
                }
                _ => {}
            }
            return true;
        }
        return false;
    }
    let Some(target) = pointer_mut(root, path) else { return false };
    match op.as_str() {
        "append" => match (target, value) {
            (Value::String(s), Value::String(v)) => {
                s.push_str(v);
                true
            }
            (t @ Value::Null, v) => {
                *t = v.clone();
                true
            }
            (Value::Array(a), Value::Array(v)) => {
                a.extend(v.iter().cloned());
                true
            }
            (Value::Array(a), v) => {
                a.push(v.clone());
                true
            }
            _ => false,
        },
        _ => {
            *target = value.clone();
            true
        }
    }
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = match p.parse::<usize>() {
            Ok(i) => cur.get(i)?,
            Err(_) => cur.get(*p)?,
        };
    }
    cur.as_str()
}

/* ---------------------------------------------------------------------------------------------------- */
/* Readers                                                                                               */
/* ---------------------------------------------------------------------------------------------------- */

/// Claude: SSE in the Messages API shape. Text in `content_block_delta` / `text_delta`, reasoning in
/// `thinking_delta`, end at `message_stop`.
#[derive(Default)]
struct Claude {
    sse: Sse,
    ans: NetAnswer,
}
impl Claude {
    fn event(&mut self, data: &str) {
        let Ok(v) = serde_json::from_str::<Value>(data) else { self.ans.skipped += 1; return };
        match v.get("type").and_then(Value::as_str) {
            // Web search results arrive as a tool_result block listing the pages read (captured 15/09/2026:
            // `content[] {type: "knowledge", title, url}`).
            Some("content_block_start") => {
                if let Some(Value::Array(items)) = v.pointer("/content_block/content") {
                    for it in items {
                        if let Some(url) = it.get("url").and_then(Value::as_str) {
                            push_source(&mut self.ans.sources, it.get("title").and_then(Value::as_str).unwrap_or(""), url, None);
                        }
                    }
                }
            }
            Some("content_block_delta") => {
                let d = &v["delta"];
                match d.get("type").and_then(Value::as_str) {
                    Some("text_delta") => self.ans.text.push_str(d["text"].as_str().unwrap_or("")),
                    Some("thinking_delta") => self.ans.reasoning.push_str(d["thinking"].as_str().unwrap_or("")),
                    Some("citations_delta") => {
                        let c = &d["citation"];
                        if let Some(url) = c.get("url").and_then(Value::as_str) {
                            push_source(&mut self.ans.sources, c.get("title").and_then(Value::as_str).unwrap_or(""), url, None);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_stop") => self.ans.done = true,
            _ => {}
        }
    }
}
impl Reader for Claude {
    fn feed(&mut self, chunk: &str) {
        for d in self.sse.push(chunk) {
            self.event(&d);
        }
    }
    fn end(&mut self) {
        for d in self.sse.flush() {
            self.event(&d);
        }
    }
    fn answer(&self) -> &NetAnswer {
        &self.ans
    }
}

/// A protobuf field value: only the two wire types the timeline uses are kept, fixed-width ones are skipped.
enum Pb<'a> {
    Num(u64),
    Bytes(&'a [u8]),
}

/// Parses one protobuf message into its (field, value) pairs, in order. `None` when the bytes are not a message.
fn pb_parse(b: &[u8]) -> Option<Vec<(u32, Pb<'_>)>> {
    fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
        let (mut r, mut shift) = (0u64, 0u32);
        loop {
            let c = *b.get(*i)?;
            *i += 1;
            if shift >= 64 {
                return None;
            }
            r |= ((c & 127) as u64) << shift;
            if c & 128 == 0 {
                return Some(r);
            }
            shift += 7;
        }
    }
    let (mut out, mut i) = (Vec::new(), 0usize);
    while i < b.len() {
        let key = varint(b, &mut i)?;
        let field = (key >> 3) as u32;
        match key & 7 {
            0 => out.push((field, Pb::Num(varint(b, &mut i)?))),
            1 => {
                i = i.checked_add(8).filter(|e| *e <= b.len())?;
            }
            2 => {
                let len = usize::try_from(varint(b, &mut i)?).ok()?;
                let end = i.checked_add(len)?;
                out.push((field, Pb::Bytes(b.get(i..end)?)));
                i = end;
            }
            5 => {
                i = i.checked_add(4).filter(|e| *e <= b.len())?;
            }
            _ => return None,
        }
    }
    Some(out)
}
/// Every length-delimited value of field `n`.
fn pb_all<'a>(f: &[(u32, Pb<'a>)], n: u32) -> Vec<&'a [u8]> {
    f.iter()
        .filter_map(|(k, v)| match v {
            Pb::Bytes(b) if *k == n => Some(*b),
            _ => None,
        })
        .collect()
}
/// Every nested message of field `n`.
fn pb_msgs<'a>(f: &[(u32, Pb<'a>)], n: u32) -> Vec<Vec<(u32, Pb<'a>)>> {
    pb_all(f, n).into_iter().filter_map(pb_parse).collect()
}
fn pb_text(f: &[(u32, Pb<'_>)], n: u32) -> Option<String> {
    pb_all(f, n).first().map(|b| String::from_utf8_lossy(b).into_owned())
}
fn pb_num(f: &[(u32, Pb<'_>)], n: u32) -> Option<u64> {
    f.iter().find_map(|(k, v)| match v {
        Pb::Num(x) if *k == n => Some(*x),
        _ => None,
    })
}

/// A gzip member to its bytes (header per RFC 1952, raw deflate, 8-byte trailer ignored). Bounded: a frame of the
/// timeline is a few kilobytes.
fn gunzip(b: &[u8]) -> Option<Vec<u8>> {
    if b.len() < 18 || b[0] != 0x1f || b[1] != 0x8b || b[2] != 8 {
        return None;
    }
    let flags = b[3];
    let mut i = 10usize;
    if flags & 4 != 0 {
        let n = u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as usize;
        i += 2 + n;
    }
    for bit in [8u8, 16] {
        if flags & bit != 0 {
            while *b.get(i)? != 0 {
                i += 1;
            }
            i += 1;
        }
    }
    if flags & 2 != 0 {
        i += 2;
    }
    miniz_oxide::inflate::decompress_to_vec_with_limit(b.get(i..b.len().checked_sub(8)?)?, 8_000_000).ok()
}

/// One content block of the timeline: text (`kind` 2), reasoning (1), or something else (tool calls).
#[derive(Default)]
struct TlBlock {
    group: String,
    kind: u64,
    text: String,
    thinking: String,
    /// First seen as a state snapshot (the text so far), so every delta after it completes the text.
    from_snapshot: bool,
    /// The provider marked the block finished (a snapshot with field 4 = 1): its text is the whole text.
    finished: bool,
    /// What a widget tool block (a translation card, ...) shows, rebuilt from its structured payload (field 13).
    /// The page draws it inside an isolated iframe, so the page reading never sees it: this is the only copy.
    widget: String,
}

/// The words a widget block of the timeline displays, from its structured payload (field `13`). The translation
/// card (`mcp__widgets__translation_display_v0`, measured 16/10/2026) is `13.33` = { `1` source language name,
/// `2` target language name, `5` original, `6` translation, `8` title } and comes out as the same text the block's
/// own field `9` carries when it has one: title, "source -> target", original, translation. Any other widget gives
/// its plain-text leaves (several words, no markup) so a card of a kind we have not seen is not silently dropped;
/// markup and code (a generated interface) are left to the page, which is where those are readable.
fn widget_text(blk: &[(u32, Pb<'_>)]) -> String {
    fn leaves(f: &[(u32, Pb<'_>)], depth: u32, out: &mut Vec<String>) {
        for (_, v) in f {
            let Pb::Bytes(b) = v else { continue };
            // bytes that are readable text are text; anything else is tried as a nested message
            let text = std::str::from_utf8(b).ok().filter(|t| !t.chars().any(|c| c.is_control() && c != '\n' && c != '\t'));
            match (text, pb_parse(b).filter(|m| !m.is_empty() && depth < 6)) {
                (None, Some(m)) => leaves(&m, depth + 1, out),
                (Some(t), _) => {
                    let t = t.trim().to_string();
                    let plain = t.contains(char::is_whitespace) && !t.contains('<') && !t.contains('{');
                    if plain && !out.contains(&t) {
                        out.push(t);
                    }
                }
                _ => {}
            }
        }
    }
    // Only the widget tools: field 13 is the input of ANY tool block (a web search carries its query there), and
    // that is not an answer. Two families are measured: the `mcp__widgets__` tools (name in field 15, payload in
    // `13.33`) and the `visualize` server (`visualize:show_widget`, 18/10/2026: field 15 is only its display name,
    // "visualize: Show widget"; the tool is identified by `13.26.5` = "visualize").
    let widgets_family = pb_text(blk, 15).map_or(false, |n| n.starts_with("mcp__widgets__"));
    let mut parts: Vec<String> = Vec::new();
    for w in pb_msgs(blk, 13) {
        if let Some(t) = visualize_text(&w) {
            parts.push(t);
            continue;
        }
        if !widgets_family {
            continue;
        }
        let card = pb_msgs(&w, 33).into_iter().find(|c| pb_text(c, 5).is_some() && pb_text(c, 6).is_some());
        if let Some(c) = card {
            let t = |n: u32| pb_text(&c, n).filter(|x| !x.trim().is_empty());
            if let Some(title) = t(8) {
                parts.push(title);
            }
            if let (Some(a), Some(b)) = (t(1), t(2)) {
                parts.push(format!("{a} → {b}"));
            }
            parts.extend(t(5));
            parts.extend(t(6));
        } else {
            leaves(&w, 0, &mut parts);
        }
    }
    parts.join("\n\n")
}

/// The words of a `visualize:show_widget` call (measured 18/10/2026): `13.26` = { `1` tool id, `5` server
/// "visualize", `2` the input as JSON once the call is complete, `13` the same JSON while it is still being
/// written }. The input's `widget_code` is the HTML the page draws in an isolated iframe (a card with several
/// sentences, say), so its visible text is the only readable copy. Only the complete input (`2`) is read: the
/// streaming one is cut mid-markup. None when this is not that tool, or the input is not complete yet.
fn visualize_text(w: &[(u32, Pb<'_>)]) -> Option<String> {
    let call = pb_msgs(w, 26).into_iter().find(|c| pb_text(c, 5).as_deref() == Some("visualize"))?;
    let input: Value = serde_json::from_str(&pb_text(&call, 2)?).ok()?;
    let text = html_text(input.get("widget_code")?.as_str()?);
    (!text.is_empty()).then_some(text)
}

/// The visible text of a small HTML fragment: tags dropped, one line per block element, the contents of elements the
/// reader does not see (`style`, `script`, `svg`, `template`, `noscript`, anything marked `sr-only`, comments) left
/// out, entities decoded. A tag ends at the first `>` OUTSIDE its quoted attributes (`onclick="x>0"` is one tag), a
/// hidden element is skipped to its own closing tag even when others of the same name are nested inside, and a `<`
/// that does not start a tag stays as text.
fn html_text(html: &str) -> String {
    const BLOCK: [&str; 14] = ["p", "div", "br", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6", "td", "th", "section"];
    const VOID: [&str; 8] = ["br", "hr", "img", "input", "meta", "link", "col", "wbr"];
    const HIDDEN: [&str; 5] = ["style", "script", "svg", "template", "noscript"];
    let (mut out, mut i) = (String::new(), 0usize);
    // the hidden element we are inside: its name and how many same-name elements are open within it
    let mut skip: Option<(String, usize)> = None;
    while i < html.len() {
        let rest = &html[i..];
        if !rest.starts_with('<') {
            let n = rest.find('<').unwrap_or(rest.len());
            if skip.is_none() {
                out.push_str(&rest[..n]);
            }
            i += n;
            continue;
        }
        if rest.starts_with("<!--") {
            // a comment ends at "-->" whatever it contains
            i += rest.find("-->").map_or(rest.len(), |e| e + 3);
            continue;
        }
        let next = rest[1..].chars().next();
        if !matches!(next, Some(c) if c.is_ascii_alphabetic() || c == '/' || c == '!') {
            if skip.is_none() {
                out.push('<');
            }
            i += 1;
            continue;
        }
        // end of the tag: the first '>' that is not inside a quoted attribute value (a quote opens only right after
        // '=', so a stray apostrophe in a bare word does not keep the tag open)
        let mut quote: Option<char> = None;
        let mut end = None;
        let mut prev = ' ';
        for (at, c) in rest.char_indices().skip(1) {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (None, '"') | (None, '\'') if prev == '=' => quote = Some(c),
                (None, '>') => {
                    end = Some(at);
                    break;
                }
                _ => {}
            }
            if !c.is_whitespace() || quote.is_some() {
                prev = c;
            }
        }
        let Some(end) = end else { break };
        let tag = &rest[1..end];
        i += end + 1;
        let closing = tag.starts_with('/');
        // the whole tag name, hyphen included: `<script-card>` is a custom element, not `<script>`
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')).collect::<String>().to_lowercase();
        // `script` and `style` hold raw text: nothing inside is a tag, the slash of `<script/>` means nothing, and the
        // element ends at the first `</script` / `</style` whatever the code in between looks like
        if !closing && (name == "script" || name == "style" || name == "noscript") {
            // (ASCII lowercasing keeps every byte offset). Like a browser, the element ends at `</name` followed by
            // a space, '/' or '>': `</scripts` inside the code is still code.
            let lower = html[i..].to_ascii_lowercase();
            let needle = format!("</{name}");
            let (mut from, mut at) = (0, lower.len());
            while let Some(p) = lower[from..].find(&needle) {
                let after = lower[from + p + needle.len()..].chars().next();
                if after.map_or(true, |c| c.is_whitespace() || c == '/' || c == '>') {
                    at = from + p;
                    break;
                }
                from += p + needle.len();
            }
            i += at;
            continue;
        }
        let self_closed = tag.ends_with('/') || VOID.contains(&name.as_str());
        if let Some((until, depth)) = &mut skip {
            if name == *until && !self_closed {
                if closing {
                    if *depth == 0 {
                        skip = None;
                    } else {
                        *depth -= 1;
                    }
                } else {
                    *depth += 1;
                }
            }
            continue;
        }
        let screen_reader_only = tag.split(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '=').any(|w| w == "sr-only");
        if !closing && !self_closed && (HIDDEN.contains(&name.as_str()) || screen_reader_only) {
            skip = Some((name, 0));
            continue;
        }
        if BLOCK.contains(&name.as_str()) {
            out.push('\n');
        }
    }
    let out = decode_entities(&out);
    out.lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

/// HTML entities in one pass (so `&amp;lt;` becomes `&lt;` and no further): the numeric ones and the few named ones
/// a generated card uses. An unknown name stays as written.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest[1..].find(';').filter(|&e| e <= 10).and_then(|e| {
            let name = &rest[1..1 + e];
            let ch = match name {
                "nbsp" => Some(' '),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "amp" => Some('&'),
                "rarr" => Some('→'),
                "larr" => Some('←'),
                "mdash" | "ndash" => Some('-'),
                "hellip" => Some('…'),
                "eacute" => Some('é'),
                "egrave" => Some('è'),
                "agrave" => Some('à'),
                "ograve" => Some('ò'),
                "ugrave" => Some('ù'),
                "igrave" => Some('ì'),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .filter(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| {
                        name.strip_prefix('#')
                            .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
                            .and_then(|d| d.parse::<u32>().ok())
                    })
                    .filter(|&n| n != 0)
                    .and_then(char::from_u32),
            };
            ch.map(|c| (c, e + 2))
        });
        match decoded {
            Some((c, used)) => {
                out.push(if c == '\u{a0}' { ' ' } else { c });
                rest = &rest[used..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Claude since October 2026. The reply to the send request (`PerformAction`) is empty and the answer is pushed
/// on the conversation's timeline stream, `ConversationService/StreamTimeline` (Connect protocol over HTTP/2:
/// a sequence of frames, each a flags byte -- 1 = gzip, 2 = trailer -- a big-endian length and a protobuf message;
/// an empty heartbeat every 20 s and a trailer "cadence" that closes the stream now and then, the page then
/// reopens it). The page observer forwards the received bytes as base64, one line per chunk.
///
/// What a message holds (field numbers measured on 14/10/2026 over text, reasoning, web search and a second turn):
/// `1` = event, with `1.1` = state changes and `1.2` = a delta. State: `2` conversation (`3` = 2 while it writes, 1
/// when idle), `3` message (`1` id, `3` role: 1 user, 2 assistant), `4` group of blocks (`1` id, `2` message), `5`
/// block (`1` id, `2` group, `9` text so far, `10.1` kind: 1 reasoning / 2 text, `12.1` reasoning so far, `11`
/// citations). Delta: `1` block, `2` text to append, `6.1` reasoning to append, `4` a citation.
///
/// The end is the conversation going back to idle after it was seen writing (`done_is_explicit`): the stream
/// itself closes for other reasons too.
#[derive(Default)]
struct ClaudeTimeline {
    /// Base64 lines not yet complete.
    lines: String,
    /// Decoded bytes not yet forming a whole frame.
    buf: Vec<u8>,
    roles: std::collections::HashMap<String, u64>,
    /// Group id -> message id.
    groups: std::collections::HashMap<String, String>,
    blocks: std::collections::HashMap<String, TlBlock>,
    /// Block ids in the order they first appeared.
    order: Vec<String>,
    busy_seen: bool,
    /// A delta of text or reasoning arrived on this stream: it is following an answer being written, even when it
    /// was opened after the conversation had gone busy (the page reconnects it now and then, 14/10/2026: a stream
    /// that opened 20 s into a 25 s essay began with a delta and never saw the "busy" state).
    live_seen: bool,
    idle: bool,
    /// The transport closed: what this stream said last is no longer true.
    closed: bool,
    ans: NetAnswer,
}
impl ClaudeTimeline {
    fn block_mut(&mut self, id: &str) -> &mut TlBlock {
        if !self.blocks.contains_key(id) {
            self.order.push(id.to_string());
        }
        self.blocks.entry(id.to_string()).or_default()
    }
    /// A snapshot is the text so far; one that is a stale prefix of what the deltas already built is ignored.
    fn snapshot(cur: &mut String, snap: String) {
        if !(cur.len() > snap.len() && cur.starts_with(&snap)) {
            *cur = snap;
        }
    }
    fn cite(&mut self, c: &[(u32, Pb<'_>)]) {
        let image = pb_msgs(c, 6).first().and_then(|x| pb_text(x, 4));
        if let Some(url) = pb_text(c, 3) {
            push_source(&mut self.ans.sources, &pb_text(c, 2).unwrap_or_default(), &url, image);
        }
    }
    fn state(&mut self, st: &[(u32, Pb<'_>)]) {
        for c in pb_msgs(st, 2) {
            match pb_num(&c, 3) {
                Some(2) => {
                    self.busy_seen = true;
                    self.idle = false;
                }
                Some(1) => self.idle = true,
                _ => {}
            }
        }
        for m in pb_msgs(st, 3) {
            if let (Some(id), Some(role)) = (pb_text(&m, 1), pb_num(&m, 3)) {
                self.roles.insert(id, role);
            }
        }
        for g in pb_msgs(st, 4) {
            if let (Some(id), Some(msg)) = (pb_text(&g, 1), pb_text(&g, 2)) {
                self.groups.insert(id, msg);
            }
        }
        for b in pb_msgs(st, 5) {
            let Some(id) = pb_text(&b, 1) else { continue };
            let is_new = !self.blocks.contains_key(&id);
            let group = pb_text(&b, 2);
            let kind = pb_msgs(&b, 10).first().and_then(|t| pb_num(t, 1));
            let text = pb_text(&b, 9);
            let thinking = pb_msgs(&b, 12).first().and_then(|t| pb_text(t, 1));
            let widget = widget_text(&b);
            let fin = pb_num(&b, 4) == Some(1);
            let blk = self.block_mut(&id);
            if is_new {
                blk.from_snapshot = true;
            }
            blk.finished |= fin;
            if let Some(g) = group {
                blk.group = g;
            }
            if let Some(k) = kind {
                blk.kind = k;
            }
            if let Some(t) = text {
                Self::snapshot(&mut blk.text, t);
            }
            if let Some(t) = thinking {
                Self::snapshot(&mut blk.thinking, t);
            }
            if !widget.is_empty() {
                blk.widget = widget;
            }
            for c in pb_msgs(&b, 11) {
                self.cite(&c);
            }
        }
    }
    fn delta(&mut self, d: &[(u32, Pb<'_>)]) {
        let Some(id) = pb_text(d, 1) else { return };
        let text = pb_text(d, 2);
        let thinking = pb_msgs(d, 6).first().and_then(|t| pb_text(t, 1));
        self.live_seen |= text.is_some() || thinking.is_some();
        let blk = self.block_mut(&id);
        if let Some(t) = text {
            if blk.kind == 0 {
                blk.kind = 2;
            }
            blk.text.push_str(&t);
        }
        if let Some(t) = thinking {
            if blk.kind == 0 {
                blk.kind = 1;
            }
            blk.thinking.push_str(&t);
        }
        for c in pb_msgs(d, 4) {
            self.cite(&c);
        }
    }
    fn frame(&mut self, flags: u8, body: &[u8]) {
        if flags & 2 != 0 {
            return; // trailer (JSON): "cadence" or an error status, nothing of the answer
        }
        let plain;
        let body = if flags & 1 != 0 {
            match gunzip(body) {
                Some(v) => {
                    plain = v;
                    &plain[..]
                }
                None => {
                    self.ans.skipped += 1;
                    return;
                }
            }
        } else {
            body
        };
        let Some(top) = pb_parse(body) else {
            self.ans.skipped += 1;
            return;
        };
        for ev in pb_msgs(&top, 1) {
            for st in pb_msgs(&ev, 1) {
                self.state(&st);
            }
            for d in pb_msgs(&ev, 2) {
                self.delta(&d);
            }
        }
    }
    /// The answer is the text blocks of the assistant's message in the order they opened (a user's own block is
    /// left out by its message's role), the reasoning the reasoning blocks.
    fn refresh(&mut self) {
        let (mut text, mut reasoning, mut whole) = (String::new(), String::new(), true);
        for id in &self.order {
            let b = &self.blocks[id];
            if self.groups.get(&b.group).and_then(|m| self.roles.get(m)) == Some(&1) {
                continue;
            }
            // A widget block that carries no text of its own (no field 9) still shows words on the page, inside an
            // iframe nobody can read but this stream: its payload is part of the answer, in its place in the order.
            let widget_only = b.kind != 1 && b.text.is_empty() && !b.widget.is_empty();
            let (into, piece) = if widget_only {
                (&mut text, &b.widget)
            } else {
                match b.kind {
                    2 => (&mut text, &b.text),
                    1 => (&mut reasoning, &b.thinking),
                    _ => continue,
                }
            };
            if !piece.is_empty() {
                // text we only know from deltas lacks whatever came before the first one, until the block's final snapshot
                whole &= b.kind != 2 || b.from_snapshot || b.finished;
                if !into.is_empty() {
                    into.push_str("\n\n");
                }
                into.push_str(piece);
            }
        }
        self.ans.done = (self.busy_seen || self.live_seen) && self.idle && whole && !text.trim().is_empty();
        self.ans.text = text;
        self.ans.reasoning = reasoning;
    }
    fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
        while self.buf.len() >= 5 {
            let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
            if len > 64_000_000 {
                self.buf.clear(); // not a Connect stream any more: drop it rather than wait for 4 GB
                self.ans.skipped += 1;
                break;
            }
            if self.buf.len() < 5 + len {
                break;
            }
            let flags = self.buf[0];
            let body: Vec<u8> = self.buf[5..5 + len].to_vec();
            self.buf.drain(..5 + len);
            self.frame(flags, &body);
        }
        self.refresh();
    }
}
impl Reader for ClaudeTimeline {
    fn feed(&mut self, chunk: &str) {
        self.lines.push_str(chunk);
        while let Some(i) = self.lines.find('\n') {
            let line: String = self.lines.drain(..=i).collect();
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match crate::audio::base64_decode(line) {
                Some(b) => self.bytes(&b),
                None => self.ans.skipped += 1,
            }
        }
    }
    fn end(&mut self) {
        self.closed = true;
    }
    fn answer(&self) -> &NetAnswer {
        &self.ans
    }
    fn done_is_explicit(&self) -> bool {
        true
    }
    fn working(&self) -> Option<bool> {
        // closed while still writing: that says nothing any more (the answer goes on in another stream); closed after
        // the idle state: it did stop -- and the last chunk and the close often arrive in the same batch
        if self.closed && !self.idle {
            return None;
        }
        (self.busy_seen || self.live_seen).then_some(!self.idle)
    }
}

/// ChatGPT: SSE in OpenAI's "delta encoding v1". A first frame adds the whole message, then frames carry
/// operations on paths (`{p, o, v}`), and a frame with only `v` continues the previous path and operation --
/// the same idea DeepSeek uses. We do not rebuild the document: the only thing worth following is the assistant
/// message being written, so the reader captures when an assistant text message opens and appends what lands in
/// its part. Reasoning ("thoughts") is kept apart, everything else (system messages, the echo of the user's own
/// question, metadata) is ignored, which is what stops the question itself from ending up in the answer.
#[derive(Default, PartialEq, Clone, Copy)]
enum Cap {
    #[default]
    None,
    Text,
    Reasoning,
}
#[derive(Default)]
struct OpenAi {
    sse: Sse,
    last_path: String,
    last_op: String,
    /// What each message (by `c`) is.
    caps: std::collections::BTreeMap<i64, Cap>,
    ans: NetAnswer,
    /// What `answer()` hands out: `ans` with ChatGPT's block directives removed (see `strip_block_directives`).
    view: NetAnswer,
    /// The message the next operation applies to: the `c` of the last frame that carried one (it holds until
    /// another says otherwise, like `p` and `o`), or the one that just opened.
    cur: i64,
    /// `c` for an `add` frame that has none (older streams): messages are numbered in the order they open.
    next_idx: i64,
    /// The assistant text messages, by `c`. See `answer_text`.
    texts: std::collections::BTreeMap<i64, String>,
}
impl OpenAi {
    fn refresh_view(&mut self) {
        self.ans.text = self.answer_text();
        self.view = self.ans.clone();
        self.view.text = strip_component_tags(&strip_block_directives(&self.ans.text), self.ans.done);
    }
    /// The answer is the assistant text messages in the order they opened, each once. Since October 2026 an
    /// answer with a web search is mirrored: the page streams into a child message (`c` 2, then `c` 9 after the
    /// search) and every operation is repeated on a second, "logical" message that ends up holding the whole
    /// text (measured 13/10/2026: both texts of one turn twice over, "Ho effettuHo effettu"). Summing
    /// every message doubled the answer, so a message whose whole text sits inside another one is a copy of part
    /// of it and is left out (the later one wins a tie). Streams without mirrors have nothing contained, and
    /// are summed as before.
    fn answer_text(&self) -> String {
        let all: Vec<(&i64, &String)> = self.texts.iter().filter(|(_, t)| !t.is_empty()).collect();
        let mut out = String::new();
        for (k, (c, t)) in all.iter().enumerate() {
            let copy = all.iter().enumerate().any(|(j, (c2, t2))| {
                j != k && t2.len() >= t.len() && (t2.len() > t.len() || **c2 > **c) && t2.contains(t.as_str())
            });
            if !copy {
                out.push_str(t);
            }
        }
        out
    }
    fn event(&mut self, data: &str) {
        let data = data.trim();
        if data == "[DONE]" {
            self.ans.done = true;
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { self.ans.skipped += 1; return };
        // Frames that are not deltas: the end of the stream is the one that matters.
        if let Some(t) = v.get("type").and_then(Value::as_str) {
            if t == "message_stream_complete" {
                self.ans.done = true;
            }
            return;
        }
        if let Some(c) = v.get("c").and_then(Value::as_i64) {
            self.cur = c;
        }
        self.op(&v);
    }
    fn op(&mut self, v: &Value) {
        if let Some(p) = v.get("p").and_then(Value::as_str) {
            self.last_path = p.to_string();
            if v.get("o").is_none() {
                self.last_op = "append".into();
            }
        }
        if let Some(o) = v.get("o").and_then(Value::as_str) {
            self.last_op = o.to_string();
        }
        let val = v.get("v");
        if self.last_op == "patch" {
            if let Some(Value::Array(items)) = val {
                // The operations inside a patch must not become the "previous operation" that a bare `{"v":..}`
                // frame continues: that is the patch itself (measured 13/10/2026: after a status update inside a
                // patch, the next bare list of appends was read as a `replace` and its text was lost).
                let (path, op) = (self.last_path.clone(), self.last_op.clone());
                for it in items.clone() {
                    self.op(&it);
                }
                self.last_path = path;
                self.last_op = op;
            }
            return;
        }
        let Some(val) = val else { return };
        // A message object opens a message whatever the previous operation was (a group of messages opens with
        // one explicit `add` and the others follow as bare `{"v":{"message":..}}`).
        let opens = val.pointer("/message/author").is_some();
        if opens || (self.last_op == "add" && self.last_path.is_empty()) {
            // A new message opens: decide whether what follows belongs to the answer.
            let m = &val["message"];
            let role = m.pointer("/author/role").and_then(Value::as_str).unwrap_or("");
            let ctype = m.pointer("/content/content_type").and_then(Value::as_str).unwrap_or("");
            // A message addressed to a tool (`web.run`) is the call, not something the user reads.
            let to_user = m.get("recipient").and_then(Value::as_str).map_or(true, |r| r == "all");
            self.cur = v.get("c").and_then(Value::as_i64).unwrap_or(self.next_idx);
            self.next_idx = self.cur + 1;
            let cap = match (role, ctype) {
                ("assistant", "text") if to_user => Cap::Text,
                ("assistant", "thoughts") => Cap::Reasoning,
                _ => Cap::None,
            };
            self.caps.insert(self.cur, cap);
            if cap == Cap::Text {
                if let Some(part) = m.pointer("/content/parts/0").and_then(Value::as_str) {
                    if !part.is_empty() {
                        self.texts.entry(self.cur).or_default().push_str(part);
                    }
                }
            }
            return;
        }
        if self.last_op == "append" {
            let Some(text) = val.as_str() else { return };
            if self.last_path.contains("/content/parts/") {
                match self.caps.get(&self.cur).copied().unwrap_or(Cap::None) {
                    Cap::Text => self.texts.entry(self.cur).or_default().push_str(text),
                    Cap::Reasoning => self.ans.reasoning.push_str(text),
                    Cap::None => {}
                }
            } else if self.last_path.contains("/thoughts") {
                self.ans.reasoning.push_str(text);
            }
        }
    }
}
impl Reader for OpenAi {
    fn feed(&mut self, chunk: &str) {
        for d in self.sse.push(chunk) {
            self.event(&d);
        }
        self.refresh_view();
    }
    fn end(&mut self) {
        for d in self.sse.flush() {
            self.event(&d);
        }
        self.refresh_view();
    }
    fn answer(&self) -> &NetAnswer {
        &self.view
    }
    fn done_is_explicit(&self) -> bool {
        true // `[DONE]` and `message_stream_complete` only: `end()` never marks the answer done
    }
}

/// ChatGPT wraps "writing" answers (poems, letters, documents) in block directives that its own page turns
/// into a canvas and never shows as text: `:::writing{variant="document" id="1" title="X"}` on the opening
/// line and `:::` on the closing one. Removes every line that is only a directive, keeping the content.
/// The last line may still be arriving, so an unfinished opening (`:::wri`, `:::writing{variant=`) goes too.
fn strip_block_directives(text: &str) -> String {
    fn is_directive(line: &str, last: bool) -> bool {
        let t = line.trim();
        let rest = t.trim_start_matches(':');
        if t.len() - rest.len() < 2 {
            return false;
        }
        let name_end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).unwrap_or(rest.len());
        let tail = &rest[name_end..];
        tail.is_empty() || (tail.starts_with('{') && (tail.ends_with('}') || (last && !tail.contains('}'))))
    }
    if !text.contains("::") {
        return text.to_string();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let n = lines.len();
    let kept: Vec<&str> = lines.iter().enumerate().filter(|(i, l)| !is_directive(l, *i + 1 == n)).map(|(_, l)| *l).collect();
    kept.join("\n").trim_matches('\n').to_string()
}

/// ChatGPT writes its rich answers as components that its page draws and never shows as markup (seen October
/// 2026): `<Entity category="song" value="Dark Horse" .../>` (a linked name, shown as its `value`),
/// `<AsyncImageGroup query={[...]}/>` and `<AsyncImage .../>` (pictures), `<Cite .../>` (a source chip),
/// `<WritingBlock ...>...</WritingBlock>` (the canvas around a text), `<Link url title/>`, `<icon name="check-circle"
/// .../>` (a symbol in a table cell), and layout boxes around cards and comparisons: `<row align=...>`, `<box ...>`,
/// `<grid>`, `<title size=...>`, `<text>`, `<caption>`, plus whole `<svg viewBox=...>` drawings. Those go and their
/// text stays. The same characters are also ordinary prose (`A<B`, `i<N`, `Result<T, E>`, "il tag <title>"), and a
/// cut or mangled answer is worse than a leftover tag, so the rule is strict: only a tag with real attributes
/// (`name="..."` or `name={...}`) is a component, and then only a known name, a layout name or a self-closed
/// capitalized one; `<svg>` also needs its `</svg>`; a closing tag goes only if its opening went. Nothing changes inside code (fenced or between backticks). While the answer is still arriving (`done`
/// false) a component cut at the end is held back; on the finished answer an unclosed `<` is just text.
fn strip_component_tags(text: &str, done: bool) -> String {
    const KNOWN: [&str; 7] = ["Entity", "AsyncImageGroup", "AsyncImage", "Cite", "Link", "WritingBlock", "CodeBlock"];
    const LAYOUT: [&str; 16] = [
        "row", "box", "grid", "grid-item", "title", "text", "caption", "icon", "stack", "col", "column", "card",
        "badge", "divider", "spacer", "svg",
    ];
    if !text.contains('<') {
        return text.to_string();
    }
    let fenced_text = codeblocks_to_fences(text);
    let text = fenced_text.as_str();
    enum End {
        At(usize),
        Unfinished,
        NotATag,
    }
    // End of the tag starting at `s[0] == '<'`: its first '>' outside double quotes and braces (a query can hold
    // `>` in a string). An apostrophe is ordinary text in Italian, so it never opens a quote. Another '<' or an
    // empty line before the end means this was not a tag.
    fn tag_end(s: &str) -> End {
        let (mut quoted, mut depth) = (false, 0i32);
        for (i, c) in s.char_indices().skip(1) {
            if quoted {
                quoted = c != '"';
                continue;
            }
            match c {
                '"' => quoted = true,
                '{' => depth += 1,
                '}' => depth -= 1,
                '>' if depth <= 0 => return End::At(i + 1),
                '<' if depth <= 0 => return End::NotATag,
                '\n' if depth <= 0 && s[i + 1..].starts_with('\n') => return End::NotATag,
                _ => {}
            }
        }
        End::Unfinished
    }
    fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
        let key = format!(" {name}=\"");
        let at = tag.find(&key)? + key.len();
        let len = tag[at..].find('"')?;
        Some(&tag[at..at + len])
    }
    fn has_attrs(tag: &str) -> bool {
        tag.contains("=\"") || tag.contains("={")
    }
    // A tag that stood alone on its line leaves no empty line behind: its indentation and line break go too.
    fn skip_emptied_line(out: &mut String, text: &str, mut i: usize) -> usize {
        let at_line_start = out.is_empty() || out.trim_end_matches([' ', '\t']).ends_with('\n');
        let blank = text[i..].len() - text[i..].trim_start_matches([' ', '\t']).len();
        if at_line_start && (text[i + blank..].starts_with('\n') || i + blank == text.len()) {
            while out.ends_with([' ', '\t']) {
                out.pop();
            }
            i += blank + usize::from(text[i + blank..].starts_with('\n'));
        }
        i
    }
    let mut out = String::with_capacity(text.len());
    // components taken out and not closed yet, each with what its closing tag leaves behind
    let mut open: Vec<(String, String)> = Vec::new();
    let (mut fenced, mut inline_code, mut line_start) = (false, false, true);
    let mut i = 0;
    'scan: while i < text.len() {
        let rest = &text[i..];
        if line_start && rest.trim_start_matches([' ', '\t']).starts_with("```") {
            fenced = !fenced;
        }
        let c = rest.chars().next().unwrap();
        line_start = c == '\n';
        if c == '`' && !fenced {
            inline_code = !inline_code;
        }
        if c == '\n' {
            inline_code = false; // an inline span never crosses a line
        }
        if c == '<' && !fenced && !inline_code {
            let after = &rest[1..];
            let closing = after.starts_with('/');
            let name_src = if closing { &after[1..] } else { after };
            let name_len = name_src.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).unwrap_or(name_src.len());
            let name = &name_src[..name_len];
            let known = KNOWN.contains(&name);
            let layout = LAYOUT.contains(&name);
            if !name.is_empty() && name_len == name_src.len() {
                // the text ends inside a tag name: a component still arriving
                let maybe = KNOWN.iter().chain(LAYOUT.iter()).any(|n| n.starts_with(name));
                if !done && maybe {
                    break 'scan;
                }
            } else if !name.is_empty() {
                match tag_end(rest) {
                    End::Unfinished => {
                        // held back only near the end of what has arrived: further back it is prose that never closed
                        let near_end = text.len() - i < 400;
                        if !done && near_end && (known || layout || name.starts_with(|c: char| c.is_ascii_uppercase())) {
                            break 'scan; // a component still arriving: held back until it is complete
                        }
                    }
                    End::NotATag => {}
                    End::At(len) => {
                        let tag = &rest[..len];
                        if closing {
                            if let Some(at) = open.iter().rposition(|(n, _)| n == name) {
                                out.push_str(&open[at].1);
                                open.truncate(at);
                                i = skip_emptied_line(&mut out, text, i + len);
                                continue;
                            }
                        } else {
                            let self_closed = tag.ends_with("/>");
                            let closes_later = !self_closed && text[i + len..].contains(&format!("</{name}>"));
                            let capital = name.starts_with(|c: char| c.is_ascii_uppercase());
                            // `<caption>` arrives bare, on a line of its own, under a table (measured 14/10/2026); a
                            // bare tag quoted in prose is never alone on its line.
                            let bare_caption = name == "caption"
                                && !has_attrs(tag)
                                && (out.is_empty() || out.trim_end_matches([' ', '\t']).ends_with('\n'))
                                && (closes_later || !done);
                            let component = if bare_caption {
                                true
                            } else if name == "svg" {
                                has_attrs(tag) && (self_closed || closes_later)
                            } else {
                                // a name quoted in prose ("il componente <Link>") has no attributes: it stays
                                (known || layout || (capital && self_closed)) && has_attrs(tag)
                            };
                            // held back only while it can still be a drawing arriving: near the end, like any tag
                            if name == "svg" && !component && !done && has_attrs(tag) && text.len() - i < 400 {
                                break 'scan; // a drawing whose end has not arrived yet
                            }
                            if component {
                                if name == "svg" && !self_closed {
                                    // a drawing: nothing of it is text
                                    let end = text[i + len..].find("</svg>").map(|e| i + len + e + "</svg>".len());
                                    i = skip_emptied_line(&mut out, text, end.unwrap_or(text.len()));
                                    continue;
                                }
                                match name {
                                    "Entity" => out.push_str(attr(tag, "value").unwrap_or("")),
                                    "icon" => {
                                        // A symbol in a table cell carries the answer ("Compatibile": check or cross),
                                        // so an empty cell would lose it. `check-circle` was seen (13/10/2026).
                                        let icon = attr(tag, "name").unwrap_or("");
                                        if icon.starts_with("check") {
                                            out.push('\u{2713}');
                                        } else if icon.starts_with('x') || icon.starts_with("close") || icon.starts_with("cross") {
                                            out.push('\u{2717}');
                                        }
                                    }
                                    "Link" => match (attr(tag, "url"), attr(tag, "title")) {
                                        (Some(u), Some(t)) => out.push_str(&format!("[{t}]({u})")),
                                        // `<Link url="...">Idealo</Link>` (measured 14/10/2026, a table of shops): the
                                        // name is the body, so it becomes the text of the link; printing the address
                                        // alone left "https://...htmlIdealo" glued together.
                                        // on a finished answer only with its `</Link>` further on, or a lone "[" would stay
                                        (Some(u), None) if !self_closed && (!done || text[i + len..].contains("</Link>")) => {
                                            out.push('[');
                                            open.push((name.to_string(), format!("]({u})")));
                                            i = skip_emptied_line(&mut out, text, i + len);
                                            continue;
                                        }
                                        (Some(x), None) | (None, Some(x)) => out.push_str(x),
                                        (None, None) => {}
                                    },
                                    // the chip sits after the sentence's space: don't leave it dangling before the break
                                    "Cite" if out.ends_with(' ') => {
                                        out.pop();
                                    }
                                    "title" if !self_closed => out.push_str("**"),
                                    _ => {}
                                }
                                if !self_closed {
                                    open.push((name.to_string(), if name == "title" { "**".to_string() } else { String::new() }));
                                }
                                i = skip_emptied_line(&mut out, text, i + len);
                                continue;
                            }
                        }
                    }
                }
            }
        }
        out.push(c);
        i += c.len_utf8();
    }
    // layout lines leave runs of blank lines: at most one empty line between paragraphs (code blocks untouched)
    let mut kept: Vec<&str> = Vec::new();
    let mut in_code = false;
    for line in out.split('\n') {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
        }
        let blank = !in_code && line.trim().is_empty();
        if blank && (kept.is_empty() || kept.last().is_some_and(|l| l.trim().is_empty())) {
            continue;
        }
        kept.push(line);
    }
    kept.join("\n")
}

/// ChatGPT sends a code block as `<CodeBlock language="text" editable>` ... `</CodeBlock>` (measured 14/10/2026, an
/// ASCII diagram): the page draws the box, the network has the tag. It becomes the fenced block it stands for, so
/// the diagram keeps its box and its spacing. Only a tag that starts its line counts, outside any fence; a block
/// still arriving is fenced from its first line.
fn codeblocks_to_fences(text: &str) -> String {
    if !text.contains("<CodeBlock") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 16);
    let (mut fenced, mut inside) = (false, false);
    for raw in text.split_inclusive('\n') {
        let mut line = raw;
        if !inside {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
            }
            let t = line.trim_start();
            if !fenced && t.starts_with("<CodeBlock") {
                // end of the opening tag: its first '>' outside double quotes
                let (mut quoted, mut end) = (false, None);
                for (i, c) in t.char_indices() {
                    match c {
                        '"' => quoted = !quoted,
                        '>' if !quoted => {
                            end = Some(i);
                            break;
                        }
                        _ => {}
                    }
                }
                if let Some(end) = end {
                    let tag = &t[..end];
                    let lang = tag.find("language=\"").and_then(|a| {
                        let v = &tag[a + 10..];
                        v.find('"').map(|e| &v[..e])
                    });
                    out.push_str("```");
                    out.push_str(lang.unwrap_or("").trim());
                    out.push('\n');
                    inside = true;
                    let after = &t[end + 1..];
                    line = after.strip_prefix("\r\n").or_else(|| after.strip_prefix('\n')).unwrap_or(after);
                    if line.is_empty() {
                        continue;
                    }
                }
            }
        }
        if inside {
            if let Some(at) = line.find("</CodeBlock>") {
                out.push_str(&line[..at]);
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```");
                out.push_str(&line[at + "</CodeBlock>".len()..]);
                inside = false;
            } else {
                out.push_str(line);
            }
            continue;
        }
        out.push_str(line);
    }
    out
}

/// True when an answer, after `strip_component_tags`, still carries the source of a generated interface instead of
/// text: template blocks (`{#each ...}`, `{#if ...}`), event handlers (`onClick=`), `GenUI.` calls, a closing tag,
/// or a tag with attributes. Seen on ChatGPT on 13/10/2026 (a moving checklist with buttons, cards with favicons):
/// the network has the program, only the page has what it shows. Code blocks and backtick spans are not looked at.
pub fn carries_generated_ui(text: &str) -> bool {
    let mut fenced = false;
    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        // outside inline code: every other piece between backticks
        for (n, part) in line.split('`').enumerate() {
            if n % 2 == 1 {
                continue;
            }
            if ["{#each", "{#if", "{/each}", "{/if}", "{:else", "onClick=", "onChange=", "GenUI."].iter().any(|m| part.contains(m)) {
                return true;
            }
            // A value the page fills in from its own data and the network never resolves: `{turn116917product7.displayed_price}`
            // in a price table (measured 14/10/2026). The page has the real figure.
            if part.match_indices("{turn").any(|(at, _)| part[at + 5..].starts_with(|c: char| c.is_ascii_digit())) {
                return true;
            }
            let mut rest = part;
            while let Some(lt) = rest.find('<') {
                let after = &rest[lt + 1..];
                let closing = after.starts_with('/');
                let name_src = if closing { &after[1..] } else { after };
                let name_len = name_src.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).unwrap_or(name_src.len());
                if name_len > 0 && name_src.starts_with(|c: char| c.is_ascii_alphabetic()) {
                    let tag_end = name_src.find('>').unwrap_or(name_src.len());
                    let body = &name_src[name_len..tag_end];
                    if closing && name_src[name_len..].starts_with('>') {
                        return true;
                    }
                    if !closing && tag_end < name_src.len() && (body.contains("=\"") || body.contains("={")) {
                        return true;
                    }
                }
                rest = after;
            }
        }
    }
    false
}

/// DeepSeek: SSE carrying a response object and then patches `{p, o, v}`; a patch with only `v` continues
/// the previous path. Fragments typed RESPONSE are the answer, THINK the reasoning; `BATCH` groups patches.
#[derive(Default)]
struct DeepSeek {
    sse: Sse,
    state: Value,
    last_path: String,
    last_op: String,
    ans: NetAnswer,
}
impl DeepSeek {
    fn patch(&mut self, v: &Value) {
        let path = v.get("p").and_then(Value::as_str).map(str::to_string);
        let op = v.get("o").and_then(Value::as_str).map(str::to_string);
        let val = v.get("v").cloned().unwrap_or(Value::Null);
        if let Some(p) = &path {
            self.last_path = p.clone();
        }
        if let Some(o) = &op {
            self.last_op = o.clone();
        } else if path.is_some() {
            self.last_op = "SET".into();
        }
        if self.last_op.eq_ignore_ascii_case("BATCH") {
            if let Value::Array(items) = &val {
                let base = self.last_path.clone();
                for it in items {
                    let mut it = it.clone();
                    if let Some(p) = it.get("p").and_then(Value::as_str) {
                        let full = if base.is_empty() { p.to_string() } else { format!("{base}/{p}") };
                        it["p"] = Value::String(full);
                    }
                    self.patch(&it);
                }
            }
            return;
        }
        if path.is_none() && op.is_none() && self.last_path.is_empty() {
            // The initial full object: `{"v": {"response": {...}}}`.
            if val.is_object() {
                self.state = val;
            }
            return;
        }
        let op = if self.last_op.eq_ignore_ascii_case("APPEND") { "append" } else { "replace" };
        let path = self.last_path.clone();
        if !apply_patch(&mut self.state, op, &path, &val) {
            self.ans.skipped += 1;
        }
        self.rebuild();
    }
    fn rebuild(&mut self) {
        let (mut text, mut reasoning) = (String::new(), String::new());
        if let Some(Value::Array(frags)) = self.state.pointer("/response/fragments") {
            for f in frags {
                let c = f.get("content").and_then(Value::as_str).unwrap_or("");
                match f.get("type").and_then(Value::as_str).unwrap_or("") {
                    "RESPONSE" => text.push_str(c),
                    t if t.starts_with("THINK") => reasoning.push_str(c),
                    _ => {}
                }
                if let Some(Value::Array(results)) = f.get("results") {
                    for r in results {
                        if let Some(url) = r.get("url").and_then(Value::as_str) {
                            push_source(&mut self.ans.sources, r.get("title").and_then(Value::as_str).unwrap_or(""), url, None);
                        }
                    }
                }
            }
        }
        // Inline citation markers (`[citation:3]`) point into the search results, which travel as sources: in the
        // text they are noise.
        self.ans.text = strip_citation_markers(&text);
        self.ans.reasoning = reasoning;
        if self.state.pointer("/response/status").and_then(Value::as_str) == Some("FINISHED") {
            self.ans.done = true;
        }
    }
    fn event(&mut self, data: &str) {
        match serde_json::from_str::<Value>(data) {
            Ok(v) if v.is_object() && (v.get("v").is_some() || v.get("p").is_some()) => self.patch(&v),
            Ok(_) => {}
            Err(_) => self.ans.skipped += 1,
        }
    }
}
impl Reader for DeepSeek {
    fn feed(&mut self, chunk: &str) {
        for d in self.sse.push(chunk) {
            self.event(&d);
        }
    }
    fn end(&mut self) {
        for d in self.sse.flush() {
            self.event(&d);
        }
        self.ans.done = true;
    }
    fn answer(&self) -> &NetAnswer {
        &self.ans
    }
}

/// Gemini: `)]}'` then frames `<length>\n<json>`; each frame is `[["wrb.fr", null, "<inner json>"]]`. In the
/// inner array the candidate is at [4][0], its text (cumulative, the whole answer so far) at [4][0][1][0].
/// The length counts UTF-16 units, so frames are cut by bracket balance instead of trusting it.
#[derive(Default)]
struct Gemini {
    objs: JsonArrays,
    ans: NetAnswer,
}
impl Reader for Gemini {
    fn feed(&mut self, chunk: &str) {
        for frame in self.objs.push(chunk) {
            let Ok(outer) = serde_json::from_str::<Value>(&frame) else { self.ans.skipped += 1; continue };
            let Some(items) = outer.as_array() else { continue };
            for item in items {
                let Some(inner_s) = item.get(2).and_then(Value::as_str) else { continue };
                let Ok(inner) = serde_json::from_str::<Value>(inner_s) else { self.ans.skipped += 1; continue };
                if let Some(t) = str_at(&inner, &["4", "0", "1", "0"]) {
                    let t = strip_ui_components(t);
                    if t.len() >= self.ans.text.len() {
                        self.ans.text = t;
                    }
                }
            }
        }
    }
    fn end(&mut self) {
        self.ans.done = true;
    }
    fn answer(&self) -> &NetAnswer {
        &self.ans
    }
}

/// Top-level JSON arrays in a text stream (Gemini frames), skipping the length lines and the `)]}'` guard.
#[derive(Default)]
struct JsonArrays {
    buf: String,
}
impl JsonArrays {
    fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        let bytes = self.buf.as_bytes();
        let (mut depth, mut in_str, mut esc, mut start, mut consumed) = (0i32, false, false, None, 0usize);
        for (i, &b) in bytes.iter().enumerate() {
            if in_str {
                if esc {
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == b'"' {
                    in_str = false;
                }
                continue;
            }
            match b {
                b'"' if depth > 0 => in_str = true,
                b'[' => {
                    if depth == 0 {
                        start = Some(i);
                    }
                    depth += 1;
                }
                b']' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        if let Some(s) = start.take() {
                            out.push(self.buf[s..=i].to_string());
                            consumed = i + 1;
                        }
                    }
                }
                _ => {}
            }
        }
        self.buf.drain(..consumed);
        out
    }
}

/// Mistral: lines `<n>:{"json": ...}`; a `message` carries patches on the assistant message (replace "/",
/// append on "/contentChunks/N/text" or "/content"). The answer is the message content, or its text chunks.
#[derive(Default)]
struct Mistral {
    objs: JsonObjects,
    msg: Value,
    ans: NetAnswer,
}
impl Reader for Mistral {
    fn feed(&mut self, chunk: &str) {
        for o in self.objs.push(chunk) {
            let Ok(v) = serde_json::from_str::<Value>(&o) else { self.ans.skipped += 1; continue };
            let j = v.get("json").unwrap_or(&v);
            if j.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            if let Some(Value::Array(patches)) = j.get("patches") {
                for p in patches {
                    let op = p.get("op").and_then(Value::as_str).unwrap_or("");
                    let path = p.get("path").and_then(Value::as_str).unwrap_or("");
                    let val = p.get("value").cloned().unwrap_or(Value::Null);
                    if !apply_patch(&mut self.msg, op, path, &val) {
                        self.ans.skipped += 1;
                    }
                }
            }
            let content = self.msg.get("content").and_then(Value::as_str).unwrap_or("");
            let mut text = String::new();
            if let Some(Value::Array(chunks)) = self.msg.get("contentChunks") {
                for c in chunks {
                    if let Some(t) = c.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
            }
            self.ans.text = if text.len() >= content.len() { text } else { content.to_string() };
            if let Some(Value::Array(refs)) = self.msg.get("references") {
                for r in refs {
                    if let Some(url) = r.get("url").and_then(Value::as_str) {
                        push_source(&mut self.ans.sources, r.get("title").and_then(Value::as_str).unwrap_or(""), url, None);
                    }
                }
            }
            if self.msg.get("generationStatus").and_then(Value::as_str) == Some("success") {
                self.ans.done = true;
            }
        }
    }
    fn end(&mut self) {
        self.ans.done = true;
    }
    fn answer(&self) -> &NetAnswer {
        &self.ans
    }
}

/// Perplexity: SSE snapshots with `blocks[]`. A block is either sent whole (`markdown_block`, ...) or as a
/// `diff_block {field, patches}` on its previous state. The answer text is the `ask_text` markdown block
/// (its `answer`, or its chunks joined); web results carry the sources.
#[derive(Default)]
struct Perplexity {
    sse: Sse,
    blocks: serde_json::Map<String, Value>,
    ans: NetAnswer,
}
impl Perplexity {
    fn event(&mut self, data: &str) {
        let v = match serde_json::from_str::<Value>(data) {
            Ok(v) => v,
            Err(e) => {
                if cfg!(test) { eprintln!("perplexity json: {e} len={}", data.len()); }
                self.ans.skipped += 1;
                return;
            }
        };
        if let Some(Value::Array(blocks)) = v.get("blocks") {
            for b in blocks {
                let usage = b.get("intended_usage").and_then(Value::as_str).unwrap_or("").to_string();
                if let Some(diff) = b.get("diff_block") {
                    let field = diff.get("field").and_then(Value::as_str).unwrap_or("").to_string();
                    let key = format!("{usage}#{field}");
                    let target = self.blocks.entry(key).or_insert(Value::Null);
                    if let Some(Value::Array(patches)) = diff.get("patches") {
                        for p in patches {
                            let op = p.get("op").and_then(Value::as_str).unwrap_or("");
                            let path = p.get("path").and_then(Value::as_str).unwrap_or("");
                            let val = p.get("value").cloned().unwrap_or(Value::Null);
                            if !apply_patch(target, op, path, &val) {
                                self.ans.skipped += 1;
                                if cfg!(test) { eprintln!("perplexity patch failed: {op} {path} on {}", target.to_string().chars().take(80).collect::<String>()); }
                            }
                        }
                    }
                } else {
                    for (k, val) in b.as_object().into_iter().flatten() {
                        if k != "intended_usage" {
                            self.blocks.insert(format!("{usage}#{k}"), val.clone());
                        }
                    }
                }
            }
        }
        self.rebuild();
        if v.get("final_sse_message").and_then(Value::as_bool) == Some(true)
            || v.get("status").and_then(Value::as_str) == Some("COMPLETED")
        {
            self.ans.done = true;
        }
    }
    fn rebuild(&mut self) {
        // The answer: prefer the dedicated markdown block, whatever its usage name ("ask_text", ...).
        let mut best = String::new();
        for (k, v) in &self.blocks {
            if !k.ends_with("#markdown_block") || k.starts_with("pending") {
                continue;
            }
            let answer = v.get("answer").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| {
                v.get("chunks")
                    .and_then(Value::as_array)
                    .map(|c| c.iter().filter_map(Value::as_str).collect::<String>())
                    .unwrap_or_default()
            });
            if answer.len() > best.len() {
                best = answer;
            }
        }
        // Current format (captured 15/09/2026): the answer is the text item of the workflow block, streamed as
        // `steps[].items[type=WORKFLOW_ITEM_TEXT].payload.text_payload.chunks`.
        for (k, v) in &self.blocks {
            if !k.ends_with("#workflow_block") {
                continue;
            }
            for step in v.get("steps").and_then(Value::as_array).into_iter().flatten() {
                for item in step.get("items").and_then(Value::as_array).into_iter().flatten() {
                    if item.get("type").and_then(Value::as_str) != Some("WORKFLOW_ITEM_TEXT") {
                        continue;
                    }
                    let t: String = item
                        .pointer("/payload/text_payload/chunks")
                        .and_then(Value::as_array)
                        .map(|c| c.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    if t.len() > best.len() {
                        best = t;
                    }
                }
            }
        }
        if !best.is_empty() {
            self.ans.text = strip_numeric_citations(&best);
        }
        for (k, v) in &self.blocks {
            if !k.starts_with("web_results#") {
                continue;
            }
            if let Some(Value::Array(results)) = v.get("web_results") {
                for r in results {
                    if let Some(url) = r.get("url").and_then(Value::as_str) {
                        let img = r
                            .pointer("/meta_data/images/0")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        push_source(&mut self.ans.sources, r.get("name").and_then(Value::as_str).unwrap_or(""), url, img);
                    }
                }
            }
        }
    }
}
impl Reader for Perplexity {
    fn feed(&mut self, chunk: &str) {
        for d in self.sse.push(chunk) {
            self.event(&d);
        }
    }
    fn end(&mut self) {
        for d in self.sse.flush() {
            self.event(&d);
        }
    }
    fn answer(&self) -> &NetAnswer {
        &self.ans
    }
}

/// Z.ai: SSE `{"type":"chat:completion","data":{"delta_content","phase"}}`; phase `thinking` is reasoning,
/// `answer` the text, `done` the end. `edit_content` replaces the text so far when present.
#[derive(Default)]
struct Zai {
    sse: Sse,
    ans: NetAnswer,
}
impl Reader for Zai {
    fn feed(&mut self, chunk: &str) {
        for d in self.sse.push(chunk) {
            let Ok(v) = serde_json::from_str::<Value>(&d) else { self.ans.skipped += 1; continue };
            let data = &v["data"];
            let phase = data.get("phase").and_then(Value::as_str).unwrap_or("");
            let delta = data.get("delta_content").and_then(Value::as_str).unwrap_or("");
            match phase {
                "thinking" => self.ans.reasoning.push_str(delta),
                "answer" => {
                    if let Some(e) = data.get("edit_content").and_then(Value::as_str) {
                        self.ans.text = e.to_string();
                    }
                    self.ans.text.push_str(delta);
                }
                "done" => self.ans.done = true,
                _ => {}
            }
            if data.get("done").and_then(Value::as_bool) == Some(true) {
                self.ans.done = true;
            }
        }
    }
    fn end(&mut self) {}
    fn answer(&self) -> &NetAnswer {
        &self.ans
    }
}

/// Qwen: SSE OpenAI-like `choices[0].delta` with a `phase` (`answer`, `think`/`thinking_summary`) and a
/// `status` that becomes `finished`.
#[derive(Default)]
struct Qwen {
    sse: Sse,
    ans: NetAnswer,
}
impl Reader for Qwen {
    fn feed(&mut self, chunk: &str) {
        for d in self.sse.push(chunk) {
            let Ok(v) = serde_json::from_str::<Value>(&d) else { self.ans.skipped += 1; continue };
            let Some(delta) = v.pointer("/choices/0/delta") else { continue };
            let content = delta.get("content").and_then(Value::as_str).unwrap_or("");
            match delta.get("phase").and_then(Value::as_str).unwrap_or("answer") {
                "answer" => self.ans.text.push_str(content),
                p if p.starts_with("think") => self.ans.reasoning.push_str(content),
                _ => {}
            }
            if delta.get("status").and_then(Value::as_str) == Some("finished") {
                self.ans.done = true;
            }
        }
    }
    fn end(&mut self) {
        self.ans.done = true;
    }
    fn answer(&self) -> &NetAnswer {
        &self.ans
    }
}

/// Grok: JSON events on a long-lived socket. Text in `response.chunk` `chunk.text.text`, split by channel
/// (the assistant response vs reasoning); `response.done` ends this answer (the socket stays open).
#[derive(Default)]
struct Grok {
    objs: JsonObjects,
    ans: NetAnswer,
    /// `event_id`s already applied. The ids are unique per event (`evt_<hash>_<n>`); a frame the socket delivers
    /// twice (a reattached conversation replaying its tail) must not append its text twice, or the answer comes out
    /// doubled and, being the longer one, wins over the page's correct reading (TODO 43). Chunks are tiny, so the
    /// set stays small.
    seen: std::collections::HashSet<String>,
}
impl Reader for Grok {
    fn feed(&mut self, chunk: &str) {
        for o in self.objs.push(chunk) {
            let Ok(v) = serde_json::from_str::<Value>(&o) else { self.ans.skipped += 1; continue };
            let ev = &v["event"];
            if let Some(id) = ev.get("event_id").and_then(Value::as_str) {
                if !self.seen.insert(id.to_string()) {
                    continue;
                }
            }
            match ev.get("type").and_then(Value::as_str) {
                Some("response.chunk") => {
                    let t = &ev["chunk"]["text"];
                    let text = t.get("text").and_then(Value::as_str).unwrap_or("");
                    match t.get("channel").and_then(Value::as_str).unwrap_or("") {
                        "CHANNEL_ASSISTANT_RESPONSE" | "" => self.ans.text.push_str(text),
                        _ => self.ans.reasoning.push_str(text),
                    }
                }
                Some("response.done") => self.ans.done = true,
                _ => {}
            }
        }
    }
    fn end(&mut self) {}
    fn answer(&self) -> &NetAnswer {
        &self.ans
    }
}

/// Removes the interface components Gemini embeds in its text stream (captured 15/09/2026: follow-up suggestion
/// chips as `<ElicitationsGroup ...> <Elicitation .../> </ElicitationsGroup>`). Only tags whose name starts with
/// an uppercase letter are touched: Markdown and HTML the model writes as content use lowercase tags.
fn strip_ui_components(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(i) = rest.find('<') else { break };
        let after = &rest[i + 1..];
        let name: String = after.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if name.is_empty() || !name.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
            out.push_str(&rest[..i + 1]);
            rest = after;
            continue;
        }
        out.push_str(&rest[..i]);
        let close = format!("</{name}>");
        if let Some(j) = rest.find(&close) {
            rest = &rest[j + close.len()..];           // paired component: drop it with its children
        } else if let Some(j) = rest.find("/>") {
            rest = &rest[j + 2..];                     // self-closing component
        } else {
            out.push_str(rest);                        // unterminated (still streaming): keep as is
            rest = "";
        }
    }
    out.push_str(rest);
    out.trim_end().to_string()
}

/// Removes DeepSeek-style `[citation:N]` markers (and the space before them).
fn strip_citation_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("[citation:") {
        let after = &rest[i + "[citation:".len()..];
        match after.find(']') {
            Some(j) if after[..j].chars().all(|c| c.is_ascii_digit() || c == ',' || c == ' ') => {
                out.push_str(rest[..i].trim_end_matches(' '));
                rest = &after[j + 1..];
            }
            _ => {
                out.push_str(&rest[..i + 1]);
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Perplexity's answer text carries its source references as bare numbers glued to the sentence ("...blu.[1][2][3]");
/// its page draws them as small chips, and the sources themselves reach Kotodama separately (`sources`). Removes a
/// `[n]` (one or two digits) only when it is glued to the text before it, never inside a code block or inline code
/// (`list[0]`), so a real "[1]" in prose after a space, or in code, stays. Known limit: an index written in plain
/// prose without backticks (x[1]) cannot be told apart from a glued citation and goes too.
fn strip_numeric_citations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code = false;
    for (li, line) in text.split('\n').enumerate() {
        if li > 0 {
            out.push('\n');
        }
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            out.push_str(line);
            continue;
        }
        if in_code {
            out.push_str(line);
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        let mut in_inline = false;
        while i < chars.len() {
            if chars[i] == '`' {
                in_inline = !in_inline;
            }
            if !in_inline && chars[i] == '[' && !out.is_empty() && !out.ends_with(char::is_whitespace) && !out.ends_with('(') {
                let mut k = i + 1;
                while k < chars.len() && k - i <= 2 && chars[k].is_ascii_digit() {
                    k += 1;
                }
                if k > i + 1 && k < chars.len() && chars[k] == ']' {
                    i = k + 1;
                    continue;
                }
            }
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn push_source(list: &mut Vec<Source>, title: &str, url: &str, image: Option<String>) {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return;
    }
    if let Some(existing) = list.iter_mut().find(|s| s.url == url) {
        if existing.title.is_empty() {
            existing.title = title.to_string();
        }
        if existing.image.is_none() {
            existing.image = image;
        }
        return;
    }
    list.push(Source { title: title.to_string(), url: url.to_string(), image });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replays a capture made with KOTO_NETPROBE (one text file per stream, chunks separated by a marker) when
    /// NETREAD_FIXTURES points at them. Skipped otherwise: raw captures carry account ids and stay local.
    fn replay(key: &str, url: &str, file: &str) -> Option<NetAnswer> {
        let dir = std::env::var("NETREAD_FIXTURES").ok()?;
        let data = std::fs::read_to_string(std::path::Path::new(&dir).join(file)).ok()?;
        let mut r = reader_for(key, url).expect("reader");
        for c in data.split("\n<<<CHUNK>>>\n") {
            r.feed(c);
        }
        r.end();
        Some(r.answer().clone())
    }

    #[test]
    fn sse_crlf_split_across_chunks() {
        let mut s = Sse::default();
        assert!(s.push("data: 1\r\n\r").is_empty());
        assert_eq!(s.push("\ndata: 2\r\n\r\n"), vec!["1".to_string(), "2".to_string()]);
    }

    #[test]
    fn sse_framing_handles_split_events() {
        let mut s = Sse::default();
        assert!(s.push("data: {\"a\":").is_empty());
        let out = s.push("1}\n\ndata: x\n\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string(), "x".to_string()]);
    }

    #[test]
    fn json_objects_ignore_braces_in_strings() {
        let mut j = JsonObjects::default();
        let out = j.push("{\"t\":\"a}b\"}{\"u\":");
        assert_eq!(out.len(), 1);
        assert_eq!(j.push("2}").len(), 1);
    }

    fn grok_chunk(id: &str, text: &str) -> String {
        format!(r#"{{"session_id":"s","event":{{"type":"response.chunk","event_id":"{id}","chunk":{{"text":{{"channel":"CHANNEL_ASSISTANT_RESPONSE","text":"{text}"}}}}}}}}"#)
    }

    #[test]
    fn grok_reader_joins_chunks_and_ends_on_done() {
        let mut g = Grok::default();
        g.feed(&grok_chunk("evt_a_1", "| A | B |\\n"));
        g.feed(&grok_chunk("evt_a_2", "| 1 | 2 |"));
        assert!(!g.answer().done);
        g.feed(r#"{"event":{"type":"response.done","event_id":"evt_a_3"}}"#);
        assert!(g.answer().done);
        assert_eq!(g.answer().text, "| A | B |\n| 1 | 2 |");
    }

    #[test]
    fn grok_reader_ignores_a_replayed_event() {
        // The same frames delivered twice (a reattached conversation replaying its tail), glued or in two reads.
        let mut g = Grok::default();
        let a = grok_chunk("evt_a_1", "Uno ");
        let b = grok_chunk("evt_a_2", "due");
        g.feed(&format!("{a}{b}"));
        g.feed(&format!("{a}{b}"));
        g.feed(&b);
        assert_eq!(g.answer().text, "Uno due");
    }

    #[test]
    fn gemini_ui_components_removed() {
        let t = "Fine.\n\n<ElicitationsGroup message=\"Vuoi?\">\n<Elicitation label=\"A\" query=\"B\"/>\n</ElicitationsGroup>";
        assert_eq!(strip_ui_components(t), "Fine.");
        assert_eq!(strip_ui_components("a <b>x</b> 2 < 3"), "a <b>x</b> 2 < 3");
    }

    #[test]
    fn perplexity_numeric_citations_removed() {
        assert_eq!(strip_numeric_citations("Il cielo appare azzurro.[1][2][3]"), "Il cielo appare azzurro.");
        assert_eq!(strip_numeric_citations("Vedi [1] qui"), "Vedi [1] qui");
        assert_eq!(strip_numeric_citations("```\nlet a = b[1];\n```"), "```\nlet a = b[1];\n```");
        assert_eq!(strip_numeric_citations("(vedi [2])"), "(vedi [2])");
        assert_eq!(strip_numeric_citations("usa `lista[0]` qui.[1]"), "usa `lista[0]` qui.");
        assert_eq!(strip_numeric_citations("uno.[1]\ndue.[12]"), "uno.\ndue.");
    }

    #[test]
    fn citation_markers_removed() {
        assert_eq!(strip_citation_markers("Fari antichi [citation:2]. Fine [citation:1,3]"), "Fari antichi. Fine");
        assert_eq!(strip_citation_markers("[nota] resta"), "[nota] resta");
    }

    #[test]
    fn deepseek_patches() {
        let mut r = reader_for("deepseek", "/api/v0/chat/completion").unwrap();
        r.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"RESPONSE\",\"content\":\"**\"}],\"status\":\"WIP\"}}}\n\n");
        r.feed("data: {\"p\":\"response/fragments/-1/content\",\"o\":\"APPEND\",\"v\":\"Tit\"}\n\n");
        r.feed("data: {\"v\":\"olo**\"}\n\n");
        r.feed("data: {\"p\":\"response/status\",\"v\":\"FINISHED\"}\n\n");
        assert_eq!(r.answer().text, "**Titolo**");
        assert!(r.answer().done);
    }

    #[test]
    fn claude_deltas() {
        let mut r = reader_for("anthropic", "/api/x/completion").unwrap();
        r.feed("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hm\"}}\n\n");
        r.feed("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"# Ciao\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(r.answer().text, "# Ciao");
        assert_eq!(r.answer().reasoning, "hm");
        assert!(r.answer().done);
    }

    #[test]
    fn openai_deltas() {
        // The shape captured on 17/09/2026: the message opens, the answer arrives in appends, and a final
        // `patch` closes it. The user's own question is echoed first and must NOT end up in the answer.
        let mut r = reader_for("openai", "https://chatgpt.com/backend-api/f/conversation").unwrap();
        r.feed("event: delta\ndata: {\"p\":\"\",\"o\":\"add\",\"v\":{\"message\":{\"author\":{\"role\":\"user\"},\"content\":{\"content_type\":\"text\",\"parts\":[\"la domanda\"]}}}}\n\n");
        r.feed("event: delta\ndata: {\"p\":\"\",\"o\":\"add\",\"v\":{\"message\":{\"author\":{\"role\":\"assistant\"},\"content\":{\"content_type\":\"text\",\"parts\":[\"\"]}}}}\n\n");
        r.feed("event: delta\ndata: {\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\"1. **Frasi\"}\n\n");
        r.feed("event: delta\ndata: {\"v\":\" brevi**\"}\n\n");
        r.feed("event: delta\ndata: {\"p\":\"\",\"o\":\"patch\",\"v\":[{\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\" e dirette.\"},{\"p\":\"/message/status\",\"o\":\"replace\",\"v\":\"finished_successfully\"}]}\n\n");
        r.feed("data: {\"type\":\"message_stream_complete\",\"conversation_id\":\"x\"}\n\n");
        assert_eq!(r.answer().text, "1. **Frasi brevi** e dirette.");
        assert!(r.answer().done);
    }

    #[test]
    fn generated_interface_recognised() {
        // 13/10/2026: a moving checklist and a product card, as their source arrived from the network
        assert!(carries_generated_ui("{#each phases as phase,i}\n**{phase.title}**\n{/each}"));
        assert!(carries_generated_ui("<button color=\"secondary\" onClick={()=>setChecked({})}>Azzera</button>"));
        assert!(carries_generated_ui("Prezzo <favicon url=\"https://www.apple.com\" size=\"24px\"/> 999 euro"));
        assert!(carries_generated_ui("<grid-item>\nTesto\n</grid-item>"));
        // plain answers, and code, are not
        for t in ["Se a<b e b<c allora a<c.", "usa `List<String>` e `{#each x}`", "```html\n<div class=\"x\"></div>\n```", "Vec<T> e Result<T, E>"] {
            assert!(!carries_generated_ui(t), "{t}");
        }
    }

    #[test]
    fn openai_component_tags_removed() {
        let strip = |t: &str| strip_component_tags(t, true);
        // Seen on 08/10 and 13/10/2026: entities, image groups and writing blocks arrived as raw markup in the chat.
        let entity = "Potrebbe essere <Entity category=\"musical_artist\" value=\"Katy Perry\"/>, nel video di \
                      <Entity category=\"song\" value=\"Dark Horse\" disambig=\"Katy Perry, 2013\"/>, dove sale.";
        assert_eq!(strip(entity), "Potrebbe essere Katy Perry, nel video di Dark Horse, dove sale.");
        let images = "egizia.\n\n<AsyncImageGroup query={[\"Katy Perry > Dark Horse\", \"x\"]}/>\n\nOppure Kylie.";
        assert_eq!(strip(images), "egizia.\n\nOppure Kylie.");
        let writing = "<WritingBlock id=\"58321\" variant=\"standard\">Quando scrivo «Tu che ne pensi?».</WritingBlock>";
        assert_eq!(strip(writing), "Quando scrivo «Tu che ne pensi?».");
        // still arriving: the unfinished component is held back
        assert_eq!(strip_component_tags("egizia.\n\n<AsyncImageGroup query={[\"", false), "egizia.\n");
        assert_eq!(strip_component_tags("Potrebbe essere <Enti", false), "Potrebbe essere ");
        // a symbol that says something stays as a symbol, one that does not goes
        assert_eq!(strip("| <icon name=\"check-circle\" color=\"success\" inline /> Sì |"), "| \u{2713} Sì |");
        assert_eq!(strip("<icon name=\"star\"/> ok"), " ok");
        // Layout boxes around a card (13/10/2026, «Lionel Messi»): the boxes go, the title stays as bold text.
        let card = "<row align=\"center\" gap={3}>\n  <box flex=\"1\" gap={1}>\n    <title size=\"lg\">Lionel Messi</title>\n    \
                    Otto Palloni d'Oro.\n  </box>\n</row>\n\nAltro testo.";
        assert_eq!(strip(card), "    **Lionel Messi**\n    Otto Palloni d'Oro.\n\nAltro testo.");
        // A drawing is not text.
        let svg = "Schema:\n<box border radius=\"lg\"><svg viewBox=\"0 0 10 10\"><g><path d=\"M0 0\"/></g></svg></box>\nFine.";
        assert_eq!(strip(svg), "Schema:\nFine.");
        // Prose is never cut or mangled (the cases of the 13/10/2026 review): it all stays as it is, finished or not.
        for prose in [
            "se a < b e <div> resta",
            "Se A<B e B<C allora A<C, quindi l'ordine e' totale.",
            "Il ciclo va finche' i<N, l'indice parte da zero. Poi altro testo importante.",
            "Il ciclo for (i=0; i<N; i++) l'ho scritto io. Fine.",
            "Puoi usare <svg> per disegnare grafica vettoriale. Il resto...",
            "In Rust usa Result<T, E> oppure HashMap<String, Vec<u8>> per la mappa.",
            "In Java scrivi Map<String, Integer> m = ...",
            "Il tag <title> definisce il titolo della pagina, mentre <caption> descrive una tabella.",
            "Sintassi: echo <text>, poi ORDER BY <column> ordina.",
            "Lista:\n- <row> e <box> sono layout",
            "Il componente <Button onClick={f}>Invia</Button> manda il modulo.",
            "Usa <Title> per il titolo. Ok.",
            "Usa il componente <Link> di Next.js per navigare.",
            "Il componente <Cite> e poi testo.",
            "Scrivi <Entity> nel campo. Fine.",
            "Metti il titolo tra <title> e </title> nella sezione head.",
            "usa Vec<T> e `<Entity value=\"x\"/>`",
            "```jsx\n<Entity value=\"x\"/>\n```\nfine",
        ] {
            assert_eq!(strip(prose), prose);
        }
        // While it is still arriving, prose that looks like the start of a tag is held back for a moment, never lost:
        // the finished answer has it all.
        assert_eq!(strip_component_tags("Se A<B e B<C allora A<C.", false), "Se A<B e B<C allora A");
        assert_eq!(strip_component_tags("Se A<B e B<C allora A<C.", true), "Se A<B e B<C allora A<C.");
    }

    #[test]
    fn openai_codeblock_becomes_a_fence() {
        // Measured 14/10/2026 (an ASCII diagram): the network has `<CodeBlock>`, the answer must keep its box.
        let raw = "Ecco lo schema:\n\n<CodeBlock language=\"text\" editable>\n  +----+\n  | A  |\n  +----+\n</CodeBlock>\n\nSpiegazione <qui>.";
        let want = "Ecco lo schema:\n\n```text\n  +----+\n  | A  |\n  +----+\n```\n\nSpiegazione <qui>.";
        assert_eq!(strip_component_tags(raw, true), want);
        assert!(!carries_generated_ui(&strip_component_tags(raw, true)));
        // still arriving: fenced from its first line, nothing of the tag shows
        assert_eq!(
            strip_component_tags("Ecco:\n<CodeBlock language=\"python\">\nprint(1)\n", false),
            "Ecco:\n```python\nprint(1)\n"
        );
        assert_eq!(strip_component_tags("Ecco:\n<CodeBl", false), "Ecco:\n");
        // no language, and a tag quoted in a fence or in prose is left alone
        assert_eq!(strip_component_tags("<CodeBlock>\nx<y\n</CodeBlock>", true), "```\nx<y\n```");
        let quoted = "```html\n<CodeBlock language=\"x\">\n```\nfine";
        assert_eq!(strip_component_tags(quoted, true), quoted);
    }

    #[test]
    fn openai_link_with_body_keeps_its_name() {
        // Measured 14/10/2026 (a table of shops): "https://...htmlMediaWorld" came out with the name glued on.
        let row = "| WH-CH520 | <Link url=\"https://www.mediaworld.it/a.html?x=1\">MediaWorld</Link> |";
        assert_eq!(strip_component_tags(row, true), "| WH-CH520 | [MediaWorld](https://www.mediaworld.it/a.html?x=1) |");
        // still arriving: the name shows as it comes, the address is added when the tag closes
        assert_eq!(strip_component_tags("| <Link url=\"https://a.it/\">Media", false), "| [Media");
        // a self-closed one and one with a title keep their old form
        assert_eq!(strip_component_tags("<Link url=\"https://a.it/\"/> e", true), "https://a.it/ e");
        assert_eq!(strip_component_tags("<Link url=\"https://a.it/\" title=\"A\"/>", true), "[A](https://a.it/)");
    }

    #[test]
    fn openai_bare_caption_under_a_table() {
        let t = "| a | b |\n|---|---|\n| 1 | 2 |\n\n<caption>Prezzi IVA inclusa. <Cite refs={[\"x\"]}/></caption>\n\nFine.";
        let out = strip_component_tags(t, true);
        assert_eq!(out, "| a | b |\n|---|---|\n| 1 | 2 |\n\nPrezzi IVA inclusa.\n\nFine.");
        assert!(!carries_generated_ui(&out));
        // quoted mid-sentence it stays
        let prose = "Il tag <caption> descrive una tabella e </caption> la chiude.";
        assert_eq!(strip_component_tags(prose, true), prose);
    }

    #[test]
    fn unresolved_page_values_mean_generated_interface() {
        // Measured 14/10/2026: a price table whose cells were `{turn116917product7.displayed_price}`.
        assert!(carries_generated_ui("| Pixel 10 | **{turn116917product7.displayed_price}** |"));
        assert!(!carries_generated_ui("In Svelte scrivi {turnover} e {turn} qui."));
        assert!(!carries_generated_ui("```\n{turn116917product7.price}\n```"));
    }

    #[test]
    fn unclosed_svg_is_held_back_only_near_the_end() {
        // An `<svg ...>` that never closes must not hide the rest of the answer until the delivery.
        let tail = "x".repeat(600);
        let t = format!("Schema: <svg viewBox=\"0 0 10 10\"> {tail}");
        assert_eq!(strip_component_tags(&t, false), t);
        // a drawing that has just started is still held back
        assert_eq!(strip_component_tags("Schema: <svg viewBox=\"0 0 10 10\"> <g>", false), "Schema: ");
    }

    /// Frames of the shape captured on 13/10/2026 (conversation with a web search), reduced to what matters: the
    /// page streams into a child message and every operation is repeated on a "logical" one (`c` 2 and 3 for the
    /// text before the search, `c` 9 and 3 for the one after it, the tool call and its results in between).
    fn mirrored_search_frames() -> Vec<String> {
        let msg = |c: u32, recipient: &str, channel: &str| {
            format!(
                r#"{{"v":{{"message":{{"author":{{"role":"assistant"}},"content":{{"content_type":"text","parts":[""]}},"recipient":"{recipient}","channel":{channel}}}}},"c":{c}}}"#
            )
        };
        let app = |c: u32, text: &str| format!(r#"{{"p":"/message/content/parts/0","o":"append","v":{text:?},"c":{c}}}"#);
        let patch = |c: u32, text: &str| format!(r#"{{"p":"","o":"patch","v":[{{"p":"/message/content/parts/0","o":"append","v":{text:?}}}],"c":{c}}}"#);
        let mut f = vec![
            r#"{"p":"","o":"add","v":{"message":{"author":{"role":"user"},"content":{"content_type":"text","parts":["la domanda"]}}},"c":0}"#.to_string(),
            msg(2, "all", "\"final\""),
            msg(3, "all", "\"final\""),
            app(2, "Prima parte"),
            patch(3, "Prima parte"),
            app(2, ", poi la ricerca.

"),
            patch(3, ", poi la ricerca.

"),
            msg(4, "web.run", "null"),
            r#"{"v":{"message":{"author":{"role":"tool","name":"web.run"},"content":{"content_type":"text","parts":[""]}}},"c":5}"#.to_string(),
            msg(9, "all", "\"final\""),
            // a patch, a status change inside one, then bare lists of operations: they continue the patch
            r#"{"o":"patch","v":[],"c":4}"#.to_string(),
            r#"{"v":[{"p":"/message/end_turn","o":"replace","v":null}],"c":3}"#.to_string(),
            r#"{"v":[{"p":"/message/content/parts/0","o":"append","v":"Seconda parte <Cite ref=\"a\"/>"}],"c":9}"#.to_string(),
            r#"{"v":[{"p":"/message/content/parts/0","o":"append","v":"Seconda parte <Cite ref=\"a\"/>"}],"c":3}"#.to_string(),
            app(9, ", fine."),
            patch(3, ", fine."),
        ];
        f.push(r#"{"type":"message_stream_complete"}"#.to_string());
        f
    }

    #[test]
    fn openai_mirrored_messages_counted_once() {
        // Seen on 13/10/2026: with a web search both texts of the turn came out twice, "Ho effettuHo effettu".
        let want = "Prima parte, poi la ricerca.

Seconda parte, fine.";
        let mut r = reader_for("openai", "https://chatgpt.com/backend-api/f/conversation").unwrap();
        for f in mirrored_search_frames() {
            r.feed(&format!("event: delta
data: {f}

"));
        }
        assert_eq!(r.answer().text, want);
        assert!(r.answer().done);
        // the same stream cut in the middle of a frame, one byte at a time
        let mut r = reader_for("openai", "https://chatgpt.com/backend-api/f/conversation").unwrap();
        let all: String = mirrored_search_frames().iter().map(|f| format!("event: delta
data: {f}

")).collect();
        for ch in all.chars() {
            r.feed(&ch.to_string());
        }
        assert_eq!(r.answer().text, want);
    }

    #[test]
    fn openai_writing_block_directives_removed() {
        // Seen on 26/09/2026: a poem arrived as a "writing" block, and its markers ended up in the chat.
        let mut r = reader_for("openai", "https://chatgpt.com/backend-api/f/conversation").unwrap();
        r.feed("event: delta\ndata: {\"p\":\"\",\"o\":\"add\",\"v\":{\"message\":{\"author\":{\"role\":\"assistant\"},\"content\":{\"content_type\":\"text\",\"parts\":[\"\"]}}}}\n\n");
        r.feed("event: delta\ndata: {\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\":::writ\"}\n\n");
        assert_eq!(r.answer().text, "");
        r.feed("event: delta\ndata: {\"v\":\"ing{variant=\\\"document\\\" id=\\\"58321\\\" title=\\\"La Luna\\\"}\\nNel cielo\"}\n\n");
        assert_eq!(r.answer().text, "Nel cielo");
        r.feed("event: delta\ndata: {\"v\":\" la luna.\\n:::\"}\n\n");
        assert_eq!(r.answer().text, "Nel cielo la luna.");
        assert_eq!(strip_block_directives("use ::std::io;\na :: b"), "use ::std::io;\na :: b");
    }

    #[test]
    fn captured_streams() {
        let cases = [
            ("anthropic", "/api/o/chat_conversations/c/completion", "anthropic-run3.txt"),
            // October 2026: the timeline stream, as the page forwards it (base64 lines)
            ("anthropic", TIMELINE_URL, "anthropic-timeline-cap2.txt"),
            ("anthropic", TIMELINE_URL, "anthropic-timeline-cap4.txt"),
            ("anthropic", TIMELINE_URL, "anthropic-timeline-cap5.txt"),
            ("anthropic", TIMELINE_URL, "anthropic-timeline-cap6.txt"),
            // a stream the page opened in the middle of an essay (joined mid-answer)
            ("anthropic", TIMELINE_URL, "anthropic-timeline-cap7.txt"),
            ("deepseek", "/api/v0/chat/completion", "deepseek-run3.txt"),
            ("gemini", "/StreamGenerate", "gemini-run3.txt"),
            ("mistral", "/api/chat", "mistral-run3.txt"),
            ("perplexity", "/rest/sse/perplexity_ask", "perplexity-run3.txt"),
            ("zai", "/api/v2/chat/completions", "zai-run3.txt"),
            ("qwen", "/api/v2/chat/completions", "qwen-run3.txt"),
            ("grok", "wss://grok.com/ws/mgw/", "grok-run3.txt"),
            ("openai", "/backend-api/f/conversation", "openai-run3.txt"),
        ];
        for (k, u, f) in cases {
            if let Some(a) = replay(k, u, f) {
                eprintln!("--- {k}: done={} skipped={} text={}B reasoning={}B sources={}\n{}", a.done, a.skipped, a.text.len(), a.reasoning.len(), a.sources.len(), a.text.chars().take(4000).collect::<String>());
            }
        }
    }

    // ---- Claude's timeline stream (ConversationService/StreamTimeline), October 2026 ----
    //
    // Protobuf in Connect framing (5-byte header: flags, big-endian length; flag 1 = gzip, flag 2 = trailer),
    // forwarded by the page observer as base64, one line per received chunk. The frames are built here with a
    // tiny encoder in the shape measured on 14/10/2026; the `REAL_*` bodies are captured frames (ids only).
    const TIMELINE_URL: &str = "https://claude.ai/claudeai-rpc/anthropic.bard.api.v1alpha.ConversationService/StreamTimeline";
    const REAL_BLOCK: &str = "cblk_000000000000000000000001";

    fn varint(mut n: u64) -> Vec<u8> {
        let mut o = Vec::new();
        loop {
            let b = (n & 127) as u8;
            n >>= 7;
            if n == 0 {
                o.push(b);
                return o;
            }
            o.push(b | 128);
        }
    }
    fn pb_len(field: u32, body: &[u8]) -> Vec<u8> {
        let mut o = varint(((field as u64) << 3) | 2);
        o.extend(varint(body.len() as u64));
        o.extend_from_slice(body);
        o
    }
    fn pb_str(field: u32, s: &str) -> Vec<u8> {
        pb_len(field, s.as_bytes())
    }
    fn pb_num(field: u32, v: u64) -> Vec<u8> {
        let mut o = varint((field as u64) << 3);
        o.extend(varint(v));
        o
    }
    fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.iter().flatten().copied().collect()
    }
    /// One event of the stream: `state` parts (field 1) and an optional delta (field 2), wrapped as the server does.
    fn tl_frame(state: &[Vec<u8>], delta: Option<Vec<u8>>) -> Vec<u8> {
        let mut ev = pb_str(10, "1791960000000-0");
        ev.extend(pb_num(11, 1));
        if !state.is_empty() {
            ev.extend(pb_len(1, &cat(state)));
        }
        if let Some(d) = delta {
            ev.extend(pb_len(2, &d));
        }
        pb_len(1, &ev)
    }
    fn conv(status: u64) -> Vec<u8> {
        pb_len(2, &cat(&[pb_str(1, "conv-1"), pb_num(3, status)]))
    }
    fn message(id: &str, role: u64) -> Vec<u8> {
        pb_len(3, &cat(&[pb_str(1, id), pb_str(2, "conv-1"), pb_num(3, role)]))
    }
    fn group(id: &str, msg: &str, kind: u64) -> Vec<u8> {
        pb_len(4, &cat(&[pb_str(1, id), pb_str(2, msg), pb_num(4, kind)]))
    }
    /// A block snapshot. `kind` 1 = reasoning (text in field 12.1), 2 = text (field 9).
    fn block(id: &str, grp: &str, kind: u64, text: &str) -> Vec<u8> {
        let mut b = cat(&[pb_str(1, id), pb_str(2, grp), pb_len(10, &pb_num(1, kind))]);
        if !text.is_empty() {
            b.extend(if kind == 1 { pb_len(12, &pb_str(1, text)) } else { pb_str(9, text) });
        }
        pb_len(5, &b)
    }
    fn delta(id: &str, text: &str) -> Vec<u8> {
        cat(&[pb_str(1, id), pb_str(2, text)])
    }
    fn connect(flags: u8, body: &[u8]) -> Vec<u8> {
        let mut o = vec![flags];
        o.extend((body.len() as u32).to_be_bytes());
        o.extend_from_slice(body);
        o
    }
    fn b64_line(bytes: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in bytes.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                if i <= c.len() {
                    s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    s.push('=');
                }
            }
        }
        s.push('\n');
        s
    }
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
    }
    /// A whole answer as the page forwards it: reasoning, then text, with the user's own message echoed first.
    fn timeline_answer() -> Vec<Vec<u8>> {
        vec![
            tl_frame(&[conv(2)], None),
            // the user's own message: role 1, a text block that must never end up in the answer
            tl_frame(&[conv(2), message("u1", 1), group("g-user", "u1", 1), block("b-user", "g-user", 2, "la domanda")], None),
            tl_frame(&[conv(2), message("a1", 2), group("g-think", "a1", 2), block("b-think", "g-think", 1, "")], None),
            tl_frame(&[], Some(cat(&[pb_str(1, "b-think"), pb_len(6, &pb_str(1, "Verifico i dati"))]))),
            tl_frame(&[group("g-text", "a1", 1), block("b-text", "g-text", 2, "**Ri")], None),
            tl_frame(&[], Some(delta("b-text", "sposta** in due"))),
            tl_frame(&[], Some(delta("b-text", " righe\n\nFine."))),
            // a citation attached to the text
            tl_frame(
                &[],
                Some(cat(&[
                    pb_str(1, "b-text"),
                    pb_len(4, &cat(&[pb_str(1, "c1"), pb_str(2, "Titolo"), pb_str(3, "https://example.org/a"), pb_len(6, &pb_str(4, "https://example.org/f.png"))])),
                ])),
            ),
            tl_frame(&[block("b-text", "g-text", 2, "**Risposta** in due righe\n\nFine.")], None),
            tl_frame(&[conv(1)], None),
        ]
    }
    fn feed_frames(r: &mut Box<dyn Reader>, frames: &[Vec<u8>]) {
        for f in frames {
            r.feed(&b64_line(&connect(0, f)));
        }
    }

    #[test]
    fn claude_timeline_answer_reasoning_and_end() {
        let mut r = reader_for("anthropic", TIMELINE_URL).expect("a reader for StreamTimeline");
        let frames = timeline_answer();
        assert_eq!(r.working(), None);
        feed_frames(&mut r, &frames[..frames.len() - 1]);
        assert_eq!(r.working(), Some(true));
        // everything is there, but the conversation is still busy: not finished
        assert_eq!(r.answer().text, "**Risposta** in due righe\n\nFine.");
        assert!(!r.answer().done);
        feed_frames(&mut r, &frames[frames.len() - 1..]);
        assert_eq!(r.working(), Some(false));
        assert!(r.answer().done);
        assert!(r.done_is_explicit());
        assert_eq!(r.answer().text, "**Risposta** in due righe\n\nFine.");
        assert_eq!(r.answer().reasoning, "Verifico i dati");
        assert_eq!(r.answer().sources.len(), 1);
        assert_eq!(r.answer().sources[0].url, "https://example.org/a");
        assert_eq!(r.answer().skipped, 0);
    }

    #[test]
    fn claude_timeline_deltas_alone_build_the_text() {
        // A stream that carries no final snapshot: the deltas are the text.
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(
            &mut r,
            &[
                tl_frame(&[conv(2), message("a1", 2), group("g", "a1", 1), block("b", "g", 2, "")], None),
                tl_frame(&[], Some(delta("b", "Ciao "))),
                tl_frame(&[], Some(delta("b", "mondo"))),
                tl_frame(&[conv(1)], None),
            ],
        );
        assert_eq!(r.answer().text, "Ciao mondo");
        assert!(r.answer().done);
    }

    #[test]
    fn claude_timeline_cut_anywhere() {
        // The page batches chunks every 60 ms: a line, a frame or a multi-byte letter can be cut anywhere.
        let all: String = timeline_answer().iter().map(|f| b64_line(&connect(0, f))).collect();
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        for ch in all.chars() {
            r.feed(&ch.to_string());
        }
        assert_eq!(r.answer().text, "**Risposta** in due righe\n\nFine.");
        assert!(r.answer().done);
        // a stream joined mid-answer: no message or group record, only a delta with accented letters
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(&mut r, &[tl_frame(&[conv(2)], None), tl_frame(&[], Some(delta("b", "è già")))]);
        assert_eq!(r.answer().text, "è già");
    }

    #[test]
    fn claude_timeline_idle_before_busy_is_not_an_end() {
        // A stream opened between turns starts idle; that is not the end of OUR answer.
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(&mut r, &[tl_frame(&[conv(1)], None)]);
        assert!(!r.answer().done);
        assert_eq!(r.working(), None);
        // ... and the heartbeat (an empty message every 20 s) is harmless
        r.feed(&b64_line(&connect(0, &hex("0a023200"))));
        assert!(!r.answer().done);
        assert_eq!(r.answer().text, "");
        assert_eq!(r.answer().skipped, 0);
    }

    #[test]
    fn claude_timeline_busy_again_reopens_the_answer() {
        // Tool use: the conversation can go idle between steps; a later busy frame means it is not over.
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(
            &mut r,
            &[tl_frame(&[conv(2), message("a1", 2), group("g", "a1", 1), block("b", "g", 2, "Prima")], None), tl_frame(&[conv(1)], None)],
        );
        assert!(r.answer().done);
        feed_frames(&mut r, &[tl_frame(&[conv(2)], None)]);
        assert!(!r.answer().done);
    }

    #[test]
    fn claude_timeline_gzip_and_trailer_frames() {
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(&mut r, &[tl_frame(&[conv(2), message("a1", 2), group("g", "a1", 1), block(REAL_BLOCK, "g", 2, "Un treno")], None)]);
        // the captured delta " incrociano alle 10", gzip-compressed (flag 1), then the trailer (flag 2)
        let gz = hex("1f8b080000000000020ae3f20e12e4e23734b734b43433b03036343734d73588601432e1924d4ecac98e37c00e0c85841532f3928bf2933313f3f21512737252150c0d0001952ce44d000000");
        r.feed(&b64_line(&connect(1, &gz)));
        r.feed(&b64_line(&connect(2, br#"{"metadata":{"Stream-Close-Reason":["cadence"]}}"#)));
        assert_eq!(r.answer().text, "Un treno incrociano alle 10");
        assert_eq!(r.answer().skipped, 0);
    }


    /// A widget tool block as the timeline sends it (measured 16/10/2026, `mcp__widgets__translation_display_v0`): no
    /// kind, a title, the structured payload in field 13 and, when `with_text`, a text fallback in field 9.
    fn translation_widget(id: &str, grp: &str, with_text: bool) -> Vec<u8> {
        let card = cat(&[
            pb_str(1, "Inglese"),
            pb_str(2, "Italiano"),
            pb_str(3, "en"),
            pb_str(4, "it"),
            pb_str(5, "Threat detected.\nOne more finding."),
            pb_str(6, "Minaccia rilevata.\nUn altro riscontro."),
            pb_str(8, "Traduzione dell'avviso"),
        ]);
        let mut b = cat(&[pb_str(1, id), pb_str(2, grp), pb_num(4, 1), pb_len(13, &pb_len(33, &card)), pb_str(15, "mcp__widgets__translation_display_v0")]);
        if with_text {
            b.extend(cat(&[pb_str(9, "TESTO DEL BLOCCO"), pb_len(10, &pb_num(1, 2))]));
        }
        pb_len(5, &b)
    }

    #[test]
    fn claude_timeline_translation_widget_without_text_is_read_from_its_payload() {
        // The page draws the card in an isolated iframe: when the block has no field 9 the payload is the only copy.
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(
            &mut r,
            &[
                tl_frame(&[conv(2)], None),
                tl_frame(&[conv(2), message("a1", 2), group("g-w", "a1", 2), translation_widget("b-w", "g-w", false)], None),
                tl_frame(&[group("g-text", "a1", 1), block_final("b-text", "g-text", "Ho lasciato invariati i termini tecnici.")], None),
                tl_frame(&[conv(1)], None),
            ],
        );
        assert_eq!(
            r.answer().text,
            "Traduzione dell'avviso\n\nInglese → Italiano\n\nThreat detected.\nOne more finding.\n\nMinaccia rilevata.\nUn altro riscontro.\n\nHo lasciato invariati i termini tecnici."
        );
        assert!(r.answer().done);
    }

    #[test]
    fn claude_timeline_widget_with_its_own_text_is_not_doubled() {
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(
            &mut r,
            &[
                tl_frame(&[conv(2)], None),
                tl_frame(&[conv(2), message("a1", 2), group("g-w", "a1", 2), translation_widget("b-w", "g-w", true)], None),
                tl_frame(&[group("g-text", "a1", 1), block_final("b-text", "g-text", "Commento.")], None),
                tl_frame(&[conv(1)], None),
            ],
        );
        assert_eq!(r.answer().text, "TESTO DEL BLOCCO\n\nCommento.");
    }

    #[test]
    fn html_text_keeps_code_out_and_reads_what_is_shown() {
        // '>' inside a quoted attribute does not end the tag
        assert_eq!(html_text("<p>Total</p><button onclick=\"go(x>0)\">Calc</button>"), "Total\nCalc");
        assert_eq!(html_text("<button onclick=\"() => run()\">Run</button><p>Hi there</p>"), "Run\nHi there");
        // a comment is skipped whatever it contains
        assert_eq!(html_text("<!-- note > here --><p>Text</p>"), "Text");
        // hidden elements, nested ones of the same name, and a void element carrying the marker
        assert_eq!(html_text("<div class=\"sr-only\"><div>in</div>leak</div><p>shown</p>"), "shown");
        assert_eq!(html_text("<svg><svg>inner</svg>chart label</svg><p>shown</p>"), "shown");
        assert_eq!(html_text("<input class=\"sr-only\" id=\"a\"><p>Visible answer text</p>"), "Visible answer text");
        assert_eq!(html_text("<template>tpl</template><noscript>raw</noscript><style>p{}</style><p>ok</p>"), "ok");
        // a custom element whose name starts like a hidden one is not hidden
        assert_eq!(html_text("<script-card><p>visible</p></script-card><p>tail</p>"), "visible\ntail");
        assert_eq!(html_text("<svg-icon>label</svg-icon><p>tail</p>"), "label\ntail");
        // a bare '<' is text, entities are decoded once
        assert_eq!(html_text("<p>5 < 6 and more</p>"), "5 < 6 and more");
        assert_eq!(html_text("<p>caff&egrave; &#8594; &#x27;a&#x27; &amp;lt; &unknown;</p>"), "caffè → 'a' &lt; &unknown;");
        // script and style are raw text: no tag inside them, the slash of `<script/>` means nothing
        assert_eq!(html_text("<script>for(let i=0;i<items.length;i++){el.innerHTML += '<li>'+items[i]+'</li>'}</script><p>shown</p>"), "shown");
        assert_eq!(html_text("<script>var s=\"<script>\";</script><p>shown</p>"), "shown");
        assert_eq!(html_text("<script src=\"a.js\"/>var secret=1;</script><p>shown</p>"), "shown");
        assert_eq!(html_text("<STYLE>p>a{}</STYLE><p>shown</p>"), "shown");
        // `not-sr-only` is visible; a stray apostrophe outside a value does not hold the tag open
        assert_eq!(html_text("<span class=\"not-sr-only\">visible</span>"), "visible");
        assert_eq!(html_text("<a title='it's'>x</a><script>if(a>b){}</script><p>shown</p>"), "x\nshown");
        // the closing tag needs a delimiter after its name; noscript is raw text too
        assert_eq!(html_text("<script>var t=\"</scripts>\"; LEAKED_CODE</script><p>shown</p>"), "shown");
        assert_eq!(html_text("<style>a::after{content:\"</styled>\"} LEAKED_CSS{}</style><p>shown</p>"), "shown");
        assert_eq!(html_text("<noscript>a<b</noscript><p>shown</p>"), "shown");
        // numeric entities: no NUL, no sign
        assert_eq!(html_text("<p>a&#0;b &#+65; &#x+41; &#65;</p>"), "a&#0;b &#+65; &#x+41; A");
        // malformed input never panics
        for t in ["<", "<>", "</", "<p", "<p class=\"x", "héllo 日本<b>x", "&", "&#99999999999;"] {
            let _ = html_text(t);
        }
    }

    #[test]
    fn claude_timeline_other_tools_are_not_answer_text() {
        // a web search block carries its query in field 13: that is the tool's input, never part of the answer
        let payload = pb_len(7, &cat(&[pb_str(1, "orari treni Roma Milano domani mattina")]));
        let b = pb_len(5, &cat(&[pb_str(1, "b-s"), pb_str(2, "g-s"), pb_num(4, 1), pb_len(13, &payload), pb_str(15, "web_search")]));
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(
            &mut r,
            &[
                tl_frame(&[conv(2)], None),
                tl_frame(&[conv(2), message("a1", 2), group("g-s", "a1", 2), b], None),
                tl_frame(&[group("g-text", "a1", 1), block_final("b-text", "g-text", "Ecco gli orari.")], None),
                tl_frame(&[conv(1)], None),
            ],
        );
        assert_eq!(r.answer().text, "Ecco gli orari.");
    }

    /// The `visualize:show_widget` block as the timeline sends it (measured 18/10/2026, invented data): field 15 is
    /// only a display name, `13.26` holds the call. While it is written the input is in `13.26.13` (cut mid-markup),
    /// once complete in `13.26.2`.
    fn visualize_widget(id: &str, grp: &str, input_field: u32, input: &str) -> Vec<u8> {
        let call = cat(&[pb_str(1, "visualize:show_widget"), pb_str(5, "visualize"), pb_str(12, "show_widget"), pb_str(input_field, input)]);
        pb_len(5, &cat(&[pb_str(1, id), pb_str(2, grp), pb_len(13, &pb_len(26, &call)), pb_str(15, "visualize: Show widget")]))
    }

    #[test]
    fn claude_timeline_visualize_widget_text_enters_the_answer() {
        let code = "<h2 class=\"sr-only\">Card description</h2><style>.a{color:red}</style><div><p>A dog, in two sentences</p>\n<p>English</p><p>The dog runs.</p><div><p>Italiano</p><p>Il cane corre.</p></div></div><div><p>English</p><p>He wags &amp; waits.</p><p>Scodinzola e aspetta.</p></div>";
        let input = serde_json::json!({"title": "dog_card", "loading_messages": ["Preparing"], "widget_code": code}).to_string();
        let partial = &input[..input.len() - 40];
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(
            &mut r,
            &[
                tl_frame(&[conv(2)], None),
                // streaming snapshot (cut input) first: it must add nothing
                tl_frame(&[conv(2), message("a1", 2), group("g-w", "a1", 2), visualize_widget("b-w", "g-w", 13, partial)], None),
                tl_frame(&[visualize_widget("b-w", "g-w", 2, &input)], None),
                tl_frame(&[group("g-text", "a1", 1), block_final("b-text", "g-text", "Ecco la scheda.")], None),
                tl_frame(&[conv(1)], None),
            ],
        );
        assert_eq!(
            r.answer().text,
            "A dog, in two sentences\nEnglish\nThe dog runs.\nItaliano\nIl cane corre.\nEnglish\nHe wags & waits.\nScodinzola e aspetta.\n\nEcco la scheda."
        );
        assert!(r.answer().done);
    }

    #[test]
    fn claude_timeline_other_server_call_with_html_input_is_not_answer_text() {
        // same shape as show_widget but another server (a web search, say): its input is never part of the answer
        let call = cat(&[pb_str(1, "web_search"), pb_str(5, "web"), pb_str(2, r#"{"query":"orari treni","widget_code":"<p>no</p>"}"#)]);
        let b = pb_len(5, &cat(&[pb_str(1, "b-s"), pb_str(2, "g-s"), pb_len(13, &pb_len(26, &call)), pb_str(15, "web_search")]));
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(
            &mut r,
            &[
                tl_frame(&[conv(2)], None),
                tl_frame(&[conv(2), message("a1", 2), group("g-s", "a1", 2), b], None),
                tl_frame(&[group("g-text", "a1", 1), block_final("b-text", "g-text", "Ecco gli orari.")], None),
                tl_frame(&[conv(1)], None),
            ],
        );
        assert_eq!(r.answer().text, "Ecco gli orari.");
    }

    #[test]
    fn claude_timeline_unknown_widget_gives_plain_text_but_never_markup() {
        let payload = pb_len(7, &cat(&[pb_str(1, "A short note for you"), pb_str(2, "<div class=\"x\">html source</div>"), pb_str(3, "single")]));
        let b = pb_len(5, &cat(&[pb_str(1, "b-w"), pb_str(2, "g-w"), pb_num(4, 1), pb_len(13, &payload), pb_str(15, "mcp__widgets__other_v0")]));
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(
            &mut r,
            &[
                tl_frame(&[conv(2)], None),
                tl_frame(&[conv(2), message("a1", 2), group("g-w", "a1", 2), b], None),
                tl_frame(&[group("g-text", "a1", 1), block_final("b-text", "g-text", "Fine.")], None),
                tl_frame(&[conv(1)], None),
            ],
        );
        assert_eq!(r.answer().text, "A short note for you\n\nFine.");
    }

    /// A block snapshot flagged finished (field 4 = 1), as the last snapshot of a block is.
    fn block_final(id: &str, grp: &str, text: &str) -> Vec<u8> {
        let mut b = cat(&[pb_str(1, id), pb_str(2, grp), pb_num(4, 1), pb_len(10, &pb_num(1, 2))]);
        b.extend(pb_str(9, text));
        pb_len(5, &b)
    }

    #[test]
    fn claude_timeline_joined_mid_answer() {
        // 14/10/2026: the page reconnects the stream while Claude writes. The new one starts with a delta, never
        // sees the conversation turn busy, and ends with the block's final snapshot and the idle state.
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(&mut r, &[tl_frame(&[], Some(delta("b", "fine del saggio.")))]);
        assert_eq!(r.working(), Some(true));
        assert!(!r.answer().done);
        feed_frames(&mut r, &[tl_frame(&[block_final("b", "g", "Tutto il saggio: fine del saggio.")], None), tl_frame(&[conv(1)], None)]);
        assert_eq!(r.working(), Some(false));
        assert_eq!(r.answer().text, "Tutto il saggio: fine del saggio.");
        assert!(r.answer().done);
    }

    #[test]
    fn claude_timeline_tail_only_is_not_a_whole_answer() {
        // The same stream without the final snapshot would hand out the last words as if they were the answer.
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(&mut r, &[tl_frame(&[], Some(delta("b", "fine del saggio."))), tl_frame(&[conv(1)], None)]);
        assert_eq!(r.answer().text, "fine del saggio.");
        assert!(!r.answer().done);
    }

    #[test]
    fn claude_timeline_closed_stream_says_nothing() {
        // A stream that closed mid-answer must not keep the page waiting for an end that will arrive on another one.
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(&mut r, &[tl_frame(&[conv(2)], None)]);
        assert_eq!(r.working(), Some(true));
        r.end();
        assert_eq!(r.working(), None);
        // ... unless it had already stopped: then the close changes nothing
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        feed_frames(&mut r, &[tl_frame(&[conv(2)], None), tl_frame(&[conv(1)], None)]);
        r.end();
        assert_eq!(r.working(), Some(false));
    }

    // Captured 14/10/2026 (ids only): a group and the text block opening, a delta, a later snapshot.
    const REAL_OPEN: &str = "0ab50152110a0f313739313936303833313638302d3058010a9d0122470a1d646772705f303030303030303030303030303030303030303030303033122430303030303030302d303030302d343030302d383030302d30303030303030303030303020012a520a1d63626c6b5f303030303030303030303030303030303030303030303031121d646772705f3030303030303030303030303030303030303030303030324a0e2a2a526973706f7374613a20736952020802";
    const REAL_DELTA: &str = "0a4b52110a0f313739313936303833313731372d30580112340a1d63626c6b5f303030303030303030303030303030303030303030303031121320696e63726f6369616e6f20616c6c65203130";
    const REAL_SNAPSHOT: &str = "0ae40152110a0f313739313936303833313937322d3158010acc012ac9010a1d63626c6b5f303030303030303030303030303030303030303030303031121d646772705f3030303030303030303030303030303030303030303030324a81012a2a526973706f7374613a20736920696e63726f6369616e6f20616c6c652031303a35323a34352063697263612c2061203330382c3235206b6d20646120526f6d612e2a2a0a0a2a2a50617373616767692a2a0a0a312e202a2a56616e74616767696f2064656c207472656e6f20646120526f6d612e2a2a20506172746520333052020802c00103";

    #[test]
    fn claude_timeline_captured_frames() {
        let mut r = reader_for("anthropic", TIMELINE_URL).unwrap();
        r.feed(&b64_line(&connect(0, &tl_frame(&[conv(2)], None))));
        r.feed(&b64_line(&connect(0, &hex(REAL_OPEN))));
        assert_eq!(r.answer().text, "**Risposta: si");
        r.feed(&b64_line(&connect(0, &hex(REAL_DELTA))));
        assert_eq!(r.answer().text, "**Risposta: si incrociano alle 10");
        r.feed(&b64_line(&connect(0, &hex(REAL_SNAPSHOT))));
        assert!(r.answer().text.starts_with("**Risposta: si incrociano alle 10:52:45 circa"));
        assert!(!r.answer().done);
        r.feed(&b64_line(&connect(0, &tl_frame(&[conv(1)], None))));
        assert!(r.answer().done);
        assert_eq!(r.answer().skipped, 0);
    }
}
