//! Images the phone pastes or attaches, staged on this machine so the app can insert the file's path
//! into the prompt. Client side: `mobile/src/session/mobile-clipboard-image.ts`.
//!
//! Two shapes of call. `saveImageAsTempFile` carries a whole image in one call. Otherwise
//! `startImageUpload` opens an upload, `appendImageUploadChunk` sends the base64 text in order, and
//! `commitImageUpload` closes it. A chunk's `offset` counts base64 characters, not bytes.
//!
//! Chunks are decoded as they arrive into `<id>.part` under `<bridge dir>/uploads/<conversation>/`.
//! Commit checks the magic bytes, renames the file to `<id>.<ext>` and returns its absolute path.
//! Any refusal removes the partial file and forgets the upload. An upload idle for five minutes
//! expires, and at most eight are in progress at once, as the desktop runtime does.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine;
use parking_lot::Mutex;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{json, Value};

use super::rpc::{Ctx, Failure};
use super::{ensure_owner_dir, terminals};

/// The app's limit on the base64 text of one image (`CLIPBOARD_IMAGE_MAX_BASE64_CHARS`).
const MAX_BASE64_CHARS: u64 = 24 * 1024 * 1024;
/// The largest decoded image this bridge stages.
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// The largest chunk the app sends (`CLIPBOARD_IMAGE_UPLOAD_CHUNK_BASE64_CHARS`).
const MAX_CHUNK_BASE64_CHARS: usize = 512 * 1024;
/// Uploads in progress at once (`CLIPBOARD_IMAGE_UPLOAD_MAX_CONCURRENT`).
const MAX_CONCURRENT: usize = 8;
/// An upload idle this long is discarded (`CLIPBOARD_IMAGE_UPLOAD_TTL_MS`).
const IDLE_TTL: Duration = Duration::from_secs(5 * 60);
/// Leading decoded bytes that identify the format. The longest signature, WEBP's, is 12 bytes.
const SNIFF_LEN: usize = 12;

// The app matches TOO_LARGE verbatim to show its own message (use-mobile-native-chat-image-upload.ts).
const TOO_LARGE: &str = "Clipboard image is too large";
const NOT_BASE64: &str = "Clipboard image content must be base64";
const UNSUPPORTED: &str = "Clipboard image is not a PNG, JPEG, GIF or WEBP image";

/// Uploads in progress, by upload id. One instance is shared by every connection through `Ctx`.
#[derive(Clone, Default)]
pub struct Store {
    inner: Arc<Mutex<HashMap<String, Upload>>>,
}

/// One upload in progress: its partial file and what has arrived so far.
struct Upload {
    id: String,
    /// The conversation's staging directory, where the committed image goes.
    dir: PathBuf,
    /// `<id>.part` while chunks arrive; renamed to `<id>.<ext>` at commit.
    part: PathBuf,
    file: std::fs::File,
    /// The base64 length the app announced, if it did.
    expected: Option<u64>,
    /// Base64 characters received so far: the offset the next chunk must carry.
    chars: u64,
    /// Decoded bytes written so far.
    bytes: u64,
    /// The first `SNIFF_LEN` decoded bytes.
    head: Vec<u8>,
    text: Base64Text,
    expires: Instant,
}

/// Decodes base64 text that arrives in pieces. A piece may end inside a four-character group; the
/// leftover characters wait for the next piece. `=` padding ends the text.
#[derive(Default)]
struct Base64Text {
    pending: Vec<u8>,
    padded: bool,
}

impl Base64Text {
    fn push(&mut self, text: &str) -> Result<Vec<u8>, Failure> {
        let body = text.trim_end_matches('=');
        if self.padded && !body.is_empty() {
            return Err(bad(NOT_BASE64));
        }
        self.padded |= body.len() != text.len();
        let mut joined = std::mem::take(&mut self.pending);
        joined.extend_from_slice(body.as_bytes());
        let whole = joined.len() - joined.len() % 4;
        let bytes = STANDARD_NO_PAD.decode(&joined[..whole]).map_err(|_| bad(NOT_BASE64))?;
        self.pending = joined.split_off(whole);
        Ok(bytes)
    }

