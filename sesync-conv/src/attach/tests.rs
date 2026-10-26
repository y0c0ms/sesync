//! Attachment tests: image headers and tokens, the limits and the notes, and the round trips that
//! carry images between the converters. The images are generated here: a valid PNG and a valid
//! baseline JPEG, each with bytes that depend on its size and seed. Every test works in a scratch
//! directory under the temp dir.

use super::*;
use crate::journal::Journal;
use crate::{claude, from_antigravity, from_claude, from_opencode, opencode, to_antigravity};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const CWD: &str = "/work/proj";

fn sandbox(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("sesync-conv-attach-{}-{name}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn journal_from_lines(dir: &Path, name: &str, lines: &[String]) -> Journal {
    let p = dir.join(name);
    fs::write(&p, lines.join("\n") + "\n").unwrap();
    Journal::read(&p).unwrap()
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// A valid 8-bit RGB PNG of `w` x `h` pixels (stored deflate blocks, real CRCs).
pub fn png(w: u32, h: u32, seed: u8) -> Vec<u8> {
    let mut raw = Vec::new();
    for y in 0..h {
        raw.push(0);
        for x in 0..w {
            raw.extend_from_slice(&[seed.wrapping_add(x as u8), seed.wrapping_add(y as u8), seed]);
        }
    }
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65_535).peekable();
    while let Some(c) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        let len = c.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &z);
    png_chunk(&mut out, b"IEND", &[]);
    out
}

/// A PNG header only: enough for the size, not a decodable image.
pub(crate) fn png_header(w: u32, h: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    png_chunk(&mut out, b"IHDR", &ihdr);
    out
}

/// A valid baseline JPEG of `w` x `h` pixels, one grey component: every 8 x 8 block is a zero DC
/// difference and an end-of-block, two bits each.
pub fn jpeg(w: u16, h: u16) -> Vec<u8> {
    let mut o = vec![0xFF, 0xD8];
    o.extend_from_slice(&[0xFF, 0xDB, 0x00, 67, 0x00]);
    o.extend_from_slice(&[1u8; 64]);
    o.extend_from_slice(&[0xFF, 0xC0, 0x00, 11, 8]);
    o.extend_from_slice(&h.to_be_bytes());
    o.extend_from_slice(&w.to_be_bytes());
    o.extend_from_slice(&[1, 1, 0x11, 0]);
    for class_and_id in [0x00u8, 0x10] {
        o.extend_from_slice(&[0xFF, 0xC4, 0x00, 20, class_and_id, 1]);
        o.extend_from_slice(&[0u8; 15]);
        o.push(0x00);
    }
    o.extend_from_slice(&[0xFF, 0xDA, 0x00, 8, 1, 1, 0x00, 0, 63, 0]);
    let blocks = (w as usize).div_ceil(8) * (h as usize).div_ceil(8);
    let bits = 2 * blocks;
    let mut scan = vec![0u8; bits.div_ceil(8)];
    for bit in bits..scan.len() * 8 {
        scan[bit / 8] |= 0x80 >> (bit % 8);
    }
    o.extend_from_slice(&scan);
    o.extend_from_slice(&[0xFF, 0xD9]);
    o
}

fn riff(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut o = b"RIFF".to_vec();
    o.extend_from_slice(&((4 + 8 + payload.len()) as u32).to_le_bytes());
    o.extend_from_slice(b"WEBP");
    o.extend_from_slice(kind);
    o.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    o.extend_from_slice(payload);
    o
}

fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

fn image(bytes: &[u8], mime: &str) -> Value {
    json!({"type": "image", "data": b64(bytes), "mimeType": mime})
}

