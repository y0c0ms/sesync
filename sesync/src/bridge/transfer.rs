//! Files moved over the bridge, in both directions, for any paired client: the phone and a `sesync machine`
//! client such as the work PC.
//!
//! **Upload (client to this machine).** `files.startUpload {name, expectedBase64Length?}` opens an upload,
//! `files.appendUploadChunk {uploadId, offset, contentBase64}` sends the base64 text in order, and
//! `files.commitUpload {uploadId}` closes it and returns `{path, name, bytes}`. The file lands in the inbox
//! folder (see [`inbox_dir`]) under a sanitised name; an existing file is never overwritten, a counter is added
//! to the name instead. Chunks are decoded as they arrive into `.<id>.part` in the inbox and renamed at commit.
//!
//! **Download (this machine to the client).** `files.startDownload {path}` names any file or directory on this
//! host. A directory is zipped to a staging file first. It returns `{downloadId, name, bytes}`;
//! `files.readDownloadChunk {downloadId, offset}` returns the next piece, and `files.endDownload {downloadId}`
//! forgets it (and removes a staged zip). Reach is the same as for images: a paired client already types into
//! shells on this host, so a file adds no reach (BRIDGE.md, "Security").
//!
//! **Where the inbox is.** `<sesync config dir>/transfer.json`, key `inbox`. The TUI's Settings view writes it;
//! the bridge reads it on every call, so a change applies without a restart. Absent, it is `~/sesync-inbox`.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use parking_lot::Mutex;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{json, Value};

use super::rpc::{Ctx, Failure};
use super::ensure_owner_dir;

/// The largest file this bridge accepts or sends: 2 GiB.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// The largest chunk a client sends, in base64 characters (the same as images).
const MAX_CHUNK_BASE64_CHARS: usize = 512 * 1024;
/// The bytes `files.readDownloadChunk` returns at most: 384 KiB, so its base64 text stays under 512 Ki characters.
const DOWNLOAD_CHUNK_BYTES: usize = 384 * 1024;
/// Transfers in progress at once, each way.
const MAX_CONCURRENT: usize = 8;
/// A transfer idle this long is discarded.
const IDLE_TTL: Duration = Duration::from_secs(5 * 60);
/// Where files arrive when nothing is configured.
const DEFAULT_INBOX: &str = "sesync-inbox";
/// The settings file's name, in the sesync config directory.
pub const SETTINGS_FILE: &str = "transfer.json";

const NOT_BASE64: &str = "File content must be base64";

/// Transfers in progress, by id. One instance is shared by every connection through `Ctx`.
#[derive(Clone, Default)]
pub struct Store {
    uploads: Arc<Mutex<HashMap<String, Upload>>>,
    downloads: Arc<Mutex<HashMap<String, Download>>>,
}

struct Upload {
    /// The inbox folder the file lands in, resolved when the upload started.
    dir: PathBuf,
    /// The sanitised name the client asked for.
    name: String,
    part: PathBuf,
    file: std::fs::File,
    expected: Option<u64>,
    /// Base64 characters received so far: the offset the next chunk must carry.
    chars: u64,
    bytes: u64,
    text: Base64Text,
    expires: Instant,
}

struct Download {
    file: std::fs::File,
    size: u64,
    /// A zip this bridge made for a directory; removed when the download ends.
    staged: Option<PathBuf>,
    expires: Instant,
}

/// Decodes base64 text that arrives in pieces; a piece may end inside a four-character group.
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

    fn end(self) -> Result<Vec<u8>, Failure> {
        STANDARD_NO_PAD.decode(&self.pending).map_err(|_| bad(NOT_BASE64))
    }
}

impl Upload {
    fn append(&mut self, offset: u64, text: &str) -> Result<(), Failure> {
        if offset != self.chars {
            return Err(bad("File chunk offset is out of order"));
        }
        if text.len() > MAX_CHUNK_BASE64_CHARS {
            return Err(bad("File chunk is too large"));
        }
        self.chars += text.len() as u64;
        if self.expected.is_some_and(|expected| self.chars > expected) {
            return Err(bad("File upload exceeded the announced size"));
        }
        let decoded = self.text.push(text)?;
        self.write(&decoded)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        if self.bytes + bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(bad("File is too large"));
        }
        self.file
            .write_all(bytes)
            .map_err(|e| unavailable(format!("cannot write {}: {e}", self.part.display())))?;
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    /// Close the upload and move the file to its final, non-colliding name.
    fn finish(&mut self) -> Result<(PathBuf, u64), Failure> {
        if self.expected.is_some_and(|expected| expected != self.chars) {
            return Err(bad("File upload is incomplete"));
        }
        let rest = std::mem::take(&mut self.text).end()?;
        self.write(&rest)?;
        self.file
            .sync_all()
            .map_err(|e| unavailable(format!("cannot write {}: {e}", self.part.display())))?;
        let path = claim_name(&self.dir, &self.name, &self.part)?;
        Ok((path, self.bytes))
    }