    /// The text is complete: what is pending must decode on its own.
    fn end(self) -> Result<Vec<u8>, Failure> {
        STANDARD_NO_PAD.decode(&self.pending).map_err(|_| bad(NOT_BASE64))
    }
}

impl Upload {
    /// Apply the chunk the app sent at `offset`.
    fn append(&mut self, offset: u64, text: &str) -> Result<(), Failure> {
        if offset != self.chars {
            return Err(bad("Clipboard image chunk offset is out of order"));
        }
        if text.len() > MAX_CHUNK_BASE64_CHARS {
            return Err(bad("Clipboard image chunk is too large"));
        }
        self.chars += text.len() as u64;
        if self.expected.is_some_and(|expected| self.chars > expected) {
            return Err(bad("Clipboard image upload exceeded expected size"));
        }
        let decoded = self.text.push(text)?;
        self.accept(&decoded)
    }

    /// Close the upload: the announced length must have arrived and the image must be a supported
    /// format. Returns the committed path.
    fn finish(&mut self) -> Result<PathBuf, Failure> {
        if self.expected.is_some_and(|expected| expected != self.chars) {
            return Err(bad("Clipboard image upload is incomplete"));
        }
        let rest = std::mem::take(&mut self.text).end()?;
        self.accept(&rest)?;
        let ext = image_extension(&self.head).ok_or_else(|| bad(UNSUPPORTED))?;
        let path = self.dir.join(format!("{}.{ext}", self.id));
        std::fs::rename(&self.part, &path)
            .map_err(|e| unavailable(format!("cannot save {}: {e}", path.display())))?;
        Ok(path)
    }

    /// Append decoded bytes to the partial file. The format is decided once the head is full, so a
    /// non-image is refused before the rest of it is sent.
    fn accept(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        if self.bytes + bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(bad(TOO_LARGE));
        }
        self.file
            .write_all(bytes)
            .map_err(|e| unavailable(format!("cannot write {}: {e}", self.part.display())))?;
        self.bytes += bytes.len() as u64;
        let take = (SNIFF_LEN - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..take]);
        if self.head.len() == SNIFF_LEN && image_extension(&self.head).is_none() {
            return Err(bad(UNSUPPORTED));
        }
        Ok(())
    }

    /// Remove the partial file. The upload is gone once this returns.
    fn discard(self) {
        let Upload { file, part, .. } = self;
        drop(file);
        let _ = std::fs::remove_file(part);
    }
}

/// `clipboard.startImageUpload`: `{uploadId}`.
pub fn start(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    start_in(&ctx.uploads, &ctx.dir, params)
}

/// `clipboard.appendImageUploadChunk`: `{ok: true}`.
pub fn append(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    append_in(&ctx.uploads, params)
}

/// `clipboard.commitImageUpload`: the image's absolute path, as a bare string.
pub fn commit(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    commit_in(&ctx.uploads, params)
}

/// `clipboard.abortImageUpload`: `{ok: true}`.
pub fn abort(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    abort_in(&ctx.uploads, params)
}

/// `clipboard.saveImageAsTempFile`: the image's absolute path, as a bare string.
pub fn save_temp_file(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    save_temp_file_in(&ctx.dir, params)
}

fn start_in(store: &Store, state_dir: &Path, params: &Value) -> Result<Value, Failure> {
    let expected = match params.get("expectedBase64Length") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| bad("expectedBase64Length must be a non-negative integer"))?,
        ),
    };
    if expected.is_some_and(|expected| expected > MAX_BASE64_CHARS) {
        return Err(bad(TOO_LARGE));
    }
    let mut uploads = store.inner.lock();
    prune(&mut uploads, Instant::now());
    if uploads.len() >= MAX_CONCURRENT {
        return Err(bad("Too many clipboard image uploads are in progress"));
    }
    let dir = staging_dir(state_dir, params)?;
    let id = new_id()?;
    let part = dir.join(format!("{id}.part"));
    let file = create_owner_only(&part)
        .map_err(|e| unavailable(format!("cannot create {}: {e}", part.display())))?;
    uploads.insert(
        id.clone(),
        Upload {
            id: id.clone(),
            dir,
            part,
            file,
            expected,
            chars: 0,
            bytes: 0,
            head: Vec::with_capacity(SNIFF_LEN),
            text: Base64Text::default(),
            expires: Instant::now() + IDLE_TTL,
        },
    );
    Ok(json!({ "uploadId": id }))
}