/// The shape of a content list as a target shows it: each text block, and each image by the
/// SHA-256 of its decoded bytes and its MIME type, in order.
fn shape(content: &Value) -> Vec<String> {
    let blocks: Vec<Value> = match content {
        Value::String(s) => vec![json!({"type": "text", "text": s})],
        Value::Array(a) => a.clone(),
        _ => vec![],
    };
    blocks
        .iter()
        .map(|b| match b["type"].as_str() {
            Some("text") => format!("text:{}", b["text"].as_str().unwrap_or("")),
            Some("image") => {
                let bytes = B64.decode(b["data"].as_str().unwrap()).unwrap();
                format!("image:{}:{}", hex::encode(Sha256::digest(&bytes)), b["mimeType"].as_str().unwrap())
            }
            other => format!("other:{other:?}"),
        })
        .collect()
}

/// The content shape of every user message of a journal, in order.
fn user_shapes(j: &Journal) -> Vec<Vec<String>> {
    j.records
        .iter()
        .filter_map(|r| r.get("message"))
        .filter(|m| m["role"] == "user")
        .map(|m| shape(&m["content"]))
        .collect()
}

fn images_of(shapes: &[Vec<String>]) -> Vec<String> {
    shapes.iter().flatten().filter(|s| s.starts_with("image:")).cloned().collect()
}

/// An omp journal: a header, then `(role, content)` messages chained by `parentId`.
fn omp_session(messages: &[(&str, Value)]) -> Vec<String> {
    let mut lines = vec![json!({"type": "session", "id": "omp-attach", "timestamp": "2026-10-01T10:00:00.000Z", "cwd": CWD}).to_string()];
    let mut parent: Option<String> = None;
    for (i, (role, content)) in messages.iter().enumerate() {
        let id = format!("m{i}");
        let ts = format!("2026-10-01T10:00:{:02}.000Z", i + 1);
        let mut message = json!({"role": role, "content": content});
        if *role == "assistant" {
            message["provider"] = json!("anthropic");
            message["model"] = json!("claude-sonnet-4-5");
        }
        lines.push(json!({"type": "message", "id": id, "parentId": parent, "timestamp": ts, "message": message}).to_string());
        parent = Some(id);
    }
    lines
}

/// The session the round trips carry: a user turn with a PNG and a JPEG between text, an
/// image-only turn, and two replies.
fn sample_messages() -> Vec<(&'static str, Value)> {
    let png_a = png(4, 3, 7);
    let jpg_b = jpeg(5, 2);
    let png_c = png(2, 2, 90);
    vec![
        (
            "user",
            json!([
                {"type": "text", "text": "before"},
                image(&png_a, "image/png"),
                {"type": "text", "text": "between"},
                image(&jpg_b, "image/jpeg"),
                {"type": "text", "text": "after"}
            ]),
        ),
        ("assistant", json!([{"type": "text", "text": "I see two images."}])),
        ("user", json!([image(&png_c, "image/png")])),
        ("assistant", json!([{"type": "text", "text": "Got it."}])),
    ]
}

// ---- headers, sizes and tokens ----

#[test]
fn the_size_is_read_from_png_jpeg_gif_and_webp_headers() {
    assert_eq!(header_dims(&png_header(640, 480)), Some((640, 480)));
    assert_eq!(header_dims(&jpeg(37, 19)), Some((37, 19)));
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&300u16.to_le_bytes());
    gif.extend_from_slice(&200u16.to_le_bytes());
    assert_eq!(header_dims(&gif), Some((300, 200)));
    let mut vp8x = vec![0u8; 4];
    vp8x.extend_from_slice(&(1920u32 - 1).to_le_bytes()[..3]);
    vp8x.extend_from_slice(&(1080u32 - 1).to_le_bytes()[..3]);
    assert_eq!(header_dims(&riff(b"VP8X", &vp8x)), Some((1920, 1080)));
    let mut vp8l = vec![0x2f];
    vp8l.extend_from_slice(&((639u32) | (479u32 << 14)).to_le_bytes());
    assert_eq!(header_dims(&riff(b"VP8L", &vp8l)), Some((640, 480)));
    let mut vp8 = vec![0, 0, 0, 0x9d, 0x01, 0x2a];
    vp8.extend_from_slice(&800u16.to_le_bytes());
    vp8.extend_from_slice(&600u16.to_le_bytes());
    assert_eq!(header_dims(&riff(b"VP8 ", &vp8)), Some((800, 600)));
}

