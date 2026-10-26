//! Attachments a copy cannot carry, and the images a target would refuse.
//!
//! A message-based copy (Claude, OpenCode, Antigravity) carries an image as the target's own image
//! block when the target takes it. Whatever it cannot carry leaves a text note in the same message,
//! at the same position, so the reader sees that something was there and where it was. A note is
//! ordinary text: every converter and every target carries it, and the copy stays valid. The
//! converters' counters still count what was dropped.
//!
//! What is not carried:
//! - a file or image held by reference (a path, a URL, a Files API id): the journal has no bytes;
//! - an image with no bytes, or only a blob reference the journal does not hold;
//! - an image over a target's limit (`Limits`): its bytes, its longer side, or for Claude the
//!   stricter side once one request holds more than 20 images;
//! - a tool-result image, in a target with no slot for one.
//!
//! `judge` is the one decision: the writers call it to carry or note an image, and `fit` calls it to
//! estimate the image's tokens, so the estimate and the copy agree. Limits are read from each
//! target's documentation (see `docs/ARCHITECTURE.md`, "Attachments").

use crate::fit::tokens_for_bytes;
use crate::journal::{Msg, BLOB_PREFIX};
use base64::Engine;
use serde_json::Value;

#[cfg(test)]
pub(crate) mod tests;

/// Every note starts with this, so a reader (and sesync's prompt check) can tell it from typed text.
pub const NOTE_PREFIX: &str = "[attachment not carried: ";
/// The tail of a note whose attachment still exists in the source session.
pub const KEPT: &str = "kept in the source session";

/// What one target accepts for an image. Sizes are checked on the base64 text the request carries.
pub struct Limits {
    /// The target's name, as the note says it.
    pub name: &'static str,
    /// Largest base64 `data` of one image, in bytes.
    pub max_data_len: usize,
    /// Largest longer side of one image, in pixels.
    pub max_side: u32,
    /// Above `.0` images in one request, every image's longer side must be at most `.1` pixels.
    pub many_images: Option<(usize, u32)>,
}

/// Claude. The API takes 10 MB of base64 per image and 8000 px per side; Bedrock takes 5 MB
/// (https://platform.claude.com/docs/en/build-with-claude/vision, "Request limits", checked
/// 2026-10-26). Claude Code also runs on Bedrock, so the copy keeps to the smaller 5 MB. Above 20
/// images in one request, the longer side is 2000 px at most, on every platform.
pub const CLAUDE: Limits = Limits {
    name: "Claude",
    max_data_len: 5_000_000,
    max_side: 8_000,
    many_images: Some((20, 2_000)),
};

/// OpenCode sends an image to whichever provider the session uses. No single provider is named
/// in a copy, so the limit is the smallest documented one: Claude's 5 MB on Bedrock. OpenCode
/// documents no pixel limit of its own; the Anthropic 8000 px rule is kept as the bound.
pub const OPENCODE: Limits = Limits {
    name: "OpenCode's providers",
    max_data_len: 5_000_000,
    max_side: 8_000,
    many_images: None,
};

/// Antigravity sends images to Gemini. Gemini documents a 20 MB limit on the whole inline request
/// (https://ai.google.dev/gemini-api/docs/image-understanding, checked 2026-10-26), which bounds
/// each image; it documents no per-image pixel limit.
pub const ANTIGRAVITY: Limits = Limits {
    name: "Gemini",
    max_data_len: 20_000_000,
    max_side: u32::MAX,
    many_images: None,
};

impl Limits {
    /// The limits of a message-based target, by the name `fit` and the converters use. An unknown
    /// name gets Claude's limits, the strictest documented.
    pub fn for_target(target: &str) -> &'static Limits {
        match target {
            "opencode" => &OPENCODE,
            "antigravity" => &ANTIGRAVITY,
            _ => &CLAUDE,
        }
    }
}

/// Whether one image goes to the target as it is.
pub enum Verdict {
    Carry,
    /// Replaced by this note text.
    Withhold(String),
}

/// The note for an attachment: `[attachment not carried: label (mime, size) — why]`. `mime` and
/// `bytes` are omitted when unknown. Never content.
pub fn note(label: &str, mime: Option<&str>, bytes: Option<usize>, why: &str) -> String {
    let mut detail: Vec<String> = Vec::new();
    if let Some(m) = mime.filter(|m| !m.is_empty()) {
        detail.push(m.to_string());
    }
    if let Some(n) = bytes {
        detail.push(human_size(n));
    }
    let what = if detail.is_empty() { label.to_string() } else { format!("{label} ({})", detail.join(", ")) };
    format!("{NOTE_PREFIX}{what} \u{2014} {why}]")
}

/// The note for a canonical image block (`data` base64, `mimeType`).
pub fn image_note(image: &Value, why: &str) -> String {
    image_note_parts(image.get("mimeType").and_then(Value::as_str), image.get("data").and_then(Value::as_str).unwrap_or(""), why)
}