    fn discard(self) {
        let Upload { file, part, .. } = self;
        drop(file);
        let _ = std::fs::remove_file(part);
    }
}

impl Download {
    fn discard(self) {
        let Download { file, staged, .. } = self;
        drop(file);
        if let Some(path) = staged {
            let _ = std::fs::remove_file(path);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Settings

/// The sesync config directory the bridge state directory sits in (`<config>/bridge`).
fn config_dir(state_dir: &Path) -> PathBuf {
    state_dir.parent().map(Path::to_path_buf).unwrap_or_else(|| state_dir.to_path_buf())
}

/// The folder `<config>/transfer.json` names, if it names one.
fn configured_inbox(state_dir: &Path) -> Option<String> {
    std::fs::read_to_string(config_dir(state_dir).join(SETTINGS_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.get("inbox").and_then(Value::as_str).map(str::to_string))
        .filter(|s| !s.trim().is_empty())
}

/// Whether a folder was chosen, as opposed to the default.
pub fn is_configured(state_dir: &Path) -> bool {
    configured_inbox(state_dir).is_some()
}

/// The folder files from a client arrive in: `inbox` in `<config>/transfer.json`, a leading `~/` meaning the home
/// directory, or `~/sesync-inbox`. Read on every call, so the TUI's change applies at once.
pub fn inbox_dir(state_dir: &Path) -> PathBuf {
    expand(configured_inbox(state_dir).as_deref().unwrap_or(&format!("~/{DEFAULT_INBOX}")))
}

/// Expand a leading `~` to the home directory.
pub fn expand(path: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    if path == "~" {
        return home().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = home() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// Save the inbox folder to `<config>/transfer.json`. The folder must be creatable and writable.
pub fn set_inbox(config: &Path, folder: &str) -> Result<PathBuf, String> {
    let folder = folder.trim();
    if folder.is_empty() {
        return Err("the folder is empty".into());
    }
    let dir = expand(folder);
    if !dir.is_absolute() {
        return Err(format!("{} is not an absolute path", dir.display()));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let probe = dir.join(format!(".sesync-write-test-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(|e| format!("{} is not writable: {e}", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    let value = json!({ "inbox": folder });
    super::write_owner_only(&config.join(SETTINGS_FILE), &serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------------------------------------
// Names

/// A file name a client sent, made safe to create in the inbox: only the last path component, no control
/// characters, no leading dots, never empty, at most 200 bytes.
pub fn sanitize_name(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let mut name: String = last
        .chars()
        .map(|c| if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    name = name.trim().trim_start_matches('.').trim().to_string();
    if name.len() > 200 {
        let mut cut = 200;
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name.truncate(cut);
    }
    if name.is_empty() {
        "file".to_string()
    } else {
        name
    }
}

/// Move `part` to `<dir>/<name>`, or `<dir>/<stem> (n).<ext>` when that exists. The move cannot replace a file:
/// the destination is reserved with `create_new` first.
fn claim_name(dir: &Path, name: &str, part: &Path) -> Result<PathBuf, Failure> {
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    for n in 0..10_000u32 {
        let candidate = if n == 0 { name.to_string() } else { format!("{stem} ({n}){ext}") };
        let target = dir.join(&candidate);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&target) {
            Ok(_) => {
                return std::fs::rename(part, &target).map(|_| target.clone()).map_err(|e| {
                    let _ = std::fs::remove_file(&target);
                    unavailable(format!("cannot save {}: {e}", target.display()))
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(unavailable(format!("cannot save {}: {e}", target.display()))),
        }
    }
    Err(unavailable(format!("too many files named {name} in {}", dir.display())))
}

// ---------------------------------------------------------------------------------------------------------
// RPC entry points

/// `files.inbox`: `{path}`, the folder files arrive in.
pub fn inbox(ctx: &Ctx) -> Result<Value, Failure> {
    Ok(json!({ "path": inbox_dir(&ctx.dir).to_string_lossy() }))
}

/// `files.startUpload`: `{uploadId}`.
pub fn start_upload(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    start_upload_in(&ctx.transfers, &ctx.dir, params)
}

/// `files.appendUploadChunk`: `{ok: true}`.
pub fn append_upload(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    append_upload_in(&ctx.transfers, params)
}

/// `files.commitUpload`: `{path, name, bytes}`.
pub fn commit_upload(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    commit_upload_in(&ctx.transfers, params)
}

/// `files.abortUpload`: `{ok: true}`.
pub fn abort_upload(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let id = id_of(params, "uploadId")?;
    let mut uploads = ctx.transfers.uploads.lock();
    prune_uploads(&mut uploads, Instant::now());
    uploads.remove(&id).ok_or_else(|| not_found("upload"))?.discard();
    Ok(json!({ "ok": true }))
}

/// `files.startDownload`: `{downloadId, name, bytes}`.
pub fn start_download(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    start_download_in(&ctx.transfers, &ctx.dir, params)
}

/// `files.readDownloadChunk`: `{offset, contentBase64, done}`.
pub fn read_download_chunk(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    read_download_chunk_in(&ctx.transfers, params)
}

/// `files.endDownload`: `{ok: true}`.
pub fn end_download(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let id = id_of(params, "downloadId")?;
    let mut downloads = ctx.transfers.downloads.lock();
    prune_downloads(&mut downloads, Instant::now());
    downloads.remove(&id).ok_or_else(|| not_found("download"))?.discard();
    Ok(json!({ "ok": true }))
}

fn start_upload_in(store: &Store, state_dir: &Path, params: &Value) -> Result<Value, Failure> {
    let name = sanitize_name(params["name"].as_str().ok_or_else(|| bad("missing file name"))?);
    let expected = match params.get("expectedBase64Length") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| bad("expectedBase64Length must be a non-negative integer"))?),
    };
    if expected.is_some_and(|e| e > MAX_FILE_BYTES.div_ceil(3) * 4) {
        return Err(bad("File is too large"));
    }
    let mut uploads = store.uploads.lock();
    prune_uploads(&mut uploads, Instant::now());
    if uploads.len() >= MAX_CONCURRENT {
        return Err(bad("Too many file uploads are in progress"));
    }
    let dir = inbox_dir(state_dir);
    ensure_owner_dir(&dir).map_err(unavailable)?;
    let id = new_id()?;
    let part = dir.join(format!(".{id}.part"));
    let file = create_owner_only(&part).map_err(|e| unavailable(format!("cannot create {}: {e}", part.display())))?;
    uploads.insert(
        id.clone(),
        Upload { dir, name, part, file, expected, chars: 0, bytes: 0, text: Base64Text::default(), expires: Instant::now() + IDLE_TTL },
    );
    Ok(json!({ "uploadId": id }))
}

fn append_upload_in(store: &Store, params: &Value) -> Result<Value, Failure> {
    let id = id_of(params, "uploadId")?;
    let offset = params["offset"].as_u64().ok_or_else(|| bad("offset must be a non-negative integer"))?;
    let text = params["contentBase64"].as_str().ok_or_else(|| bad("missing file content"))?;
    let mut uploads = store.uploads.lock();
    prune_uploads(&mut uploads, Instant::now());
    let upload = uploads.get_mut(&id).ok_or_else(|| not_found("upload"))?;
    upload.expires = Instant::now() + IDLE_TTL;
    match upload.append(offset, text) {
        Ok(()) => Ok(json!({ "ok": true })),
        Err(failure) => {
            forget_upload(&mut uploads, &id);
            Err(failure)
        }
    }
}

fn commit_upload_in(store: &Store, params: &Value) -> Result<Value, Failure> {
    let id = id_of(params, "uploadId")?;
    let mut uploads = store.uploads.lock();
    prune_uploads(&mut uploads, Instant::now());
    let upload = uploads.get_mut(&id).ok_or_else(|| not_found("upload"))?;
    match upload.finish() {
        Ok((path, bytes)) => {
            uploads.remove(&id);
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            Ok(json!({ "path": path.to_string_lossy(), "name": name, "bytes": bytes }))
        }
        Err(failure) => {
            forget_upload(&mut uploads, &id);
            Err(failure)
        }
    }
}

fn start_download_in(store: &Store, state_dir: &Path, params: &Value) -> Result<Value, Failure> {
    let raw = params["path"].as_str().filter(|p| !p.is_empty()).ok_or_else(|| bad("missing path"))?;
    if raw.contains('\0') {
        return Err(bad("the path contains a NUL byte"));
    }
    let path = std::fs::canonicalize(expand(raw)).map_err(|_| ("not_found", format!("{raw} was not found")))?;
    let meta = std::fs::metadata(&path).map_err(|e| unavailable(format!("cannot read {}: {e}", path.display())))?;
    let mut downloads = store.downloads.lock();
    prune_downloads(&mut downloads, Instant::now());
    if downloads.len() >= MAX_CONCURRENT {
        return Err(bad("Too many file downloads are in progress"));
    }
    let (file, name, staged) = if meta.is_dir() {
        let (zip_path, name) = zip_directory(state_dir, &path)?;
        let file = std::fs::File::open(&zip_path).map_err(|e| unavailable(format!("cannot open {}: {e}", zip_path.display())))?;
        (file, name, Some(zip_path))
    } else if meta.is_file() {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        let file = std::fs::File::open(&path).map_err(|e| unavailable(format!("cannot open {}: {e}", path.display())))?;
        (file, name, None)
    } else {
        return Err(bad("only a file or a directory can be sent"));
    };
    let size = file.metadata().map_err(|e| unavailable(e.to_string()))?.len();
    if size > MAX_FILE_BYTES {
        if let Some(staged) = &staged {
            let _ = std::fs::remove_file(staged);
        }
        return Err(("file_too_large", format!("{} is {size} bytes; the limit is {MAX_FILE_BYTES}", path.display())));
    }
    let id = new_id()?;
    downloads.insert(id.clone(), Download { file, size, staged, expires: Instant::now() + IDLE_TTL });
    Ok(json!({ "downloadId": id, "name": name, "bytes": size }))
}

fn read_download_chunk_in(store: &Store, params: &Value) -> Result<Value, Failure> {
    let id = id_of(params, "downloadId")?;
    let offset = params["offset"].as_u64().ok_or_else(|| bad("offset must be a non-negative integer"))?;
    let mut downloads = store.downloads.lock();
    prune_downloads(&mut downloads, Instant::now());
    let download = downloads.get_mut(&id).ok_or_else(|| not_found("download"))?;
    download.expires = Instant::now() + IDLE_TTL;
    if offset > download.size {
        return Err(bad("offset is past the end of the file"));
    }
    download.file.seek(SeekFrom::Start(offset)).map_err(|e| unavailable(e.to_string()))?;
    let want = DOWNLOAD_CHUNK_BYTES.min((download.size - offset) as usize);
    let mut buffer = vec![0u8; want];
    download.file.read_exact(&mut buffer).map_err(|e| unavailable(format!("cannot read the file: {e}")))?;
    let done = offset + want as u64 >= download.size;
    Ok(json!({ "offset": offset, "contentBase64": STANDARD.encode(&buffer), "done": done }))
}

/// Zip the directory `dir` into `<state>/transfer/<id>.zip`, returning that path and the zip's name
/// (`<directory name>.zip`). Symlinks are skipped, so a link cannot pull in a file from elsewhere.
fn zip_directory(state_dir: &Path, dir: &Path) -> Result<(PathBuf, String), Failure> {
    let staging = state_dir.join("transfer");
    ensure_owner_dir(&staging).map_err(unavailable)?;
    let base = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "folder".into());
    let zip_path = staging.join(format!("{}.zip", new_id()?));
    let file = create_owner_only(&zip_path).map_err(|e| unavailable(format!("cannot create {}: {e}", zip_path.display())))?;
    if let Err(e) = write_zip_of(file, dir, &base) {
        let _ = std::fs::remove_file(&zip_path);
        return Err(match e {
            ZipError::TooLarge(message) => ("file_too_large", message),
            ZipError::Failed(message) => unavailable(message),
        });
    }
    Ok((zip_path, format!("{base}.zip")))
}

/// Why a folder could not be zipped.
pub enum ZipError {
    /// The files add up to more than [`MAX_FILE_BYTES`].
    TooLarge(String),
    Failed(String),
}

impl std::fmt::Display for ZipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZipError::TooLarge(m) | ZipError::Failed(m) => f.write_str(m),
        }
    }
}

/// Write a zip of the folder `dir` to `file`, every entry under `<base>/`. Symlinks are skipped, so a link cannot
/// pull in a file from elsewhere. Shared by the bridge (a download) and `sesync machine send`.
pub fn write_zip_of(file: std::fs::File, dir: &Path, base: &str) -> Result<(), ZipError> {
    use zip::write::SimpleFileOptions;
    let failed = |e: &dyn std::fmt::Display| ZipError::Failed(e.to_string());
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).large_file(true);
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&current)
            .map_err(|e| ZipError::Failed(format!("cannot read {}: {e}", current.display())))?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let kind = entry.file_type().map_err(|e| failed(&e))?;
            if kind.is_symlink() {
                continue;
            }
            let relative = path.strip_prefix(dir).unwrap_or(&path);
            let name = format!("{base}/{}", relative.to_string_lossy().replace('\\', "/"));
            if kind.is_dir() {
                zip.add_directory(format!("{name}/"), options).map_err(|e| failed(&e))?;
                stack.push(path);
            } else if kind.is_file() {
                total += entry.metadata().map_err(|e| failed(&e))?.len();
                if total > MAX_FILE_BYTES {
                    return Err(ZipError::TooLarge(format!("{} holds more than {MAX_FILE_BYTES} bytes", dir.display())));
                }
                zip.start_file(name, options).map_err(|e| failed(&e))?;
                let mut source = std::fs::File::open(&path).map_err(|e| ZipError::Failed(format!("cannot read {}: {e}", path.display())))?;
                std::io::copy(&mut source, &mut zip).map_err(|e| ZipError::Failed(format!("cannot zip {}: {e}", path.display())))?;
            }
        }
    }
    zip.finish().map_err(|e| failed(&e))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Bookkeeping

fn id_of(params: &Value, key: &str) -> Result<String, Failure> {
    params[key]
        .as_str()
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_string)
        .ok_or_else(|| bad(format!("missing {key}")))
}

fn not_found(what: &str) -> Failure {
    ("not_found", format!("the file {what} was not found"))
}

fn bad(message: impl Into<String>) -> Failure {
    ("bad_request", message.into())
}

fn unavailable(message: impl Into<String>) -> Failure {
    ("unavailable", message.into())
}

fn forget_upload(uploads: &mut HashMap<String, Upload>, id: &str) {
    if let Some(upload) = uploads.remove(id) {
        upload.discard();
    }
}

fn prune_uploads(uploads: &mut HashMap<String, Upload>, now: Instant) {
    let expired: Vec<String> = uploads.iter().filter(|(_, u)| u.expires <= now).map(|(id, _)| id.clone()).collect();
    for id in &expired {
        forget_upload(uploads, id);
    }
}

fn prune_downloads(downloads: &mut HashMap<String, Download>, now: Instant) {
    let expired: Vec<String> = downloads.iter().filter(|(_, d)| d.expires <= now).map(|(id, _)| id.clone()).collect();
    for id in expired {
        if let Some(download) = downloads.remove(&id) {
            download.discard();
        }
    }
}

/// 128 random bits as lowercase hex: unguessable, and safe in a file name.
fn new_id() -> Result<String, Failure> {
    let mut bytes = [0u8; 16];
    SystemRandom::new().fill(&mut bytes).map_err(|_| unavailable("no system randomness for a transfer id"))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::testkit::{ctx_with, sandbox};

    /// A context whose config dir is `<root>/cfg` and whose inbox is `<root>/inbox`.
    fn setup(name: &str) -> (Ctx, PathBuf) {
        let root = sandbox(name);
        let config = root.join("cfg");
        std::fs::create_dir_all(&config).unwrap();
        set_inbox(&config, root.join("inbox").to_str().unwrap()).unwrap();
        (ctx_with(vec![], &config.join("bridge")), root)
    }

    fn send(ctx: &Ctx, name: &str, bytes: &[u8], chunk: usize) -> Result<Value, Failure> {
        let text = STANDARD.encode(bytes);
        let id = start_upload(&json!({ "name": name, "expectedBase64Length": text.len() }), ctx)?["uploadId"].as_str().unwrap().to_string();
        let mut offset = 0;
        for piece in text.as_bytes().chunks(chunk.max(1)) {
            let piece = std::str::from_utf8(piece).unwrap();
            append_upload(&json!({ "uploadId": id, "offset": offset, "contentBase64": piece }), ctx)?;
            offset += piece.len();
        }
        commit_upload(&json!({ "uploadId": id }), ctx)
    }

    fn fetch(ctx: &Ctx, path: &str) -> (Value, Vec<u8>) {
        let started = start_download(&json!({ "path": path }), ctx).unwrap();
        let id = started["downloadId"].as_str().unwrap().to_string();
        let mut out = Vec::new();
        loop {
            let part = read_download_chunk(&json!({ "downloadId": id, "offset": out.len() }), ctx).unwrap();
            out.extend(STANDARD.decode(part["contentBase64"].as_str().unwrap()).unwrap());
            if part["done"].as_bool().unwrap() {
                break;
            }
        }
        end_download(&json!({ "downloadId": id }), ctx).unwrap();
        (started, out)
    }

    #[test]
    fn an_uploaded_file_arrives_whole_in_the_configured_inbox() {
        let (ctx, root) = setup("xfer-whole");
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        // Chunks of 1001 base64 characters end inside four-character groups.
        let done = send(&ctx, "data.bin", &body, 1001).unwrap();
        let path = PathBuf::from(done["path"].as_str().unwrap());
        assert_eq!(path, root.join("inbox").join("data.bin"));
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert_eq!(done["bytes"], body.len());
        let leftovers: Vec<_> = std::fs::read_dir(root.join("inbox")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("data.bin")], "no .part file is left behind");
    }

    #[test]
    fn a_second_file_with_the_same_name_gets_a_counter_and_the_first_is_untouched() {
        let (ctx, root) = setup("xfer-collide");
        send(&ctx, "report.pdf", b"first", 64).unwrap();
        let second = send(&ctx, "report.pdf", b"second", 64).unwrap();
        let third = send(&ctx, "report.pdf", b"third", 64).unwrap();
        assert_eq!(second["name"], "report (1).pdf");
        assert_eq!(third["name"], "report (2).pdf");
        assert_eq!(std::fs::read(root.join("inbox/report.pdf")).unwrap(), b"first");
        assert_eq!(std::fs::read(root.join("inbox/report (1).pdf")).unwrap(), b"second");
    }

    #[test]
    fn a_name_that_climbs_out_of_the_inbox_is_cut_to_its_last_component() {
        let (ctx, root) = setup("xfer-escape");
        // "" and ".." both reduce to "file"; the second one meets the first and gets a counter, so each lands apart.
        for (asked, saved) in [("../../etc/passwd", "passwd"), ("..\\..\\win.ini", "win.ini"), ("/abs/olute.txt", "olute.txt"), (".hidden", "hidden"), ("", "file"), ("..", "file (1)")] {
            let done = send(&ctx, asked, b"x", 64).unwrap();
            assert_eq!(done["name"], saved, "{asked:?}");
            assert!(PathBuf::from(done["path"].as_str().unwrap()).starts_with(root.join("inbox")), "{asked:?}");
        }
        assert!(!root.join("etc").exists() && !root.join("win.ini").exists());
    }

    #[test]
    fn an_out_of_order_or_oversized_chunk_removes_the_partial_file() {
        let (ctx, root) = setup("xfer-order");
        let id = start_upload(&json!({ "name": "a.bin" }), &ctx).unwrap()["uploadId"].as_str().unwrap().to_string();
        let skipped = append_upload(&json!({ "uploadId": id, "offset": 8, "contentBase64": "AAAA" }), &ctx).unwrap_err();
        assert_eq!(skipped.0, "bad_request");
        // The upload is gone: a later append finds nothing, and nothing is left in the inbox.
        assert_eq!(append_upload(&json!({ "uploadId": id, "offset": 0, "contentBase64": "AAAA" }), &ctx).unwrap_err().0, "not_found");
        assert_eq!(std::fs::read_dir(root.join("inbox")).unwrap().count(), 0);
    }

    #[test]
    fn an_upload_shorter_than_announced_is_refused_at_commit_and_leaves_nothing() {
        let (ctx, root) = setup("xfer-short");
        let id = start_upload(&json!({ "name": "a.bin", "expectedBase64Length": 100 }), &ctx).unwrap()["uploadId"].as_str().unwrap().to_string();
        append_upload(&json!({ "uploadId": id, "offset": 0, "contentBase64": "AAAA" }), &ctx).unwrap();
        assert_eq!(commit_upload(&json!({ "uploadId": id }), &ctx).unwrap_err().0, "bad_request");
        assert_eq!(std::fs::read_dir(root.join("inbox")).unwrap().count(), 0);
    }

    #[test]
    fn a_download_returns_the_file_byte_for_byte_over_several_chunks() {
        let (ctx, root) = setup("xfer-down");
        let body: Vec<u8> = (0..(DOWNLOAD_CHUNK_BYTES * 2 + 17) as u32).map(|i| (i % 253) as u8).collect();
        let file = root.join("big.dat");
        std::fs::write(&file, &body).unwrap();
        let (started, got) = fetch(&ctx, file.to_str().unwrap());
        assert_eq!(started["name"], "big.dat");
        assert_eq!(started["bytes"], body.len());
        assert_eq!(got, body);
    }

    #[test]
    fn a_directory_downloads_as_a_zip_of_its_files_and_skips_symlinks() {
        let (ctx, root) = setup("xfer-zip");
        let dir = root.join("proj");
        std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        std::fs::write(dir.join("a.txt"), b"alpha").unwrap();
        std::fs::write(dir.join("sub/b.txt"), b"bravo").unwrap();
        std::fs::write(dir.join("sub/deeper/c.txt"), b"charlie").unwrap();
        let secret = root.join("secret.txt");
        std::fs::write(&secret, b"must not travel").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, dir.join("link.txt")).unwrap();
        let (started, bytes) = fetch(&ctx, dir.to_str().unwrap());
        assert_eq!(started["name"], "proj.zip");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut names: Vec<String> = (0..archive.len()).map(|i| archive.by_index(i).unwrap().name().to_string()).collect();
        names.sort();
        assert_eq!(names, ["proj/a.txt", "proj/sub/", "proj/sub/b.txt", "proj/sub/deeper/", "proj/sub/deeper/c.txt"]);
        let mut text = String::new();
        archive.by_name("proj/sub/deeper/c.txt").unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "charlie");
        // The staged zip is removed when the download ends.
        let staged = std::fs::read_dir(ctx.dir.join("transfer")).map(|d| d.count()).unwrap_or(0);
        assert_eq!(staged, 0);
    }