#[test]
fn an_unknown_or_cut_short_header_has_no_size() {
    assert_eq!(header_dims(b"not an image at all"), None);
    assert_eq!(header_dims(&png_header(10, 10)[..20]), None, "a PNG cut inside IHDR");
    assert_eq!(header_dims(&jpeg(16, 16)[..40]), None, "a JPEG cut before its SOF");
    assert_eq!(dims("AAAA"), None, "three zero bytes are no image");
    assert_eq!(dims("!!!!"), None, "not base64");
}

#[test]
fn a_jpeg_whose_size_lies_beyond_the_head_is_still_read() {
    let j = jpeg(123, 45);
    let mut o = j[..2].to_vec();
    for _ in 0..2 {
        o.extend_from_slice(&[0xFF, 0xE1, 0xFF, 0xFF]);
        o.extend(std::iter::repeat(0u8).take(65_533));
    }
    o.extend_from_slice(&j[2..]);
    assert!(b64(&o).len() > 128 * 1024, "the size is past the head that dims() decodes first");
    assert_eq!(dims(&b64(&o)), Some((123, 45)));
}

#[test]
fn image_tokens_follow_the_documented_rules() {
    // Claude: one token per 28 x 28 patch.
    assert_eq!(anthropic_tokens(1568, 1568), 56 * 56);
    assert_eq!(anthropic_tokens(28, 28), 1);
    assert_eq!(anthropic_tokens(9000, 9000), ANTHROPIC_MAX_TOKENS, "capped at the high-resolution tier");
    // OpenAI tiles: 85 + 170 per 512 px tile, after fitting 2048 and the short side to 768.
    assert_eq!(openai_tiles(100, 100), 85 + 170);
    assert_eq!(openai_tiles(1000, 1000), 85 + 170 * 4, "1000 px scales to 768 px: 2 x 2 tiles");
    assert_eq!(openai_tiles(2048, 768), 85 + 170 * 8);
    // The estimate is the larger of the two.
    assert_eq!(image_tokens(1920, 1080), 69 * 39, "1920 x 1080: 2691 patches beat the tiles");
    assert_eq!(image_tokens(1000, 1000), 36 * 36, "1000 x 1000: 1296 patches beat 765 tiles");
    assert_eq!(image_tokens(100, 100), 85 + 170, "a small image: the tile base wins");
}

#[test]
fn an_unreadable_image_costs_the_fallback_and_a_known_one_its_formula() {
    let unknown = json!({"type": "image", "data": "AAAA", "mimeType": "image/png"});
    assert_eq!(estimate(&CLAUDE, &unknown), FALLBACK_IMAGE_TOKENS);
    let known = json!({"type": "image", "data": b64(&png_header(1920, 1080)), "mimeType": "image/png"});
    assert_eq!(estimate(&CLAUDE, &known), image_tokens(1920, 1080));
}

// ---- notes and limits ----

#[test]
fn a_note_has_the_documented_shape() {
    assert_eq!(
        note("report.pdf", Some("application/pdf"), Some(1_200_000), KEPT),
        "[attachment not carried: report.pdf (application/pdf, 1.2 MB) \u{2014} kept in the source session]"
    );
    assert_eq!(note("attachment", None, None, KEPT), "[attachment not carried: attachment \u{2014} kept in the source session]");
    assert!(note("x", None, None, KEPT).starts_with(NOTE_PREFIX));
}

#[test]
fn sizes_read_in_decimal_units() {
    assert_eq!(human_size(512), "512 B");
    assert_eq!(human_size(1_000), "1 KB");
    assert_eq!(human_size(1_234), "1.2 KB");
    assert_eq!(human_size(5_000_000), "5 MB");
    assert_eq!(human_size(1_250_000), "1.2 MB");
}

