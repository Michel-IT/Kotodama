//! Provider icons: the real favicon of each provider, fetched from the provider's own site, cached on disk in the
//! app data folder and handed to the interface as a data URL. A cached icon is re-checked in the background about
//! once a month; a provider that offered no icon is not asked again for a week.
//!
//! Rules:
//! - the icon is shown as the provider publishes it: never recoloured, filtered or redrawn;
//! - nothing is shipped in the repo or in the binary: the bytes come from the provider's site at run time;
//! - the interface falls back to a disc with the provider's initials when there is no icon (offline, option off,
//!   invalid file), so a missing icon never breaks the layout.
//!
//! Network safety: HTTPS only, the provider's own domain (plus a short list of its known CDN domains), at most
//! 3 redirects (each one checked against the same list), no IP literals, no cookies, no credentials, 5 s per icon
//! request (8 s for the home page, of which at most 256 KB are read), 64 KB cap per icon, type checked from the
//! content. A custom DNS resolver drops private, loopback, link-local and metadata addresses, so a name that
//! resolves to an internal host (DNS rebinding) is refused at connection time, redirects included.
//! Limit: if `HTTPS_PROXY` / `ALL_PROXY` (or the lowercase forms) are set in the environment, reqwest connects
//! to the proxy and the proxy resolves the name, so the DNS filter does not apply; the HTTPS-only and
//! domain-allowlist checks still do. A proxy on localhost simply means no icons arrive (the initials stay): accepted.
//! Declared image size is capped (`MAX_ICON_SIDE`) as well as the byte size.
//! An SVG is only ever shown through `<img>` with a data URL (the browser runs no script and loads no external
//! resource from an image in that mode); the check in `svg_is_inert` is a second barrier, not the first.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, Manager};
use url::{Host, Url};

const MAX_ICON_BYTES: usize = 64 * 1024;
/// Largest declared width/height, in pixels, for PNG, ICO and WebP (a favicon is at most a few hundred).
const MAX_ICON_SIDE: u32 = 512;
const MAX_HTML_BYTES: usize = 256 * 1024;
const MAX_REDIRECTS: usize = 3;
const MAX_LINK_CANDIDATES: usize = 4;
const TIMEOUT: Duration = Duration::from_secs(5);
/// The home page is allowed longer (it is the heavy request); the number of requests does not grow.
const HOME_TIMEOUT: Duration = Duration::from_secs(8);
/// A cached icon older than this is still shown at once and re-checked in the background.
const FRESH_FOR: Duration = Duration::from_secs(30 * 24 * 3600);
/// After a network failure the provider is not tried again for this long (no retry loop).
const NET_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// After a site answered but offered no usable icon, not tried again for this long.
/// A week, because some sites (DeepSeek, Mistral) answer 403 from their CDN to any non-browser client and would
/// otherwise be asked again every day for nothing.
const MISS_FOR: Duration = Duration::from_secs(7 * 24 * 3600);
const USER_AGENT: &str = concat!("Kotodama/", env!("CARGO_PKG_VERSION"), " (desktop app; provider icon fetch)");

/// A provider known to the app: its home page and the extra domains its icon may legitimately come from.
struct Site {
    key: &'static str,
    home: &'static str,
    extra_roots: &'static [&'static str],
}

const SITES: &[Site] = &[
    Site { key: "openai", home: "https://chatgpt.com/", extra_roots: &["openai.com", "oaistatic.com"] },
    Site { key: "anthropic", home: "https://claude.ai/", extra_roots: &["anthropic.com"] },
    Site { key: "grok", home: "https://grok.com/", extra_roots: &["x.ai"] },
    Site { key: "gemini", home: "https://gemini.google.com/", extra_roots: &["gstatic.com", "gemini.google"] },
    Site { key: "perplexity", home: "https://www.perplexity.ai/", extra_roots: &[] },
    Site { key: "qwen", home: "https://chat.qwen.ai/", extra_roots: &["alicdn.com", "qwenlm.ai"] },
    Site { key: "deepseek", home: "https://chat.deepseek.com/", extra_roots: &[] },
    Site { key: "zai", home: "https://chat.z.ai/", extra_roots: &["zhipuai.cn", "bigmodel.cn", "chatglm.cn"] },
    Site { key: "mistral", home: "https://chat.mistral.ai/", extra_roots: &[] },
    Site { key: "poe", home: "https://poe.com/", extra_roots: &["poecdn.net", "quora.com"] },
    Site { key: "kimi", home: "https://www.kimi.com/", extra_roots: &["moonshot.cn", "moonshot.ai"] },
    Site { key: "meta", home: "https://www.meta.ai/", extra_roots: &["meta.com", "facebook.com", "fbcdn.net"] },
    Site { key: "copilot", home: "https://copilot.microsoft.com/", extra_roots: &["bing.com", "office.net"] },
];

fn site_for(key: &str) -> Option<&'static Site> {
    SITES.iter().find(|s| s.key == key)
}

// ---------------------------------------------------------------------------------------------------------------
// Pure parts (unit tested)
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Png,
    Ico,
    Webp,
    Svg,
}

impl Kind {
    fn mime(self) -> &'static str {
        match self {
            Kind::Png => "image/png",
            Kind::Ico => "image/x-icon",
            Kind::Webp => "image/webp",
            Kind::Svg => "image/svg+xml",
        }
    }
}

/// The type of an image from its CONTENT (never from the Content-Type header or the file name).
fn sniff_kind(b: &[u8]) -> Option<Kind> {
    if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(Kind::Png);
    }
    if b.len() > 6 && b.starts_with(&[0, 0, 1, 0]) && b[4] > 0 {
        return Some(Kind::Ico);
    }
    if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Some(Kind::Webp);
    }
    if svg_root_first(b) {
        return Some(Kind::Svg);
    }
    None
}

