//! Read-aloud audio: the provider's own voice, captured while it streams and kept as a local file.
//!
//! Captured live (15/09/2026): Claude and DeepSeek both send speech over a WebSocket as raw Opus packets,
//! one packet per binary frame, with no container. Claude sends the bare packet; DeepSeek prefixes each one
//! with a 4-byte big-endian sequence number. A raw packet list cannot be played, so on save the packets are
//! wrapped in an Ogg Opus stream (RFC 7845), which every webview plays with a plain `<audio>` element.

/// Where a provider's speech socket connects, as a regex source for the page, or None when the provider has
/// no read-aloud we can capture. The same list decides whether the UI offers the button at all.
pub fn tts_url_pattern(key: &str) -> Option<&'static str> {
    match key {
        "anthropic" => Some("/api/ws/text_to_speech/"),
        "deepseek" => Some("/api/v0/chat/tts/"),
        // ChatGPT answers with a finished audio file instead of a stream of packets (captured: audio/aac).
        "openai" => Some("/backend-api/synthesize"),
        _ => None,
    }
}

/// The provider's read-aloud control under an answer, by structure only: (CSS selector, start of the icon's SVG
/// path when the button carries no stable attribute, menu item to pick when the control opens a menu instead).
/// The last match is the last answer's.
pub fn tts_button(key: &str) -> (&'static str, &'static str, &'static str) {
    match key {
        "anthropic" => ("[data-testid=\"action-bar-read-aloud\"]", "", ""),
        "deepseek" => ("", "M9.31006 14.8936", ""),
        // ChatGPT keeps read aloud in the row's last control ("more actions"), which carries no stable attribute of
        // its own: it is addressed as the last button of the row that holds the copy button, then the menu item.
        "openai" => ("row-last:[data-testid=\"copy-turn-action-button\"]", "", "[data-testid=\"voice-play-turn-action-button\"]"),
        _ => ("", "", ""),
    }
}

/// File extension for an audio content type, for the providers that hand over a finished file.
pub fn container_ext(ct: &str) -> Option<&'static str> {
    let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    Some(match ct.as_str() {
        "audio/aac" | "audio/aacp" | "audio/x-aac" => "aac",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/webm" => "webm",
        _ => return None,
    })
}

/// Bytes in front of the Opus packet inside one binary frame.
pub fn frame_prefix(key: &str) -> usize {
    match key {
        "deepseek" => 4, // sequence number
        _ => 0,
    }
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            acc |= val(*c)? << (18 - 6 * i as u32);
        }
        let n = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        for i in 0..n {
            out.push((acc >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

/// Samples at 48 kHz carried by one Opus packet, read from its TOC byte (RFC 6716, section 3.1).
pub fn packet_samples(p: &[u8]) -> Option<u64> {
    let toc = *p.first()?;
    let config = toc >> 3;
    // Frame duration in tenths of a millisecond.
    let tenths: u64 = match config {
        0..=11 => [100, 200, 400, 600][(config % 4) as usize],
        12..=15 => [100, 200][(config % 2) as usize],
        _ => [25, 50, 100, 200][(config % 4) as usize],
    };
    let frames: u64 = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*p.get(1)? & 0x3f) as u64,
    };
    Some(frames * tenths * 48 / 10)
}

fn ogg_crc(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04c1_1db7 } else { crc << 1 };
        }
    }
    crc
}

fn ogg_page(out: &mut Vec<u8>, serial: u32, seq: u32, flags: u8, granule: u64, packets: &[&[u8]]) {
    let mut lacing = Vec::new();
    for p in packets {
        let mut n = p.len();
        while n >= 255 {
            lacing.push(255u8);
            n -= 255;
        }
        lacing.push(n as u8);
    }
    let start = out.len();
    out.extend_from_slice(b"OggS");
    out.push(0);
    out.push(flags);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.push(lacing.len() as u8);
    out.extend_from_slice(&lacing);
    for p in packets {
        out.extend_from_slice(p);
    }
    let crc = ogg_crc(&out[start..]);
    out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
}

