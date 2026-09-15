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
}

/// The answer endpoint of a provider, as a JavaScript regular expression source: the page observer forwards
/// only requests whose URL matches it. Kept next to `reader_for`, which recognises the same URLs.
pub fn url_pattern(key: &str) -> Option<&'static str> {
    Some(match key {
        "anthropic" => "/completion(\\?|$)",
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
        "anthropic" if url.contains("/completion") => Box::new(Claude::default()),
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
            self.ans.text = best;
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
}
impl Reader for Grok {
    fn feed(&mut self, chunk: &str) {
        for o in self.objs.push(chunk) {
            let Ok(v) = serde_json::from_str::<Value>(&o) else { self.ans.skipped += 1; continue };
            let ev = &v["event"];
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

    #[test]
    fn gemini_ui_components_removed() {
        let t = "Fine.\n\n<ElicitationsGroup message=\"Vuoi?\">\n<Elicitation label=\"A\" query=\"B\"/>\n</ElicitationsGroup>";
        assert_eq!(strip_ui_components(t), "Fine.");
        assert_eq!(strip_ui_components("a <b>x</b> 2 < 3"), "a <b>x</b> 2 < 3");
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
    fn captured_streams() {
        let cases = [
            ("anthropic", "/api/o/chat_conversations/c/completion", "anthropic-run3.txt"),
            ("deepseek", "/api/v0/chat/completion", "deepseek-run3.txt"),
            ("gemini", "/StreamGenerate", "gemini-run3.txt"),
            ("mistral", "/api/chat", "mistral-run3.txt"),
            ("perplexity", "/rest/sse/perplexity_ask", "perplexity-run3.txt"),
            ("zai", "/api/v2/chat/completions", "zai-run3.txt"),
            ("qwen", "/api/v2/chat/completions", "qwen-run3.txt"),
            ("grok", "wss://grok.com/ws/mgw/", "grok-run3.txt"),
        ];
        for (k, u, f) in cases {
            if let Some(a) = replay(k, u, f) {
                eprintln!("--- {k}: done={} skipped={} text={}B reasoning={}B sources={}\n{}", a.done, a.skipped, a.text.len(), a.reasoning.len(), a.sources.len(), a.text.chars().take(400).collect::<String>());
            }
        }
    }
}