#[test]
fn decoded_size_is_read_from_the_base64_text() {
    assert_eq!((decoded_len("AAAA"), decoded_len("AA=="), decoded_len("AAA=")), (3, 1, 2));
}

#[test]
fn judge_carries_what_a_target_takes_and_withholds_the_rest_with_a_reason() {
    let ok = json!({"type": "image", "data": b64(&png_header(64, 64)), "mimeType": "image/png"});
    assert!(matches!(judge(&CLAUDE, &ok, false), Verdict::Carry));

    let empty = json!({"type": "image", "data": "", "mimeType": "image/png"});
    let Verdict::Withhold(n) = judge(&CLAUDE, &empty, false) else { panic!("no bytes is withheld") };
    assert!(n.ends_with("the journal holds no bytes for it]"), "{n}");

    let blob = json!({"type": "image", "data": "blob:sha256:00", "mimeType": "image/png"});
    let Verdict::Withhold(n) = judge(&CLAUDE, &blob, false) else { panic!("a blob reference is withheld") };
    assert!(n.contains("blob reference"), "{n}");

    // Claude: 8000 px per side.
    let wide = json!({"type": "image", "data": b64(&png_header(9000, 100)), "mimeType": "image/png"});
    let Verdict::Withhold(n) = judge(&CLAUDE, &wide, false) else { panic!("9000 px is over 8000") };
    assert!(n.contains("9000x100 px is over the 8000 px limit for one image in Claude"), "{n}");
    assert!(n.ends_with("; kept in the source session]"), "{n}");

    // Claude: above 20 images in one request, 2000 px.
    let medium = json!({"type": "image", "data": b64(&png_header(2100, 100)), "mimeType": "image/png"});
    assert!(matches!(judge(&CLAUDE, &medium, false), Verdict::Carry), "2100 px is fine on its own");
    let Verdict::Withhold(n) = judge(&CLAUDE, &medium, true) else { panic!("2100 px is over 2000 in a many-image request") };
    assert!(n.contains("over the 2000 px limit for a request with more than 20 images"), "{n}");

    // OpenCode and Antigravity have no pixel rule in their documentation: 2100 px is carried.
    assert!(matches!(judge(&OPENCODE, &medium, true), Verdict::Carry));
    assert!(matches!(judge(&ANTIGRAVITY, &wide, true), Verdict::Carry));

    // Bytes: the 5 MB limit is on the base64 text (the largest Claude platform takes 10 MB, Bedrock 5 MB).
    let mut big = png_header(100, 100);
    big.resize(3_800_000, 0);
    let huge = json!({"type": "image", "data": b64(&big), "mimeType": "image/png"});
    let Verdict::Withhold(n) = judge(&CLAUDE, &huge, false) else { panic!("5 MB of base64 is over the limit") };
    assert!(n.starts_with("[attachment not carried: image (image/png, 3.8 MB) \u{2014} over the 5 MB limit for one image in Claude"), "{n}");
    assert!(matches!(judge(&ANTIGRAVITY, &huge, false), Verdict::Carry), "Gemini's limit is 20 MB");
}

#[test]
fn carried_count_counts_only_the_user_images_a_target_takes() {
    let dir = sandbox("carried");
    let mut content = vec![json!({"type": "text", "text": "x"})];
    content.extend((0..3).map(|_| image(&png_header(10, 10), "image/png")));
    content.push(image(&png_header(9000, 10), "image/png"));
    let j = journal_from_lines(&dir, "c.jsonl", &omp_session(&[("user", json!(content)), ("assistant", json!([image(&png_header(10, 10), "image/png")]))]));
    assert_eq!(carried_count(&CLAUDE, &j.messages()), 3, "the oversize side is withheld, the reply's image is not a user image");
}

// ---- the round trips ----