/// Wraps raw Opus packets in an Ogg Opus stream. Returns the file bytes and the duration in milliseconds.
/// Packets whose TOC cannot be read are dropped rather than corrupting the timeline.
pub fn ogg_opus(packets: &[Vec<u8>], serial: u32) -> Option<(Vec<u8>, u64)> {
    let packets: Vec<(&[u8], u64)> = packets
        .iter()
        .filter_map(|p| packet_samples(p).filter(|s| *s > 0).map(|s| (p.as_slice(), s)))
        .collect();
    if packets.is_empty() {
        return None;
    }
    let channels: u8 = if packets[0].0[0] & 0x04 != 0 { 2 } else { 1 };
    let mut out = Vec::new();
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(channels);
    head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip: unknown for a stream we did not encode
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family
    ogg_page(&mut out, serial, 0, 0x02, 0, &[&head]);
    let vendor = b"Kotodama";
    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes());
    ogg_page(&mut out, serial, 1, 0, 0, &[&tags]);
    // Audio pages: whole packets only, about one second per page, never more than 255 lacing values.
    let mut seq = 2u32;
    let mut granule = 0u64;
    let mut i = 0;
    while i < packets.len() {
        let mut page: Vec<&[u8]> = Vec::new();
        let (mut segs, mut samples) = (0usize, 0u64);
        while i < packets.len() {
            let (p, s) = packets[i];
            let need = p.len() / 255 + 1;
            if !page.is_empty() && (segs + need > 255 || samples >= 48_000) {
                break;
            }
            page.push(p);
            segs += need;
            samples += s;
            granule += s;
            i += 1;
        }
        let flags = if i == packets.len() { 0x04 } else { 0 };
        ogg_page(&mut out, serial, seq, flags, granule, &page);
        seq += 1;
    }
    Some((out, granule / 48))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        assert_eq!(base64_decode("TWFu").unwrap(), b"Man");
        assert_eq!(base64_decode("TWE=").unwrap(), b"Ma");
        assert_eq!(base64_decode("TQ==").unwrap(), b"M");
        assert!(base64_decode("T").is_none());
        assert!(base64_decode("T$==").is_none());
    }

    #[test]
    fn toc_durations() {
        // Claude: config 15 (hybrid, 20 ms), one frame.
        assert_eq!(packet_samples(&[0x78, 0x0c]), Some(960));
        // DeepSeek: config 13 (hybrid, 20 ms), code 3 with 5 frames.
        assert_eq!(packet_samples(&[0x6b, 0x85]), Some(4800));
        // CELT 2.5 ms, two frames.
        assert_eq!(packet_samples(&[(16 << 3) | 1]), Some(240));
        assert_eq!(packet_samples(&[]), None);
    }

    #[test]
    fn ogg_structure() {
        let pk: Vec<Vec<u8>> = (0..120).map(|_| vec![0x78, 1, 2, 3]).collect();
        let (bytes, ms) = ogg_opus(&pk, 7).unwrap();
        assert_eq!(ms, 120 * 20);
        assert_eq!(&bytes[..4], b"OggS");
        assert_eq!(bytes[5], 0x02);
        // Every page checksum verifies.
        let mut pos = 0;
        let mut pages = 0;
        while pos < bytes.len() {
            let nseg = bytes[pos + 26] as usize;
            let body: usize = bytes[pos + 27..pos + 27 + nseg].iter().map(|x| *x as usize).sum();
            let end = pos + 27 + nseg + body;
            let mut page = bytes[pos..end].to_vec();
            let want = u32::from_le_bytes(page[22..26].try_into().unwrap());
            page[22..26].copy_from_slice(&[0; 4]);
            assert_eq!(ogg_crc(&page), want);
            pages += 1;
            if end == bytes.len() {
                assert_eq!(bytes[pos + 5], 0x04);
            }
            pos = end;
        }
        assert!(pages >= 4);
    }

    /// Replays a captured DeepSeek speech socket (KOTO_NETPROBE record file) into an .ogg next to it.
    /// Run with AUDIO_FIXTURE=<path to jsonl>; the output is checked with ffprobe by hand.
    #[test]
    fn replay_fixture() {
        let Ok(path) = std::env::var("AUDIO_FIXTURE") else { return };
        let key = std::env::var("AUDIO_KEY").unwrap_or_else(|_| "deepseek".into());
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut packets = Vec::new();
        for line in raw.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let r = &v["rec"];
            if r["kind"] != "ws" || r["ev"] != "chunk" {
                continue;
            }
            let Some(b64) = r["data"].as_str().and_then(|d| d.strip_prefix("base64:")) else { continue };
            let bytes = base64_decode(b64).unwrap();
            packets.push(bytes[frame_prefix(&key)..].to_vec());
        }
        let (ogg, ms) = ogg_opus(&packets, 1).unwrap();
        eprintln!("packets={} ms={ms}", packets.len());
        std::fs::write(format!("{path}.ogg"), ogg).unwrap();
    }
}