/// The note for an image given its MIME type and base64 data (empty or a blob reference: no size).
pub fn image_note_parts(mime: Option<&str>, data: &str, why: &str) -> String {
    let bytes = (!data.is_empty() && !data.starts_with(BLOB_PREFIX)).then(|| decoded_len(data));
    note("image", mime, bytes, why)
}

/// Bytes of a base64 string once decoded (its length times 3/4, less the padding).
pub fn decoded_len(b64: &str) -> usize {
    let pad = b64.bytes().rev().take_while(|&c| c == b'=').count().min(2);
    (b64.len() / 4 * 3).saturating_sub(pad)
}

/// "512 B", "1.2 KB", "5 MB": decimal units, one decimal place when it is not a whole number.
pub fn human_size(n: usize) -> String {
    let (v, unit) = if n < 1_000 {
        return format!("{n} B");
    } else if n < 1_000_000 {
        (n as f64 / 1_000.0, "KB")
    } else {
        (n as f64 / 1_000_000.0, "MB")
    };
    let s = format!("{v:.1}");
    format!("{} {unit}", s.strip_suffix(".0").unwrap_or(&s))
}

/// The decision for one canonical image block (`data` base64, `mimeType`) going to `limits`.
/// `many`: the request this image is in holds more than the `many_images` count of images.
pub fn judge(limits: &Limits, image: &Value, many: bool) -> Verdict {
    let data = image.get("data").and_then(Value::as_str).unwrap_or("");
    let mime = image.get("mimeType").and_then(Value::as_str);
    if data.is_empty() {
        return Verdict::Withhold(note("image", mime, None, "the journal holds no bytes for it"));
    }
    if data.starts_with(BLOB_PREFIX) {
        return Verdict::Withhold(note("image", mime, None, "the journal holds only a blob reference to it"));
    }
    let bytes = decoded_len(data);
    let withheld = |reason: String| Verdict::Withhold(note("image", mime, Some(bytes), &format!("{reason}; {KEPT}")));
    if data.len() > limits.max_data_len {
        return withheld(format!("over the {} limit for one image in {}", human_size(limits.max_data_len), limits.name));
    }
    if let Some((w, h)) = dims(data) {
        let long = w.max(h);
        if long > limits.max_side {
            return withheld(format!("{w}x{h} px is over the {} px limit for one image in {}", limits.max_side, limits.name));
        }
        if let (true, Some((count, side))) = (many, limits.many_images) {
            if long > side {
                return withheld(format!(
                    "{w}x{h} px is over the {side} px limit for a request with more than {count} images in {}",
                    limits.name
                ));
            }
        }
    }
    Verdict::Carry
}

/// Images in the messages that the target carries as they are (user messages; the same test the
/// Claude writer applies with `many = false`). Used to decide the many-image rule for one copy.
pub fn carried_count(limits: &Limits, msgs: &[Msg]) -> usize {
    msgs.iter()
        .filter(|m| m.role == "user")
        .flat_map(|m| m.content.iter())
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("image"))
        .filter(|b| matches!(judge(limits, b, false), Verdict::Carry))
        .count()
}

/// Visual tokens an image costs in the copy, as `fit` counts it: its note when it is withheld,
/// else the larger of the two documented rules for its size (`image_tokens`), or the fallback when
/// the size cannot be read from its header.
pub fn estimate(limits: &Limits, image: &Value) -> usize {
    match judge(limits, image, false) {
        Verdict::Withhold(note) => tokens_for_bytes(note.len()),
        Verdict::Carry => match image.get("data").and_then(Value::as_str).and_then(dims) {
            Some((w, h)) => image_tokens(w, h),
            None => FALLBACK_IMAGE_TOKENS,
        },
    }
}

/// Tokens for an image the size of which cannot be read from its header. Claude's standard tier
/// costs at most 1,568 tokens for an image; the high-resolution tier can cost up to 4,784.
pub const FALLBACK_IMAGE_TOKENS: usize = 1_600;

/// The longest note a tool-result image gets: the prefix, its size, and the longest reason any
/// target writes for one. Every target's wording fits under it.
const RESULT_IMAGE_NOTE_MAX: usize = 160;

/// Tokens a tool-result image costs in a copy: its note, bounded by `RESULT_IMAGE_NOTE_MAX`.
pub fn result_image_tokens() -> usize {
    tokens_for_bytes(RESULT_IMAGE_NOTE_MAX)
}

/// The largest visual-token count of one image on Claude's high-resolution tier (Claude 4.7 and
/// later). The standard tier caps at 1,568. The copy does not know which model resumes it, so the
/// estimate uses the larger cap.
pub const ANTHROPIC_MAX_TOKENS: usize = 4_784;

/// Visual tokens of a `w` x `h` image: the larger of the two rules below. Each is an upper bound.
pub fn image_tokens(w: u32, h: u32) -> usize {
    anthropic_tokens(w, h).max(openai_tiles(w, h))
}