fn append_in(store: &Store, params: &Value) -> Result<Value, Failure> {
    let id = upload_id(params)?;
    let offset = params["offset"]
        .as_u64()
        .ok_or_else(|| bad("offset must be a non-negative integer"))?;
    let text = params["contentBase64"]
        .as_str()
        .ok_or_else(|| bad("Missing image content"))?;
    let mut uploads = store.inner.lock();
    prune(&mut uploads, Instant::now());
    let upload = uploads.get_mut(&id).ok_or_else(not_found)?;
    upload.expires = Instant::now() + IDLE_TTL;
    match upload.append(offset, text) {
        Ok(()) => Ok(json!({ "ok": true })),
        Err(failure) => {
            forget(&mut uploads, &id);
            Err(failure)
        }
    }
}

fn commit_in(store: &Store, params: &Value) -> Result<Value, Failure> {
    let id = upload_id(params)?;
    let mut uploads = store.inner.lock();
    prune(&mut uploads, Instant::now());
    let upload = uploads.get_mut(&id).ok_or_else(not_found)?;
    match upload.finish() {
        Ok(path) => {
            uploads.remove(&id);
            Ok(Value::String(path.to_string_lossy().into_owned()))
        }
        Err(failure) => {
            forget(&mut uploads, &id);
            Err(failure)
        }
    }
}

fn abort_in(store: &Store, params: &Value) -> Result<Value, Failure> {
    let id = upload_id(params)?;
    let mut uploads = store.inner.lock();
    prune(&mut uploads, Instant::now());
    let upload = uploads.remove(&id).ok_or_else(not_found)?;
    upload.discard();
    Ok(json!({ "ok": true }))
}

fn save_temp_file_in(state_dir: &Path, params: &Value) -> Result<Value, Failure> {
    let text = params["contentBase64"]
        .as_str()
        .ok_or_else(|| bad("Missing image content"))?;
    if text.len() as u64 > MAX_BASE64_CHARS {
        return Err(bad(TOO_LARGE));
    }
    let mut decoder = Base64Text::default();
    let mut bytes = decoder.push(text)?;
    bytes.extend(decoder.end()?);
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(bad(TOO_LARGE));
    }
    let ext = image_extension(&bytes).ok_or_else(|| bad(UNSUPPORTED))?;
    let dir = staging_dir(state_dir, params)?;
    let path = dir.join(format!("{}.{ext}", new_id()?));
    let written = create_owner_only(&path).and_then(|mut file| file.write_all(&bytes));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&path);
        return Err(unavailable(format!("cannot save {}: {e}", path.display())));
    }
    Ok(Value::String(path.to_string_lossy().into_owned()))
}