/// True when the FIRST element of the text is `<svg`: only a BOM, whitespace, comments, an XML declaration and a
/// `<!DOCTYPE svg ...>` may precede it. An HTML page that merely starts with a comment and contains an `<svg`
/// somewhere is not an SVG file.
fn svg_root_first(b: &[u8]) -> bool {
    let head = &b[..b.len().min(8192)];
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    let mut t: &str = text.trim_start_matches('\u{feff}');
    loop {
        t = t.trim_start();
        if let Some(r) = t.strip_prefix("<!--") {
            match r.find("-->") {
                Some(e) => t = &r[e + 3..],
                None => return false,
            }
        } else if let Some(r) = t.strip_prefix("<?xml") {
            match r.find("?>") {
                Some(e) => t = &r[e + 2..],
                None => return false,
            }
        } else if t.starts_with("<!doctype svg") {
            match t.find('>') {
                Some(e) => t = &t[e + 1..],
                None => return false,
            }
        } else {
            break;
        }
    }
    match t.strip_prefix("<svg") {
        Some(r) => matches!(r.chars().next(), Some(c) if c.is_ascii_whitespace() || c == '>' || c == '/'),
        None => false,
    }
}

/// True when an SVG shows none of the usual ways to run code or reach out: script, foreign content, `<use>` and
/// `<image>`, event attributes, links that point outside the file, `url(...)` that is not a local fragment, entity
/// definitions and external DOCTYPEs. The SVG is displayed only through `<img>` with a data URL, where a browser
/// runs nothing and fetches nothing: this check is a second barrier for a file that ends up somewhere else, not the
/// first one, and it does not claim to catch every possible construct (it errs on the side of refusing).
fn svg_is_inert(b: &[u8]) -> bool {
    let s = String::from_utf8_lossy(b).to_ascii_lowercase();
    for bad in [
        "<script", "<foreignobject", "<!entity", "<iframe", "<embed", "<object", "<use", "<image", "<animate",
        "<set ", "<handler", "<listener", "@import", "expression(",
    ] {
        if s.contains(bad) {
            return false;
        }
    }
    // Scheme names hidden by whitespace or control characters.
    let compact: String = s.chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect();
    for bad in ["javascript:", "vbscript:", "data:text/html", "data:image/svg", "data:application"] {
        if compact.contains(bad) {
            return false;
        }
    }
    if !entities_are_plain(&s) || !doctype_is_plain(&s) || !urls_are_local(&s) || !hrefs_are_local(&s) {
        return false;
    }
    !has_event_attribute(s.as_bytes())
}

/// Only the five predefined XML entities; no numeric or named character references (they can spell out a scheme).
fn entities_are_plain(s: &str) -> bool {
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        rest = &rest[p + 1..];
        if !["amp;", "lt;", "gt;", "quot;", "apos;"].iter().any(|e| rest.starts_with(e)) {
            return false;
        }
    }
    true
}

/// A DOCTYPE may name the SVG type but must not carry an internal subset or an external identifier.
fn doctype_is_plain(s: &str) -> bool {
    match s.find("<!doctype") {
        None => true,
        Some(p) => {
            let end = s[p..].find('>').map(|e| p + e).unwrap_or(s.len());
            let d = &s[p..end];
            !d.contains('[') && !d.contains("system") && !d.contains("public")
        }
    }
}

/// Every `url(` must point at a fragment of the same file (`url(#gradient)`).
fn urls_are_local(s: &str) -> bool {
    let mut rest = s;
    while let Some(p) = rest.find("url(") {
        rest = &rest[p + 4..];
        if !rest.trim_start_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'').starts_with('#') {
            return false;
        }
    }
    true
}

/// Every `href` / `xlink:href` value must be a fragment of the same file.
fn hrefs_are_local(s: &str) -> bool {
    let mut rest = s;
    while let Some(p) = rest.find("href") {
        rest = &rest[p + 4..];
        let Some(after) = rest.trim_start().strip_prefix('=') else { continue };
        if !after.trim_start_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'').starts_with('#') {
            return false;
        }
    }
    true
}