/// Claude: one token per 28 x 28 pixel patch, `ceil(w/28) * ceil(h/28)`, capped at the tier maximum.
/// Downscaling only lowers the patch count, so the count of the image as it is bounds the count
/// after Claude resizes it.
pub fn anthropic_tokens(w: u32, h: u32) -> usize {
    let patches = (w as usize).div_ceil(28) * (h as usize).div_ceil(28);
    patches.min(ANTHROPIC_MAX_TOKENS)
}

/// OpenAI tile rule (gpt-4o, gpt-4.1; `detail: high`): scale to fit a 2048 px square, then scale
/// the shorter side down to 768 px (rounding down), then `85 + 170` per 512 px tile.
pub fn openai_tiles(w: u32, h: u32) -> usize {
    let (mut w, mut h) = (w as f64, h as f64);
    let s = (2048.0 / w.max(h)).min(1.0);
    w = (w * s).floor();
    h = (h * s).floor();
    let short = w.min(h);
    if short > 768.0 {
        let s2 = 768.0 / short;
        w = (w * s2).floor();
        h = (h * s2).floor();
    }
    let tiles = (w / 512.0).ceil() * (h / 512.0).ceil();
    85 + 170 * tiles as usize
}

/// The pixel size of an image from its base64 data, read from the header: PNG, JPEG, GIF, WebP.
/// None when the header is not one of them or is cut short.
pub fn dims(data_b64: &str) -> Option<(u32, u32)> {
    // A JPEG's size sits in its SOF segment, usually near the start; decode a head first and the
    // whole image only when the head does not reach it.
    const HEAD_CHARS: usize = 128 * 1024;
    if let Some(head) = data_b64.get(..HEAD_CHARS / 4 * 4).filter(|_| data_b64.len() > HEAD_CHARS) {
        if let Some(d) = decode(head).and_then(|b| header_dims(&b)) {
            return Some(d);
        }
    }
    header_dims(&decode(data_b64)?)
}

fn decode(b64: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(b64).ok()
}

fn header_dims(b: &[u8]) -> Option<(u32, u32)> {
    let d = if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.len() >= 24 && &b[12..16] == b"IHDR" {
        Some((be32(&b[16..20]), be32(&b[20..24])))
    } else if (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) && b.len() >= 10 {
        Some((le16(&b[6..8]) as u32, le16(&b[8..10]) as u32))
    } else if b.len() >= 20 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        webp_dims(b)
    } else if b.starts_with(&[0xFF, 0xD8]) {
        jpeg_dims(b)
    } else {
        None
    };
    d.filter(|(w, h)| *w > 0 && *h > 0)
}

/// WebP: the lossy `VP8 `, lossless `VP8L` and extended `VP8X` headers.
fn webp_dims(b: &[u8]) -> Option<(u32, u32)> {
    match &b[12..16] {
        b"VP8 " if b.len() >= 30 && b[23..26] == [0x9d, 0x01, 0x2a] => {
            Some(((le16(&b[26..28]) & 0x3fff) as u32, (le16(&b[28..30]) & 0x3fff) as u32))
        }
        b"VP8L" if b.len() >= 25 && b[20] == 0x2f => {
            let v = le32(&b[21..25]);
            Some(((v & 0x3fff) + 1, ((v >> 14) & 0x3fff) + 1))
        }
        b"VP8X" if b.len() >= 30 => Some((le24(&b[24..27]) + 1, le24(&b[27..30]) + 1)),
        _ => None,
    }
}

/// JPEG: the first start-of-frame segment (SOF0..SOF15 but not DHT, JPG or DAC).
fn jpeg_dims(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let m = b[i + 1];
        match m {
            0xFF => i += 1,
            0xD8 | 0x01 | 0xD0..=0xD7 => i += 2,
            0xD9 | 0xDA => return None,
            _ => {
                if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
                    if i + 9 > b.len() {
                        return None;
                    }
                    return Some((be16(&b[i + 7..i + 9]) as u32, be16(&b[i + 5..i + 7]) as u32));
                }
                let len = be16(&b[i + 2..i + 4]) as usize;
                if len < 2 {
                    return None;
                }
                i += 2 + len;
            }
        }
    }
    None
}

fn be16(s: &[u8]) -> u16 {
    u16::from_be_bytes([s[0], s[1]])
}
fn le16(s: &[u8]) -> u16 {
    u16::from_le_bytes([s[0], s[1]])
}
fn be32(s: &[u8]) -> u32 {
    u32::from_be_bytes([s[0], s[1], s[2], s[3]])
}
fn le32(s: &[u8]) -> u32 {
    u32::from_le_bytes([s[0], s[1], s[2], s[3]])
}
fn le24(s: &[u8]) -> u32 {
    u32::from_le_bytes([s[0], s[1], s[2], 0])
}