#[test]
fn omp_to_claude_to_opencode_and_back_keeps_every_image_byte_for_byte_and_in_place() {
    let dir = sandbox("roundtrip-claude-opencode");
    let j = journal_from_lines(&dir, "omp.jsonl", &omp_session(&sample_messages()));
    let want = user_shapes(&j);
    assert_eq!(images_of(&want).len(), 3);

    let claude_lines = claude::to_lines(
        &j.window(),
        &claude::Options { session_id: "00000000-0000-4000-8000-000000000001", cwd: CWD, cli_version: "2.1.245", title: "rt" },
    );
    let (back_claude, stats) = from_claude::to_journal_lines(&(claude_lines.join("\n") + "\n")).unwrap();
    assert_eq!(stats.images, 3, "every image comes back from the Claude copy");
    let j_claude = journal_from_lines(&dir, "claude-back.jsonl", &back_claude);
    assert_eq!(user_shapes(&j_claude), want, "Claude: same turns, same images, same order");

    let bundle = opencode::to_bundle(
        &j_claude.window(),
        &opencode::Options { session_id: "ses_attachroundtrip0000000000", title: "rt", directory: CWD, cli_version: "1.18.16", include_thinking: true },
    );
    let export = from_opencode::parse_export(&serde_json::to_string(&bundle).unwrap()).unwrap();
    let (back_oc, stats) = from_opencode::to_journal_lines(&export).unwrap();
    assert_eq!((stats.images, stats.skipped_file_parts), (3, 0));
    let j_oc = journal_from_lines(&dir, "opencode-back.jsonl", &back_oc);
    assert_eq!(user_shapes(&j_oc), want, "OpenCode: same turns, same images, same order");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn omp_to_antigravity_and_back_keeps_every_image_byte_for_byte() {
    let dir = sandbox("roundtrip-agy");
    let j = journal_from_lines(&dir, "omp.jsonl", &omp_session(&sample_messages()));
    let want = images_of(&user_shapes(&j));
    let store = dir.join("antigravity-cli");
    let (db, _, notes) = to_antigravity::install(&j, &j.window(), &store, "conv-attach", "rt").unwrap();
    assert!(notes.is_empty(), "{notes:?}");
    let (lines, st) = from_antigravity::to_journal_lines(&db).unwrap();
    assert_eq!((st.images, st.missing_images), (3, 0));
    let back = journal_from_lines(&dir, "agy-back.jsonl", &lines);
    assert_eq!(images_of(&user_shapes(&back)), want, "Antigravity keeps the images, in order, byte for byte");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_image_position_survives_claude_and_opencode_among_text_turns() {
    let dir = sandbox("positions");
    let j = journal_from_lines(&dir, "omp.jsonl", &omp_session(&sample_messages()));
    let claude_lines = claude::to_lines(&j.window(), &claude::Options { session_id: "00000000-0000-4000-8000-000000000002", cwd: CWD, cli_version: "2.1.245", title: "" });
    let first_user: Value = serde_json::from_str(&claude_lines[0]).unwrap();
    let kinds: Vec<&str> = first_user["message"]["content"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["text", "image", "text", "image", "text"], "the Claude record keeps the text / image interleaving");
    let _ = fs::remove_dir_all(&dir);
}

// ---- what is not carried leaves a note ----

#[test]
fn an_oversize_image_is_replaced_by_a_note_in_every_target_and_the_copy_stays_valid() {
    let dir = sandbox("huge");
    let mut big = png_header(100, 100);
    big.resize(3_800_000, 0);
    let messages = [("user", json!([{"type": "text", "text": "see"}, image(&big, "image/png"), {"type": "text", "text": "after"}]))];
    let j = journal_from_lines(&dir, "omp.jsonl", &omp_session(&messages));

    let claude_lines = claude::to_lines(&j.window(), &claude::Options { session_id: "00000000-0000-4000-8000-000000000003", cwd: CWD, cli_version: "2.1.245", title: "" });
    let rec: Value = serde_json::from_str(&claude_lines[0]).unwrap();
    let blocks = rec["message"]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 3, "the image is replaced, its place is kept");
    assert_eq!(blocks[0]["text"], "see");
    assert_eq!(blocks[2]["text"], "after");
    let note = blocks[1]["text"].as_str().unwrap();
    assert!(note.starts_with(NOTE_PREFIX) && note.contains("over the 5 MB limit for one image in Claude"), "{note}");
    assert!(!claude_lines.join("\n").contains("\"type\":\"image\""), "no image block reaches the Claude copy");

    let bundle = opencode::to_bundle(&j.window(), &opencode::Options { session_id: "ses_attachhuge00000000000000", title: "", directory: CWD, cli_version: "1.18.16", include_thinking: true });
    let parts = bundle["messages"][0]["parts"].as_array().unwrap();
    let kinds: Vec<&str> = parts.iter().map(|p| p["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["text", "text", "text"], "the note takes the image's place among the parts");
    assert!(parts[1]["text"].as_str().unwrap().starts_with(NOTE_PREFIX), "the note is the middle part");
    assert_eq!(parts[2]["text"], "after");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_tool_result_image_leaves_a_note_in_the_result_for_claude_and_opencode() {
    let dir = sandbox("tool-image");
    let shot = png(4, 3, 11);
    let lines = vec![
        json!({"type": "session", "id": "omp-tool", "timestamp": "2026-10-01T10:00:00.000Z", "cwd": CWD}).to_string(),
        json!({"type": "message", "id": "u", "parentId": null, "timestamp": "2026-10-01T10:00:01.000Z", "message": {"role": "user", "content": [{"type": "text", "text": "look"}]}}).to_string(),
        json!({"type": "message", "id": "a", "parentId": "u", "timestamp": "2026-10-01T10:00:02.000Z", "message": {"role": "assistant", "provider": "anthropic", "model": "claude-sonnet-4-5", "content": [{"type": "toolCall", "id": "call_1", "name": "shot", "arguments": {}}]}}).to_string(),
        json!({"type": "message", "id": "r", "parentId": "a", "timestamp": "2026-10-01T10:00:03.000Z", "message": {"role": "toolResult", "toolCallId": "call_1", "toolName": "shot", "isError": false, "content": [{"type": "text", "text": "captured"}, image(&shot, "image/png")]}}).to_string(),
        json!({"type": "message", "id": "r2", "parentId": "r", "timestamp": "2026-10-01T10:00:04.000Z", "message": {"role": "assistant", "provider": "anthropic", "model": "claude-sonnet-4-5", "content": [{"type": "text", "text": "Done."}]}}).to_string(),
    ];
    let j = journal_from_lines(&dir, "omp.jsonl", &lines);
    let claude_lines = claude::to_lines(&j.window(), &claude::Options { session_id: "00000000-0000-4000-8000-000000000004", cwd: CWD, cli_version: "2.1.245", title: "" });
    let result = claude_lines
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|r| r["message"]["content"][0]["type"] == "tool_result")
        .unwrap();
    let text = result["message"]["content"][0]["content"].as_str().unwrap();
    assert!(text.starts_with("captured\n[attachment not carried: image (image/png, "), "{text}");
    assert!(text.ends_with("kept in the source session]"), "{text}");

    let bundle = opencode::to_bundle(&j.window(), &opencode::Options { session_id: "ses_attachtool000000000000000", title: "", directory: CWD, cli_version: "1.18.16", include_thinking: true });
    let tool = bundle["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap().clone())
        .find(|p| p["type"] == "tool")
        .unwrap();
    let out = tool["state"]["output"].as_str().unwrap();
    assert!(out.starts_with("captured\n[attachment not carried: image (image/png, "), "{out}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_claude_image_held_by_reference_leaves_a_note_and_never_the_url() {
    let raw = json!({"type": "user", "uuid": "u1", "parentUuid": null, "sessionId": "s", "timestamp": "2026-10-01T10:00:01.000Z", "cwd": CWD,
        "message": {"role": "user", "content": [
            {"type": "text", "text": "see"},
            {"type": "image", "source": {"type": "url", "url": "https://x.example/a/shot.png?sig=TOKEN"}},
            {"type": "image", "source": {"type": "file", "file_id": "file_1"}}]}})
        .to_string()
        + "\n";
    let (lines, stats) = from_claude::to_journal_lines(&raw).unwrap();
    assert_eq!((stats.images, stats.skipped_image_blocks), (0, 2));
    let rec: Value = serde_json::from_str(&lines[1]).unwrap();
    let c = &rec["message"]["content"];
    assert_eq!(c[0]["text"], "see");
    assert_eq!(c[1]["text"], "[attachment not carried: image \u{2014} an image by URL, which is not fetched; kept in the source session]");
    assert_eq!(c[2]["text"], "[attachment not carried: image \u{2014} an image by Files API id; kept in the source session]");
    assert!(!lines.join("\n").contains("TOKEN") && !lines.join("\n").contains("shot.png"), "a URL is never written");
}

#[test]
fn an_opencode_file_that_cannot_be_carried_leaves_a_note_in_its_place_and_is_counted() {
    let pdf = vec![7u8; 1_200];
    let export = json!({
        "info": {"id": "ses_files", "directory": CWD, "title": "files", "time": {"created": 1_790_000_000_000i64}},
        "messages": [{
            "info": {"id": "msg_u", "role": "user", "time": {"created": 1_790_000_000_000i64}},
            "parts": [
                {"type": "text", "text": "read these"},
                {"type": "file", "mime": "application/pdf", "filename": "report.pdf", "url": format!("data:application/pdf;base64,{}", b64(&pdf))},
                {"type": "file", "mime": "text/plain", "url": "file:///home/u/notes.txt"},
                {"type": "file", "mime": "text/csv", "url": "https://files.example/data.csv?token=SECRET"},
                {"type": "text", "text": "and this"}
            ]}]});
    let (lines, stats) = from_opencode::to_journal_lines(&export).unwrap();
    assert_eq!((stats.images, stats.skipped_file_parts), (0, 3));
    let rec: Value = serde_json::from_str(&lines[1]).unwrap();
    let c = rec["message"]["content"].as_array().unwrap();
    assert_eq!(c.len(), 5, "the notes keep their places between the text");
    assert_eq!(c[0]["text"], "read these");
    assert_eq!(c[1]["text"], "[attachment not carried: report.pdf (application/pdf, 1.2 KB) \u{2014} kept in the source session]");
    assert_eq!(c[2]["text"], "[attachment not carried: attachment (text/plain) \u{2014} kept in the source session]");
    assert_eq!(c[3]["text"], "[attachment not carried: attachment (text/csv) \u{2014} kept in the source session]");
    assert_eq!(c[4]["text"], "and this");
    assert!(!lines.join("\n").contains("SECRET") && !lines.join("\n").contains("notes.txt"), "a URL or path is never written");

    // The copies carry the same notes in the same place.
    let dir = sandbox("opencode-files");
    let j = journal_from_lines(&dir, "oc.jsonl", &lines);
    let bundle = opencode::to_bundle(&j.window(), &opencode::Options { session_id: "ses_attachfiles0000000000000", title: "", directory: CWD, cli_version: "1.18.16", include_thinking: true });
    let parts = bundle["messages"][0]["parts"].as_array().unwrap();
    let texts: Vec<&str> = parts.iter().map(|p| p["text"].as_str().unwrap()).collect();
    assert_eq!(texts, c.iter().map(|b| b["text"].as_str().unwrap()).collect::<Vec<_>>());
    let claude_lines = claude::to_lines(&j.window(), &claude::Options { session_id: "00000000-0000-4000-8000-000000000005", cwd: CWD, cli_version: "2.1.245", title: "" });
    assert!(claude_lines[0].contains("report.pdf (application/pdf, 1.2 KB)"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_claude_copy_withholds_only_the_large_images_once_it_holds_more_than_twenty() {
    let dir = sandbox("many");
    let small = image(&png_header(100, 100), "image/png");
    let large = || image(&png_header(2100, 100), "image/png");
    let opts = claude::Options { session_id: "00000000-0000-4000-8000-000000000006", cwd: CWD, cli_version: "2.1.245", title: "" };

    // Twenty large images are the limit itself: all carried.
    let twenty: Vec<Value> = (0..20).map(|_| large()).collect();
    let j = journal_from_lines(&dir, "twenty.jsonl", &omp_session(&[("user", json!(twenty))]));
    let rec: Value = serde_json::from_str(&claude::to_lines(&j.window(), &opts)[0]).unwrap();
    assert_eq!(rec["message"]["content"].as_array().unwrap().iter().filter(|b| b["type"] == "image").count(), 20);

    // Twenty-two images: the 2100 px ones are notes, the 100 px one is carried.
    let mut many: Vec<Value> = (0..21).map(|_| large()).collect();
    many.push(small);
    let j = journal_from_lines(&dir, "many.jsonl", &omp_session(&[("user", json!(many))]));
    let rec: Value = serde_json::from_str(&claude::to_lines(&j.window(), &opts)[0]).unwrap();
    let blocks = rec["message"]["content"].as_array().unwrap();
    let carried = blocks.iter().filter(|b| b["type"] == "image").count();
    let notes = blocks.iter().filter(|b| b["text"].as_str().is_some_and(|t| t.contains("more than 20 images"))).count();
    assert_eq!((carried, notes), (1, 21));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_claude_reply_image_and_a_blob_reference_leave_notes() {
    let dir = sandbox("reply-blob");
    let j = journal_from_lines(
        &dir,
        "x.jsonl",
        &omp_session(&[
            ("user", json!([{"type": "text", "text": "x"}, {"type": "image", "data": "blob:sha256:00", "mimeType": "image/png"}])),
            ("assistant", json!([{"type": "text", "text": "y"}, image(&png(2, 2, 1), "image/png")])),
        ]),
    );
    let lines = claude::to_lines(&j.window(), &claude::Options { session_id: "00000000-0000-4000-8000-000000000007", cwd: CWD, cli_version: "2.1.245", title: "" });
    let user: Value = serde_json::from_str(&lines[0]).unwrap();
    let u = user["message"]["content"].as_array().unwrap();
    assert!(u[1]["text"].as_str().unwrap().contains("the journal holds only a blob reference to it"), "{u:?}");
    let reply: Value = serde_json::from_str(&lines[1]).unwrap();
    let r = reply["message"]["content"].as_array().unwrap();
    assert_eq!(r[0]["text"], "y");
    assert!(r[1]["text"].as_str().unwrap().starts_with("[attachment not carried: image (image/png, "), "{r:?}");
    assert!(r[1]["text"].as_str().unwrap().contains("an image in a reply"), "{r:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_antigravity_image_whose_base64_does_not_decode_leaves_a_note() {
    let dir = sandbox("agy-bad-b64");
    let j = journal_from_lines(&dir, "omp.jsonl", &omp_session(&[("user", json!([{"type": "text", "text": "x"}, {"type": "image", "data": "!!!!", "mimeType": "image/png"}]))]));
    let store = dir.join("antigravity-cli");
    let (db, s, _) = to_antigravity::install(&j, &j.window(), &store, "conv-bad-b64", "t").unwrap();
    assert_eq!(s.dropped_blocks, 1);
    let (lines, st) = from_antigravity::to_journal_lines(&db).unwrap();
    assert_eq!(st.images, 0);
    let rec: Value = serde_json::from_str(&lines[1]).unwrap();
    let text = rec["message"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("x\n[attachment not carried: image (image/png, 3 B) \u{2014} its base64 data does not decode"), "{text}");
    let _ = fs::remove_dir_all(&dir);
}