fn upload_id(params: &Value) -> Result<String, Failure> {
    params["uploadId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| bad("missing upload id"))
}

fn not_found() -> Failure {
    bad("Clipboard image upload was not found")
}

fn bad(message: impl Into<String>) -> Failure {
    ("bad_request", message.into())
}

fn unavailable(message: impl Into<String>) -> Failure {
    ("unavailable", message.into())
}

/// Forget an upload and remove its partial file.
fn forget(uploads: &mut HashMap<String, Upload>, id: &str) {
    if let Some(upload) = uploads.remove(id) {
        upload.discard();
    }
}

/// Drop every upload whose idle time has run out, removing its partial file.
fn prune(uploads: &mut HashMap<String, Upload>, now: Instant) {
    let expired: Vec<String> = uploads
        .iter()
        .filter(|(_, upload)| upload.expires <= now)
        .map(|(id, _)| id.clone())
        .collect();
    for id in &expired {
        forget(uploads, id);
    }
}

/// `<state_dir>/uploads/<conversation>/`, owner-only and created if missing.
fn staging_dir(state_dir: &Path, params: &Value) -> Result<PathBuf, Failure> {
    let root = staging_root(state_dir);
    let dir = root.join(conversation_dir_name(params));
    ensure_owner_dir(&root).map_err(|m| unavailable(m))?;
    ensure_owner_dir(&dir).map_err(|m| unavailable(m))?;
    Ok(dir)
}

/// `<state_dir>/uploads`: the root every staged image sits under, one directory per conversation.
fn staging_root(state_dir: &Path) -> PathBuf {
    state_dir.join("uploads")
}

/// The staging directory's name for the conversation a request names: its worktree id, with every
/// character outside `[A-Za-z0-9._-]` replaced by `_`. A leading `.` is replaced too, so no name is
/// `.` or `..`. The app's clipboard params name no worktree, so those requests share `_unscoped`.
fn conversation_dir_name(params: &Value) -> String {
    let worktree = params["worktree"]
        .as_str()
        .map(terminals::worktree_from_selector)
        .unwrap_or("");
    if worktree.is_empty() {
        return "_unscoped".to_string();
    }
    let mut name: String = worktree
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.starts_with('.') {
        name.insert(0, '_');
    }
    name
}

/// 128 random bits as lowercase hex: unguessable, and safe in a file name.
fn new_id() -> Result<String, Failure> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| unavailable("no system randomness for an upload id"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Create `path` new, readable only by its owner from the moment it exists.
fn create_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// The extension of a supported image, from its leading bytes; `None` for anything else.
fn image_extension(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("gif")
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP".as_slice()) {
        Some("webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    /// A fresh state directory for one test, under the system temp dir.
    fn state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sesync-uploads-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Where requests without a worktree stage.
    fn unscoped(state: &Path) -> PathBuf {
        state.join("uploads").join("_unscoped")
    }

    /// A PNG: its signature, then filler up to `len` bytes.
    fn png(len: usize) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(len, 7);
        bytes
    }

    fn encoded(bytes: &[u8]) -> String {
        STANDARD.encode(bytes)
    }

    /// Open an upload with the given announced length; returns its id.
    fn begin(store: &Store, state: &Path, expected: Option<u64>) -> String {
        let reply = start_in(store, state, &json!({ "expectedBase64Length": expected })).unwrap();
        reply["uploadId"].as_str().unwrap().to_string()
    }

    fn send(store: &Store, id: &str, offset: usize, text: &str) -> Result<Value, Failure> {
        append_in(store, &json!({ "uploadId": id, "offset": offset, "contentBase64": text }))
    }

    fn commit_id(store: &Store, id: &str) -> Result<Value, Failure> {
        commit_in(store, &json!({ "uploadId": id }))
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn base64_split_inside_a_group_decodes_exactly() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let text = encoded(&data);
        let mut decoder = Base64Text::default();
        let mut out = Vec::new();
        for piece in text.as_bytes().chunks(5) {
            out.extend(decoder.push(std::str::from_utf8(piece).unwrap()).unwrap());
        }
        out.extend(decoder.end().unwrap());
        assert_eq!(out, data);
    }

    #[test]
    fn padding_must_end_the_base64_text() {
        let mut whole = Base64Text::default();
        assert!(whole.push("QQ==").is_ok());
        let mut after = Base64Text::default();
        after.push("QQ==").unwrap();
        assert!(after.push("QQ").is_err());
        // Padding split across pieces is still padding.
        let mut split = Base64Text::default();
        split.push("QQ=").unwrap();
        split.push("=").unwrap();
        assert_eq!(split.end().unwrap(), b"A".to_vec());
    }

    #[test]
    fn chunks_past_the_decoded_cap_are_refused_and_the_partial_file_removed() {
        let state = state_dir("cap");
        let store = Store::default();
        let text = encoded(&png(MAX_IMAGE_BYTES as usize + 1));
        let id = begin(&store, &state, None);
        let mut offset = 0;
        let mut refused = None;
        for chunk in text.as_bytes().chunks(MAX_CHUNK_BASE64_CHARS) {
            let piece = std::str::from_utf8(chunk).unwrap();
            match send(&store, &id, offset, piece) {
                Ok(_) => offset += piece.len(),
                Err(failure) => {
                    refused = Some(failure);
                    break;
                }
            }
        }
        let (code, message) = refused.expect("the cap must refuse the image");
        assert_eq!(code, "bad_request");
        assert_eq!(message, TOO_LARGE);
        assert!(offset > 0, "the cap is crossed by a later chunk, not the first");
        assert_eq!(std::fs::read_dir(unscoped(&state)).unwrap().count(), 0);
        assert!(send(&store, &id, offset, "AAAA").is_err(), "the upload is forgotten");
    }

    #[test]
    fn an_announced_length_above_the_base64_limit_is_refused_at_start() {
        let state = state_dir("announce");
        let store = Store::default();
        let too_long = MAX_BASE64_CHARS + 1;
        let failure = start_in(&store, &state, &json!({ "expectedBase64Length": too_long })).unwrap_err();
        assert_eq!(failure, ("bad_request", TOO_LARGE.to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn staging_is_owner_only_and_so_are_the_files() {
        let state = state_dir("perms");
        let store = Store::default();
        let bytes = png(64);
        let id = begin(&store, &state, Some(encoded(&bytes).len() as u64));
        assert_eq!(mode(&state.join("uploads")), 0o700);
        assert_eq!(mode(&unscoped(&state)), 0o700);
        assert_eq!(mode(&unscoped(&state).join(format!("{id}.part"))), 0o600);
        send(&store, &id, 0, &encoded(&bytes)).unwrap();
        let path = PathBuf::from(commit_id(&store, &id).unwrap().as_str().unwrap());
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn commit_returns_the_absolute_path_of_the_named_image() {
        let state = state_dir("commit");
        let store = Store::default();
        let bytes = png(64);
        let id = begin(&store, &state, Some(encoded(&bytes).len() as u64));
        send(&store, &id, 0, &encoded(&bytes)).unwrap();
        let reply = commit_id(&store, &id).unwrap();
        let path = PathBuf::from(reply.as_str().unwrap());
        assert!(path.is_absolute());
        assert_eq!(path, unscoped(&state).join(format!("{id}.png")));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(commit_id(&store, &id).is_err(), "a committed upload is forgotten");
    }

    #[test]
    fn non_images_are_refused_and_the_partial_file_removed() {
        let state = state_dir("magic");
        let store = Store::default();
        let id = begin(&store, &state, None);
        let failure = send(&store, &id, 0, &encoded(b"not an image, plainly text")).unwrap_err();
        assert_eq!(failure, ("bad_request", UNSUPPORTED.to_string()));
        assert_eq!(std::fs::read_dir(unscoped(&state)).unwrap().count(), 0);
        assert!(commit_id(&store, &id).is_err());

        let short = begin(&store, &state, None);
        send(&store, &short, 0, &encoded(b"hi")).unwrap();
        let failure = commit_id(&store, &short).unwrap_err();
        assert_eq!(failure, ("bad_request", UNSUPPORTED.to_string()));
        assert_eq!(std::fs::read_dir(unscoped(&state)).unwrap().count(), 0);
    }

    #[test]
    fn save_temp_file_refuses_a_non_image_and_writes_nothing() {
        let state = state_dir("save-refuse");
        let failure = save_temp_file_in(&state, &json!({ "contentBase64": encoded(b"hello there") }))
            .unwrap_err();
        assert_eq!(failure, ("bad_request", UNSUPPORTED.to_string()));
        assert!(!unscoped(&state).exists(), "no staging directory for a refused image");
    }

    #[cfg(unix)]
    #[test]
    fn save_temp_file_writes_one_owner_only_image() {
        let state = state_dir("save");
        let bytes = png(40);
        let reply = save_temp_file_in(&state, &json!({ "contentBase64": encoded(&bytes) })).unwrap();
        let path = PathBuf::from(reply.as_str().unwrap());
        assert!(path.is_absolute());
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
        assert_eq!(path.parent().unwrap(), unscoped(&state).as_path());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn abort_removes_the_partial_file_and_forgets_the_id() {
        let state = state_dir("abort");
        let store = Store::default();
        let id = begin(&store, &state, None);
        send(&store, &id, 0, &encoded(&png(64))).unwrap();
        let part = unscoped(&state).join(format!("{id}.part"));
        assert!(part.exists());
        assert_eq!(abort_in(&store, &json!({ "uploadId": id })).unwrap(), json!({ "ok": true }));
        assert!(!part.exists());
        assert_eq!(abort_in(&store, &json!({ "uploadId": id })).unwrap_err().0, "bad_request");
        assert_eq!(send(&store, &id, 0, "AAAA").unwrap_err().0, "bad_request");
    }

    #[test]
    fn a_chunk_must_start_where_the_last_one_ended() {
        let state = state_dir("offset");
        let store = Store::default();
        let text = encoded(&png(64));
        let id = begin(&store, &state, Some(text.len() as u64));
        let (first, rest) = text.split_at(8);
        send(&store, &id, 0, first).unwrap();
        let failure = send(&store, &id, 99, rest).unwrap_err();
        assert_eq!(failure.0, "bad_request");
        assert!(failure.1.contains("out of order"));
    }

    #[test]
    fn the_announced_length_must_arrive_and_not_be_exceeded() {
        let state = state_dir("length");
        let store = Store::default();
        let short = begin(&store, &state, Some(20));
        send(&store, &short, 0, "AAAA").unwrap();
        let failure = commit_id(&store, &short).unwrap_err();
        assert_eq!(failure.0, "bad_request");
        assert!(failure.1.contains("incomplete"));

        let long = begin(&store, &state, Some(4));
        let failure = send(&store, &long, 0, "AAAAAAAA").unwrap_err();
        assert!(failure.1.contains("exceeded expected size"));
    }

    #[test]
    fn an_idle_upload_expires_and_its_partial_file_goes() {
        let state = state_dir("ttl");
        let store = Store::default();
        let id = begin(&store, &state, None);
        let part = unscoped(&state).join(format!("{id}.part"));
        assert!(part.exists());
        let later = Instant::now() + IDLE_TTL + Duration::from_secs(1);
        prune(&mut store.inner.lock(), later);
        assert!(!part.exists());
        assert!(store.inner.lock().is_empty());
    }

    #[test]
    fn at_most_eight_uploads_are_in_progress() {
        let state = state_dir("concurrent");
        let store = Store::default();
        for _ in 0..MAX_CONCURRENT {
            begin(&store, &state, None);
        }
        let failure = start_in(&store, &state, &json!({})).unwrap_err();
        assert_eq!(failure.0, "bad_request");
    }

    #[test]
    fn conversation_names_are_sanitized_and_never_escape_the_root() {
        assert_eq!(conversation_dir_name(&json!({ "worktree": "id:repo/a b::x" })), "repo_a_b__x");
        assert_eq!(conversation_dir_name(&json!({ "worktree": "id:.." })), "_..");
        assert_eq!(conversation_dir_name(&json!({ "worktree": "plain-id.1_x" })), "plain-id.1_x");
        assert_eq!(conversation_dir_name(&json!({ "worktree": "id:" })), "_unscoped");
        assert_eq!(conversation_dir_name(&json!({})), "_unscoped");
    }

    #[test]
    fn each_supported_format_is_recognised_by_its_magic_bytes() {
        assert_eq!(image_extension(b"\x89PNG\r\n\x1a\nrest"), Some("png"));
        assert_eq!(image_extension(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(image_extension(b"GIF89a...."), Some("gif"));
        assert_eq!(image_extension(b"GIF87a...."), Some("gif"));
        assert_eq!(image_extension(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("webp"));
        assert_eq!(image_extension(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
        assert_eq!(image_extension(b"GIF8"), None);
    }
}