/// An attribute named `on<letters>` after whitespace, `/` or a quote, with spaces, tabs or newlines allowed before `=`.
fn has_event_attribute(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < bytes.len() {
        if (bytes[i].is_ascii_whitespace() || matches!(bytes[i], b'/' | b'"' | b'\'')) && bytes[i + 1] == b'o' && bytes[i + 2] == b'n' {
            let mut j = i + 3;
            while j < bytes.len() && bytes[j].is_ascii_lowercase() {
                j += 1;
            }
            let letters = j > i + 3;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if letters && j < bytes.len() && bytes[j] == b'=' {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Accepts a downloaded or cached file only if it is a small, supported, inert image.
fn validate_icon(b: &[u8]) -> Option<Kind> {
    if b.is_empty() || b.len() > MAX_ICON_BYTES {
        return None;
    }
    let k = sniff_kind(b)?;
    if k == Kind::Svg && !svg_is_inert(b) {
        return None;
    }
    if let Some((w, h)) = declared_size(b, k) {
        if w > MAX_ICON_SIDE || h > MAX_ICON_SIDE {
            return None;
        }
    }
    Some(k)
}

/// Width and height declared in the header of a PNG (IHDR), ICO (largest directory entry) or WebP (VP8, VP8L,
/// VP8X). `None` when the header is too short or not of a known layout: the byte cap still applies then.
fn declared_size(b: &[u8], k: Kind) -> Option<(u32, u32)> {
    let le16 = |i: usize| b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as u32);
    let le24 = |i: usize| b.get(i..i + 3).map(|s| s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16);
    match k {
        Kind::Png => {
            let w = b.get(16..20)?;
            let h = b.get(20..24)?;
            Some((u32::from_be_bytes([w[0], w[1], w[2], w[3]]), u32::from_be_bytes([h[0], h[1], h[2], h[3]])))
        }
        Kind::Ico => {
            let count = le16(4)? as usize;
            let (mut mw, mut mh) = (0u32, 0u32);
            for i in 0..count.min(64) {
                let e = 6 + i * 16;
                let (w, h) = (*b.get(e)? as u32, *b.get(e + 1)? as u32);
                // 0 means 256 in an ICO directory.
                mw = mw.max(if w == 0 { 256 } else { w });
                mh = mh.max(if h == 0 { 256 } else { h });
            }
            if count == 0 { None } else { Some((mw, mh)) }
        }
        Kind::Webp => match b.get(12..16)? {
            // frame header: 3 bytes tag, 3 bytes start code, then 14-bit width and height
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            b"VP8L" => {
                let v = b.get(21..25)?;
                let bits = u32::from_le_bytes([v[0], v[1], v[2], v[3]]);
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            _ => None,
        },
        _ => None,
    }
}

/// "chat.qwen.ai" -> "qwen.ai"; "claude.ai" -> "claude.ai".
fn root_domain(host: &str) -> String {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = h.split('.').collect();
    if labels.len() > 2 {
        labels[labels.len() - 2..].join(".")
    } else {
        h
    }
}

/// The domains a provider's icon may come from.
#[derive(Debug, Clone)]
struct Allow {
    roots: Vec<String>,
}

impl Allow {
    fn for_site(s: &Site) -> Allow {
        let mut roots = Vec::new();
        if let Ok(u) = Url::parse(s.home) {
            if let Some(h) = u.host_str() {
                roots.push(root_domain(h));
            }
        }
        for r in s.extra_roots {
            roots.push(r.to_ascii_lowercase());
        }
        Allow { roots }
    }

    /// HTTPS, default port, no credentials, a real domain name (never an IP literal, never a local name), and that
    /// domain is the provider's own (or a subdomain of it) or one of its listed CDN domains.
    fn url_ok(&self, u: &Url) -> bool {
        if u.scheme() != "https" || !u.username().is_empty() || u.password().is_some() {
            return false;
        }
        if let Some(p) = u.port() {
            if p != 443 {
                return false;
            }
        }
        let host = match u.host() {
            Some(Host::Domain(d)) => d.trim_end_matches('.').to_ascii_lowercase(),
            _ => return false,
        };
        if is_local_name(&host) {
            return false;
        }
        self.roots.iter().any(|r| host == *r || host.ends_with(&format!(".{r}")))
    }
}

fn is_local_name(host: &str) -> bool {
    !host.contains('.')
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".lan")
        || host.ends_with(".home.arpa")
        || host.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// The conventional location, tried first.
fn favicon_ico_url(home: &Url) -> Option<Url> {
    home.join("/favicon.ico").ok()
}

/// Icon URLs declared by `<link rel="icon" ...>` (and `apple-touch-icon`) in a home page, resolved against the
/// page URL, kept only if the allow-list accepts them. Plain `icon` links first, `apple-touch-icon` after
/// (those are large). `mask-icon` and friends are ignored.
fn links_from_html(html: &str, page: &Url, allow: &Allow) -> Vec<Url> {
    let lower = html.to_ascii_lowercase();
    let mut plain: Vec<Url> = Vec::new();
    let mut touch: Vec<Url> = Vec::new();
    let mut from = 0;
    while let Some(p) = lower[from..].find("<link") {
        let start = from + p;
        let end = match lower[start..].find('>') {
            Some(e) => start + e,
            None => break,
        };
        let tag = &html[start..end];
        from = end;
        let rel = attr_value(tag, "rel").unwrap_or_default().to_ascii_lowercase();
        let Some(href) = attr_value(tag, "href") else { continue };
        let tokens: Vec<&str> = rel.split_whitespace().collect();
        let is_touch = tokens.iter().any(|t| *t == "apple-touch-icon" || *t == "apple-touch-icon-precomposed");
        let is_icon = tokens.iter().any(|t| *t == "icon");
        if !is_touch && !is_icon {
            continue;
        }
        let Ok(u) = page.join(href.trim()) else { continue };
        if !allow.url_ok(&u) {
            continue;
        }
        let bucket = if is_touch && !is_icon { &mut touch } else { &mut plain };
        if !bucket.contains(&u) {
            bucket.push(u);
        }
    }
    plain.extend(touch);
    plain.truncate(MAX_LINK_CANDIDATES);
    plain
}

/// Value of an attribute inside one tag's text (quoted with " or ', or bare).
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find(name) {
        let at = from + p;
        from = at + name.len();
        let before_ok = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace() || lower.as_bytes()[at - 1] == b'"' || lower.as_bytes()[at - 1] == b'\'';
        if !before_ok {
            continue;
        }
        let rest = tag[at + name.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let mut chars = rest.chars();
        return match chars.next() {
            Some(q @ ('"' | '\'')) => {
                let body = &rest[1..];
                body.find(q).map(|e| body[..e].to_string())
            }
            Some(_) => Some(rest.split(|c: char| c.is_ascii_whitespace()).next().unwrap_or("").to_string()),
            None => None,
        };
    }
    None
}

// ---------------------------------------------------------------------------------------------------------------
// Cache on disk
// ---------------------------------------------------------------------------------------------------------------

fn cache_dir(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("provider-icons"))
}

fn age_of(path: &Path) -> Option<Duration> {
    std::fs::metadata(path).ok()?.modified().ok()?.elapsed().ok()
}

/// Bytes + whether the cache entry is still fresh. A file that no longer validates is treated as absent.
fn read_cache(dir: &Path, key: &str) -> Option<(Vec<u8>, Kind, bool)> {
    let path = dir.join(format!("{key}.icon"));
    // Look at the size first: a file that grew (or was replaced) must not be read whole into memory.
    if std::fs::metadata(&path).ok()?.len() > (MAX_ICON_BYTES as u64) + 1024 {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let kind = validate_icon(&bytes)?;
    let fresh = age_of(&path).map(|a| a < FRESH_FOR).unwrap_or(false);
    Some((bytes, kind, fresh))
}

/// Same idea as `settings::write_atomic`, for bytes: write beside the target and rename over it, with a few short
/// retries (an antivirus or an indexer can hold the file for a moment).
fn write_atomic_bytes(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    let mut last = String::new();
    for attempt in 0..4 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        if attempt < 3 {
            std::thread::sleep(Duration::from_millis(60));
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(last)
}

fn to_data_url(bytes: &[u8], kind: Kind) -> String {
    format!("data:{};base64,{}", kind.mime(), base64::engine::general_purpose::STANDARD.encode(bytes))
}

// ---------------------------------------------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------------------------------------------

enum Outcome {
    Icon(Vec<u8>),
    /// The site answered but offered no usable icon.
    NotFound,
    /// Offline, DNS, timeout, TLS: try again later (backoff).
    NetworkError,
}

fn ensure_crypto_provider() {
    // reqwest is built with `rustls-no-provider` (same as the updater): a process-wide provider must exist.
    // Installing twice is harmless, hence the ignored result.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// False for every address that must never be contacted on behalf of a provider's icon: loopback, private,
/// link-local (which includes the cloud metadata address 169.254.169.254), carrier-grade NAT, unspecified,
/// multicast, broadcast, documentation, unique-local and the IPv6 forms that embed an IPv4 address (mapped,
/// compatible, NAT64, 6to4), judged by the embedded address.
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => is_public_v4(v),
        IpAddr::V6(v) => {
            if let Some(m) = embedded_v4(v) {
                return is_public_v4(m);
            }
            let s = v.segments();
            !(v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (s[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (s[0] & 0xffc0) == 0xfec0 // deprecated site-local
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || v == Ipv6Addr::LOCALHOST)
        }
    }
}

/// The IPv4 address hidden inside an IPv6 one, for the forms that carry one: IPv4-mapped `::ffff:a.b.c.d`,
/// IPv4-compatible `::a.b.c.d`, NAT64 `64:ff9b::/96` and 6to4 `2002::/16`. The same rule as for plain IPv4 then applies.
fn embedded_v4(v: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(m) = v.to_ipv4_mapped() {
        return Some(m);
    }
    let s = v.segments();
    let o = v.octets();
    if s[..6].iter().all(|&x| x == 0) && !(s[6] == 0 && s[7] <= 1) {
        return Some(Ipv4Addr::new(o[12], o[13], o[14], o[15])); // ::a.b.c.d ("::" and "::1" are handled apart)
    }
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6].iter().all(|&x| x == 0) {
        return Some(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    if s[0] == 0x2002 {
        return Some(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    None
}

fn is_public_v4(v: Ipv4Addr) -> bool {
    let o = v.octets();
    !(v.is_loopback()
        || v.is_private()
        || v.is_link_local()
        || v.is_unspecified()
        || v.is_broadcast()
        || v.is_multicast()
        || v.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // carrier-grade NAT 100.64.0.0/10
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking 198.18.0.0/15
        || o[0] >= 240)
}

/// Keeps only public addresses (see `is_public_ip`); an empty result is an error, so the connection is refused.
fn keep_public(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    addrs.into_iter().filter(|a| is_public_ip(a.ip())).collect()
}

/// DNS resolver that drops internal addresses: a provider name that (re)points to a private host (DNS rebinding)
/// cannot be connected to, for the first request and for every redirect.
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found = tauri::async_runtime::spawn_blocking(move || {
                (host.as_str(), 443u16).to_socket_addrs().map(|i| i.collect::<Vec<SocketAddr>>())
            })
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            let ok = keep_public(found);
            if ok.is_empty() {
                let e: Box<dyn std::error::Error + Send + Sync> = "name resolves only to internal addresses".into();
                return Err(e);
            }
            Ok(Box::new(ok.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn build_client(allow: &Allow) -> Option<reqwest::Client> {
    ensure_crypto_provider();
    let allow = allow.clone();
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if allow.url_ok(attempt.url()) {
            attempt.follow()
        } else {
            attempt.error("redirect outside the provider's domains")
        }
    });
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT)
        .redirect(policy)
        .dns_resolver(PublicOnlyResolver)
        .build()
        .ok()
}

enum FetchErr {
    Network,
    Refused,
}

/// Appends `chunk` to `out` without ever exceeding `max`. Returns true when the limit was reached (the caller
/// stops reading). With `truncate` the part that fits is kept; without it the whole answer is refused later.
fn append_capped(out: &mut Vec<u8>, chunk: &[u8], max: usize) -> bool {
    let room = max.saturating_sub(out.len());
    if chunk.len() > room {
        out.extend_from_slice(&chunk[..room]);
        true
    } else {
        out.extend_from_slice(chunk);
        out.len() >= max
    }
}

/// GET with a size limit. `truncate = false` (icons): an answer larger than `max` is refused. `truncate = true`
/// (the home page): only the first `max` bytes are read, whatever `Content-Length` says, and the rest is dropped,
/// because the `<link rel="icon">` tags sit at the top of the document.
async fn fetch_limited(client: &reqwest::Client, url: &Url, max: usize, truncate: bool, timeout: Duration) -> Result<Vec<u8>, FetchErr> {
    let mut resp = client.get(url.clone()).timeout(timeout).send().await.map_err(|e| {
        if e.is_redirect() {
            FetchErr::Refused
        } else {
            FetchErr::Network
        }
    })?;
    if !resp.status().is_success() {
        return Err(FetchErr::Refused);
    }
    if !truncate && resp.content_length().map(|n| n as usize > max).unwrap_or(false) {
        return Err(FetchErr::Refused);
    }
    let mut out: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(c)) => {
                if !truncate && out.len() + c.len() > max {
                    return Err(FetchErr::Refused);
                }
                if append_capped(&mut out, &c, max) {
                    break;
                }
            }
            Ok(None) => break,
            // Truncated reading of a page: whatever arrived before the connection broke is still usable.
            Err(_) if truncate && !out.is_empty() => break,
            Err(_) => return Err(FetchErr::Network),
        }
    }
    Ok(out)
}

async fn download(site: &Site) -> Outcome {
    let allow = Allow::for_site(site);
    let Some(client) = build_client(&allow) else { return Outcome::NotFound };
    let Ok(home) = Url::parse(site.home) else { return Outcome::NotFound };
    let mut net_error = false;

    if let Some(u) = favicon_ico_url(&home) {
        match fetch_limited(&client, &u, MAX_ICON_BYTES, false, TIMEOUT).await {
            Ok(b) if validate_icon(&b).is_some() => return Outcome::Icon(b),
            Err(FetchErr::Network) => net_error = true,
            _ => {}
        }
    }
    if !net_error {
        match fetch_limited(&client, &home, MAX_HTML_BYTES, true, HOME_TIMEOUT).await {
            Ok(html) => {
                let html = String::from_utf8_lossy(&html).into_owned();
                for u in links_from_html(&html, &home, &allow) {
                    match fetch_limited(&client, &u, MAX_ICON_BYTES, false, TIMEOUT).await {
                        Ok(b) if validate_icon(&b).is_some() => return Outcome::Icon(b),
                        Err(FetchErr::Network) => {
                            net_error = true;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(FetchErr::Network) => net_error = true,
            Err(FetchErr::Refused) => {}
        }
    }
    if net_error {
        Outcome::NetworkError
    } else {
        Outcome::NotFound
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ProviderIcon {
    /// `data:image/...;base64,...` ready for an <img src>, or None.
    pub data_url: Option<String>,
    /// True when the interface must draw the disc with the provider's initials.
    pub fallback: bool,
}

impl ProviderIcon {
    fn fallback() -> Self {
        ProviderIcon { data_url: None, fallback: true }
    }
    fn from(bytes: &[u8], kind: Kind) -> Self {
        ProviderIcon { data_url: Some(to_data_url(bytes, kind)), fallback: false }
    }
}

fn inflight() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

fn backoff_map() -> &'static Mutex<HashMap<String, Instant>> {
    static S: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// An expired cache entry is re-checked in the background only if it is not in the in-process backoff and no
/// attempt (`.miss` marker) was made within the last day.
fn should_background_refresh(fresh: bool, backoff: bool, miss_age: Option<Duration>) -> bool {
    !fresh && !backoff && miss_age.map(|a| a >= MISS_FOR).unwrap_or(true)
}

fn in_backoff(key: &str) -> bool {
    backoff_map().lock().map(|m| m.get(key).map(|t| t.elapsed() < NET_BACKOFF).unwrap_or(false)).unwrap_or(false)
}

/// Downloads and caches one provider's icon. Returns the validated bytes on success. Never panics, never loops:
/// a network failure arms the backoff, a "nothing usable" answer leaves a `.miss` marker for a week (`MISS_FOR`).
async fn refresh(dir: PathBuf, site: &'static Site) -> Option<(Vec<u8>, Kind)> {
    {
        let mut f = inflight().lock().ok()?;
        if !f.insert(site.key.to_string()) {
            return None;
        }
    }
    let outcome = download(site).await;
    let result = match outcome {
        Outcome::Icon(bytes) => {
            let kind = validate_icon(&bytes);
            if let Some(kind) = kind {
                let _ = std::fs::create_dir_all(&dir);
                if write_atomic_bytes(&dir.join(format!("{}.icon", site.key)), &bytes).is_ok() {
                    let _ = std::fs::remove_file(dir.join(format!("{}.miss", site.key)));
                }
                Some((bytes, kind))
            } else {
                None
            }
        }
        Outcome::NotFound => {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(format!("{}.miss", site.key)), b"");
            None
        }
        Outcome::NetworkError => {
            if let Ok(mut m) = backoff_map().lock() {
                m.insert(site.key.to_string(), Instant::now());
            }
            // An expired icon that could not be re-checked: remember the attempt on disk too, so that the next
            // launches do not try again within the week (the backoff above only lives as long as the process).
            if dir.join(format!("{}.icon", site.key)).exists() {
                let _ = std::fs::write(dir.join(format!("{}.miss", site.key)), b"");
            }
            None
        }
    };
    if let Ok(mut f) = inflight().lock() {
        f.remove(site.key);
    }
    result
}

/// The favicon of a known provider as a data URL, from the cache when there is one, otherwise downloaded once
/// from the provider's site. Everything slow runs off the UI thread (async command); a stale entry is returned at
/// once and refreshed in the background.
#[tauri::command]
pub async fn provider_icon(app: AppHandle, key: String) -> ProviderIcon {
    let Some(site) = site_for(&key) else { return ProviderIcon::fallback() };
    // Nothing is fetched until the user has answered (welcome on a new install, one-time notice on an update).
    let enabled = app
        .state::<crate::AppState>()
        .settings
        .lock()
        .map(|s| s.prov_icons_decided && s.fetch_provider_icons)
        .unwrap_or(false);
    if !enabled {
        return ProviderIcon::fallback();
    }
    let Some(dir) = cache_dir(&app) else { return ProviderIcon::fallback() };

    if let Some((bytes, kind, fresh)) = read_cache(&dir, site.key) {
        let miss_age = age_of(&dir.join(format!("{}.miss", site.key)));
        if should_background_refresh(fresh, in_backoff(site.key), miss_age) {
            let d = dir.clone();
            tauri::async_runtime::spawn(async move {
                let _ = refresh(d, site).await;
            });
        }
        return ProviderIcon::from(&bytes, kind);
    }
    if in_backoff(site.key) {
        return ProviderIcon::fallback();
    }
    if let Some(a) = age_of(&dir.join(format!("{}.miss", site.key))) {
        if a < MISS_FOR {
            return ProviderIcon::fallback();
        }
    }
    // Someone else (another window) may already be downloading this one: wait for it instead of a second request.
    let busy = inflight().lock().map(|f| f.contains(site.key)).unwrap_or(false);
    if busy {
        for _ in 0..40 {
            tokio_sleep().await;
            if let Some((bytes, kind, _)) = read_cache(&dir, site.key) {
                return ProviderIcon::from(&bytes, kind);
            }
            if !inflight().lock().map(|f| f.contains(site.key)).unwrap_or(false) {
                break;
            }
        }
        return ProviderIcon::fallback();
    }
    match refresh(dir, site).await {
        Some((bytes, kind)) => ProviderIcon::from(&bytes, kind),
        None => ProviderIcon::fallback(),
    }
}

async fn tokio_sleep() {
    // `tauri::async_runtime::spawn_blocking` keeps this free of a direct tokio dependency.
    let _ = tauri::async_runtime::spawn_blocking(|| std::thread::sleep(Duration::from_millis(150))).await;
}

// ---------------------------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend_from_slice(&[0; 32]);
        v
    }

    #[test]
    fn sniffs_types_from_content() {
        assert_eq!(sniff_kind(&png()), Some(Kind::Png));
        assert_eq!(sniff_kind(&[0, 0, 1, 0, 2, 0, 16, 16, 0, 0, 1, 0, 32, 0]), Some(Kind::Ico));
        let mut webp = b"RIFF\x10\0\0\0WEBPVP8 ".to_vec();
        webp.extend_from_slice(&[0; 8]);
        assert_eq!(sniff_kind(&webp), Some(Kind::Webp));
        assert_eq!(sniff_kind(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>"), Some(Kind::Svg));
        assert_eq!(sniff_kind(b"\xef\xbb\xbf  <?xml version=\"1.0\"?><svg></svg>"), Some(Kind::Svg));
    }

    #[test]
    fn rejects_other_content() {
        assert_eq!(sniff_kind(b"<!DOCTYPE html><html><body>not found</body></html>"), None);
        assert_eq!(sniff_kind(b"GIF89a\x01\0\x01\0"), None);
        assert_eq!(sniff_kind(&[0xff, 0xd8, 0xff, 0xe0, 0, 0x10]), None);
        assert_eq!(sniff_kind(b""), None);
        // an .ico header that declares zero images is not an icon
        assert_eq!(sniff_kind(&[0, 0, 1, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn size_limit_is_enforced() {
        let mut big = png();
        big.resize(MAX_ICON_BYTES + 1, 0);
        assert!(validate_icon(&big).is_none());
        big.truncate(MAX_ICON_BYTES);
        assert_eq!(validate_icon(&big), Some(Kind::Png));
        assert!(validate_icon(&[]).is_none());
    }

    #[test]
    fn svg_with_active_content_is_rejected() {
        assert!(validate_icon(b"<svg><script>alert(1)</script></svg>").is_none());
        assert!(validate_icon(b"<svg onload=\"x()\"></svg>").is_none());
        assert!(validate_icon(b"<svg xmlns='http://www.w3.org/2000/svg'><a href=\"javascript:x()\"/></svg>").is_none());
        assert!(validate_icon(b"<svg><foreignObject><body/></foreignObject></svg>").is_none());
        assert!(validate_icon(b"<?xml version=\"1.0\"?><!DOCTYPE svg [<!ENTITY a \"b\">]><svg/>").is_none());
        // plain attributes that merely contain "on" are fine
        assert!(validate_icon(b"<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1 1\"><path d=\"M0 0\"/></svg>").is_some());
    }

    #[test]
    fn root_domain_is_normalised() {
        assert_eq!(root_domain("chat.qwen.ai"), "qwen.ai");
        assert_eq!(root_domain("WWW.Perplexity.AI."), "perplexity.ai");
        assert_eq!(root_domain("claude.ai"), "claude.ai");
        assert_eq!(root_domain("gemini.google.com"), "google.com");
    }

    fn allow(key: &str) -> Allow {
        Allow::for_site(site_for(key).unwrap())
    }

    #[test]
    fn url_policy_accepts_only_https_provider_domains() {
        let a = allow("anthropic");
        let ok = |s: &str| a.url_ok(&Url::parse(s).unwrap());
        assert!(ok("https://claude.ai/favicon.ico"));
        assert!(ok("https://a.cdn.claude.ai/x.png"));
        assert!(ok("https://www.anthropic.com/i.svg"));
        assert!(!ok("http://claude.ai/favicon.ico"));
        assert!(!ok("https://evilclaude.ai/favicon.ico"));
        assert!(!ok("https://claude.ai.evil.com/favicon.ico"));
        assert!(!ok("https://user:pw@claude.ai/favicon.ico"));
        assert!(!ok("https://claude.ai:8443/favicon.ico"));
        assert!(!ok("data:image/png;base64,AAAA"));
        assert!(!ok("file:///c:/x.png"));
    }

    #[test]
    fn url_policy_rejects_ip_literals_and_local_names() {
        let a = Allow { roots: vec!["127.0.0.1".into(), "localhost".into(), "example.local".into(), "10.0.0.1".into()] };
        for u in [
            "https://127.0.0.1/favicon.ico",
            "https://10.0.0.1/favicon.ico",
            "https://[::1]/favicon.ico",
            "https://localhost/favicon.ico",
            "https://printer.example.local/favicon.ico",
        ] {
            assert!(!a.url_ok(&Url::parse(u).unwrap()), "{u}");
        }
    }

    #[test]
    fn every_known_provider_has_an_https_home_inside_its_own_allow_list() {
        for s in SITES {
            let home = Url::parse(s.home).unwrap();
            assert!(Allow::for_site(s).url_ok(&home), "{}", s.key);
            assert_eq!(favicon_ico_url(&home).unwrap().path(), "/favicon.ico");
        }
        assert_eq!(SITES.len(), 13);
        assert!(site_for("kotodama").is_none());
        assert!(site_for("../x").is_none());
    }

    #[test]
    fn picks_icon_links_in_order_and_filters_foreign_hosts() {
        let a = allow("perplexity");
        let page = Url::parse("https://www.perplexity.ai/").unwrap();
        let html = r#"<html><head>
            <link rel="apple-touch-icon" href="/touch.png">
            <link rel="mask-icon" href="/mask.svg">
            <link rel="stylesheet" href="/a.css">
            <link rel='shortcut icon' href='//www.perplexity.ai/static/i.ico'>
            <link rel="icon" type="image/svg+xml" href=icons/logo.svg>
            <link rel="icon" href="https://tracker.example.com/i.png">
            <link rel="icon" href="http://www.perplexity.ai/plain.png">
            </head></html>"#;
        let got: Vec<String> = links_from_html(html, &page, &a).iter().map(|u| u.to_string()).collect();
        assert_eq!(
            got,
            vec![
                "https://www.perplexity.ai/static/i.ico",
                "https://www.perplexity.ai/icons/logo.svg",
                "https://www.perplexity.ai/touch.png",
            ]
        );
    }

    #[test]
    fn attr_value_handles_quotes() {
        assert_eq!(attr_value("<link rel=\"icon\" href='/a b.png'", "href").as_deref(), Some("/a b.png"));
        assert_eq!(attr_value("<link rel=icon href=/x.png", "href").as_deref(), Some("/x.png"));
        assert_eq!(attr_value("<link data-href=\"/no\" rel=\"icon\"", "href"), None);
    }

    #[test]
    fn html_is_not_taken_for_svg() {
        assert_eq!(sniff_kind(b"<!-- x --><html><body><svg></svg></body></html>"), None);
        assert_eq!(sniff_kind(b"<!doctype html><html><svg></svg></html>"), None);
        assert_eq!(sniff_kind(b"<svgfoo></svgfoo>"), None);
        assert_eq!(sniff_kind(b"<!-- made by a tool --><svg viewBox=\"0 0 1 1\"></svg>"), Some(Kind::Svg));
        assert_eq!(sniff_kind(b"<?xml version=\"1.0\"?>\n<!-- c -->\n<svg\n xmlns=\"http://www.w3.org/2000/svg\"/>"), Some(Kind::Svg));
        assert_eq!(sniff_kind(b"<?xml version=\"1.0\"?><html><svg/></html>"), None);
    }

    #[test]
    fn svg_hardening_covers_the_evasions() {
        let bad: &[&str] = &[
            "<SVG><SCRIPT>x()</SCRIPT></SVG>",
            "<svg/onload=x()></svg>",
            "<svg onload\t=\"x()\"></svg>",
            "<svg onload\n=\"x()\"></svg>",
            "<svg><a href=\"java&#115;cript:x()\"/></svg>",
            "<svg><a href=\"java\tscript:x()\"/></svg>",
            "<svg xmlns:xlink=\"http://www.w3.org/1999/xlink\"><image xlink:href=\"https://evil.example/a.png\"/></svg>",
            "<svg><a href=\"https://evil.example/\"/></svg>",
            "<svg><a xlink:href = 'https://evil.example/'/></svg>",
            "<svg><use href=\"https://evil.example/a.svg#x\"/></svg>",
            "<svg><style>a{background:url(https://evil.example/x.png)}</style></svg>",
            "<svg><rect style=\"fill:url( 'https://evil.example/x' )\"/></svg>",
            "<svg><style>@import 'https://evil.example/a.css';</style></svg>",
            "<?xml version=\"1.0\"?><!DOCTYPE svg SYSTEM \"http://evil.example/a.dtd\"><svg/>",
            "<!DOCTYPE svg [ <!ENTITY x SYSTEM \"file:///etc/passwd\"> ]><svg/>",
            "<svg><foreignObject><div/></foreignObject></svg>",
            "<svg><image href=\"data:image/png;base64,AAAA\"/></svg>",
        ];
        for s in bad {
            assert!(!svg_is_inert(s.as_bytes()), "{s}");
            assert!(validate_icon(s.as_bytes()).is_none(), "{s}");
        }
        let good: &[&str] = &[
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1 1\"><defs><linearGradient id=\"a\"/></defs><path fill=\"url(#a)\" d=\"M0 0\"/></svg>",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><path fill=\"url('#a')\" d=\"M0 0\"/><title>a &amp; b</title></svg>",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><linearGradient id=\"g\" xlink:href=\"#h\"/></svg>",
        ];
        for s in good {
            assert!(svg_is_inert(s.as_bytes()), "{s}");
        }
    }

    #[test]
    fn declared_image_size_is_capped() {
        let png_sized = |w: u32, h: u32| {
            let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
            v.extend_from_slice(&w.to_be_bytes());
            v.extend_from_slice(&h.to_be_bytes());
            v.extend_from_slice(&[8, 6, 0, 0, 0]);
            v
        };
        assert_eq!(validate_icon(&png_sized(32, 32)), Some(Kind::Png));
        assert_eq!(validate_icon(&png_sized(512, 512)), Some(Kind::Png));
        assert!(validate_icon(&png_sized(513, 16)).is_none());
        assert!(validate_icon(&png_sized(16, 60000)).is_none());
        assert!(validate_icon(&png_sized(100000, 100000)).is_none());
        // ICO: the largest entry counts; 0 means 256
        let ico = |w: u8, h: u8| {
            let mut v = vec![0, 0, 1, 0, 1, 0, w, h, 0, 0, 1, 0, 32, 0];
            v.extend_from_slice(&[0; 8]);
            v
        };
        assert_eq!(declared_size(&ico(0, 0), Kind::Ico), Some((256, 256)));
        assert_eq!(declared_size(&ico(16, 32), Kind::Ico), Some((16, 32)));
        // WebP lossless (VP8L): 14-bit width-1 / height-1
        let mut webp = b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0\x2f".to_vec();
        let bits: u32 = (1000 - 1) | ((20 - 1) << 14);
        webp.extend_from_slice(&bits.to_le_bytes());
        webp.extend_from_slice(&[0; 8]);
        assert_eq!(declared_size(&webp, Kind::Webp), Some((1000, 20)));
        assert!(validate_icon(&webp).is_none());
        // WebP extended (VP8X): 24-bit width-1 / height-1
        let mut x = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
        x.extend_from_slice(&[31, 0, 0, 15, 0, 0]);
        x.extend_from_slice(&[0; 8]);
        assert_eq!(declared_size(&x, Kind::Webp), Some((32, 16)));
        assert_eq!(validate_icon(&x), Some(Kind::Webp));
        // a truncated header is not a reason to refuse (the byte cap still applies)
        assert_eq!(declared_size(&[0x89, b'P'], Kind::Png), None);
    }

    #[test]
    fn miss_marker_lasts_a_week() {
        assert_eq!(MISS_FOR, Duration::from_secs(7 * 24 * 3600));
    }

    #[test]
    fn internal_addresses_are_never_public() {
        let no = [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.1", "169.254.169.254", "0.0.0.0",
            "100.64.0.1", "224.0.0.1", "255.255.255.255", "198.18.0.1", "::1", "::", "fe80::1", "fc00::1", "fd12::1",
            "::ffff:127.0.0.1", "::ffff:10.0.0.1", "ff02::1", "::7f00:1", "::a00:1", "::a9fe:a9fe", "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe", "2002:7f00:1::", "2002:a00:1::1", "2002:c0a8:101::",
        ];
        for s in no {
            assert!(!is_public_ip(s.parse().unwrap()), "{s}");
        }
        for s in ["8.8.8.8", "142.250.180.14", "2606:4700:4700::1111", "172.32.0.1", "100.128.0.1", "64:ff9b::808:808", "2002:808:808::1"] {
            assert!(is_public_ip(s.parse().unwrap()), "{s}");
        }
        let mixed: Vec<SocketAddr> = vec!["10.0.0.1:443".parse().unwrap(), "93.184.216.34:443".parse().unwrap()];
        assert_eq!(keep_public(mixed), vec!["93.184.216.34:443".parse::<SocketAddr>().unwrap()]);
        assert!(keep_public(vec!["127.0.0.1:443".parse().unwrap()]).is_empty());
    }

    #[test]
    fn page_reading_stops_at_the_limit_whatever_the_page_size() {
        let mut out = Vec::new();
        assert!(!append_capped(&mut out, &[1; 100], 256));
        assert!(!append_capped(&mut out, &[2; 100], 256));
        assert!(append_capped(&mut out, &[3; 100], 256));
        assert_eq!(out.len(), 256);
        assert_eq!(out[255], 3);
        // exactly at the limit also stops
        let mut o2 = Vec::new();
        assert!(append_capped(&mut o2, &[0; 256], 256));
        assert_eq!(o2.len(), 256);
        // a link in the first KB is found even when the page that follows is huge
        let mut page = b"<html><head><link rel=\"icon\" href=\"/images/favicon.svg\"></head><body>".to_vec();
        page.resize(MAX_HTML_BYTES * 4, b'x');
        let mut got = Vec::new();
        for c in page.chunks(16 * 1024) {
            if append_capped(&mut got, c, MAX_HTML_BYTES) {
                break;
            }
        }
        assert_eq!(got.len(), MAX_HTML_BYTES);
        let home = Url::parse("https://grok.com/").unwrap();
        let links = links_from_html(&String::from_utf8_lossy(&got), &home, &allow("grok"));
        assert_eq!(links.iter().map(|u| u.to_string()).collect::<Vec<_>>(), vec!["https://grok.com/images/favicon.svg"]);
    }

    #[test]
    fn new_domains_for_zai_and_copilot() {
        let z = allow("zai");
        let u = |s: &str| Url::parse(s).unwrap();
        assert!(z.url_ok(&u("https://z-cdn.chatglm.cn/z-ai/static/logo.svg")));
        assert!(!z.url_ok(&u("https://evilchatglm.cn/x.svg")));
        let c = allow("copilot");
        assert!(c.url_ok(&u("https://copilot.microsoft.com/")));
        assert!(c.url_ok(&u("https://res.cdn.office.net/assets/favicon.ico")));
        assert!(!c.url_ok(&u("https://login.microsoftonline.com/common/oauth2/authorize")));
        assert!(!c.url_ok(&u("https://login.live.com/")));
        // office.net is a Copilot-only domain
        for k in ["openai", "anthropic", "gemini", "mistral", "grok", "zai"] {
            assert!(!allow(k).url_ok(&u("https://res.cdn.office.net/x.ico")), "{k}");
        }
        // the CDN icon links the providers declare are accepted
        let g = allow("gemini");
        let page = u("https://gemini.google.com/");
        let html = "<link rel=\"icon\" href=\"https://www.gstatic.com/lamda/images/gemini_sparkle_aurora_33f86dc0c0257da337c63.png\">";
        assert_eq!(links_from_html(html, &page, &g).len(), 1);
        let m = u("https://chat.mistral.ai/");
        assert_eq!(links_from_html("<link rel=\"icon\" type=\"image/png\" sizes=\"32x32\" href=\"/favicons/favicon-32x32.png\">", &m, &allow("mistral")).len(), 1);
        let cp = u("https://copilot.microsoft.com/");
        assert_eq!(links_from_html("<link rel=\"icon\" href=\"https://res.cdn.office.net/x/fav.ico\">", &cp, &c).len(), 1);
    }

    /// Live check against the real sites (needs the network): `cargo test --lib live_ -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_every_provider() {
        for s in SITES {
            let r = tauri::async_runtime::block_on(download(s));
            let what = match &r {
                Outcome::Icon(b) => format!("ICON {} bytes {:?}", b.len(), validate_icon(b)),
                Outcome::NotFound => "no icon".to_string(),
                Outcome::NetworkError => "network error".to_string(),
            };
            println!("{:<11} {}", s.key, what);
        }
    }

    #[test]
    fn an_expired_icon_is_not_retried_within_a_week() {
        let day = MISS_FOR;
        assert!(should_background_refresh(false, false, None));
        assert!(should_background_refresh(false, false, Some(day)));
        assert!(!should_background_refresh(false, false, Some(day - Duration::from_secs(1))));
        assert!(!should_background_refresh(true, false, None));
        assert!(!should_background_refresh(false, true, None));
    }

    #[test]
    fn data_url_is_prefixed_with_the_sniffed_mime() {
        let u = to_data_url(&png(), Kind::Png);
        assert!(u.starts_with("data:image/png;base64,"));
        assert!(to_data_url(b"<svg/>", Kind::Svg).starts_with("data:image/svg+xml;base64,"));
    }
}