    #[test]
    fn a_missing_path_is_not_found_and_a_device_node_is_refused() {
        let (ctx, _root) = setup("xfer-missing");
        assert_eq!(start_download(&json!({ "path": "/nonexistent/nope.txt" }), &ctx).unwrap_err().0, "not_found");
        assert_eq!(start_download(&json!({ "path": "" }), &ctx).unwrap_err().0, "bad_request");
        #[cfg(unix)]
        assert_eq!(start_download(&json!({ "path": "/dev/null" }), &ctx).unwrap_err().0, "bad_request");
    }

    #[test]
    fn changing_the_inbox_setting_takes_effect_on_the_next_upload_without_a_restart() {
        let (ctx, root) = setup("xfer-setting");
        send(&ctx, "one.txt", b"1", 64).unwrap();
        let elsewhere = root.join("elsewhere");
        set_inbox(&config_dir(&ctx.dir), elsewhere.to_str().unwrap()).unwrap();
        let done = send(&ctx, "two.txt", b"2", 64).unwrap();
        assert_eq!(PathBuf::from(done["path"].as_str().unwrap()), elsewhere.join("two.txt"));
        assert!(root.join("inbox/one.txt").exists());
        assert_eq!(inbox(&ctx).unwrap()["path"], elsewhere.to_string_lossy().as_ref());
    }

    #[test]
    fn set_inbox_refuses_a_relative_path_an_empty_name_and_a_folder_it_cannot_write() {
        let root = sandbox("xfer-set");
        assert!(set_inbox(&root, "relative/dir").is_err());
        assert!(set_inbox(&root, "   ").is_err());
        #[cfg(unix)]
        assert!(set_inbox(&root, "/proc/sesync-cannot-make-this").is_err());
        assert!(!root.join(SETTINGS_FILE).exists(), "a refused folder is not saved");
    }
}
