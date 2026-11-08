//! `sesync machine send` and `sesync machine get`: move a file or a folder between this machine and another one
//! over the bridge link, the way the phone moves an image.
//!
//! `send <machine> <path>` uploads a file into the other machine's inbox (`transfer.startUpload` and its chunks,
//! `bridge::transfer`); a folder is zipped here first. `get <machine> <path-there> [<folder-here>]` downloads a
//! file, or a folder as a zip, from the other machine into a folder on this one (the current directory by default).

use super::client::{self, Reader, Writer};
use super::store::Machine;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::json;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Raw bytes per upload chunk: 384 KiB, whose base64 text is 512 Ki characters, the most the bridge takes.
const UPLOAD_CHUNK_BYTES: usize = 384 * 1024;

/// What a finished transfer reports: where the file is and how many bytes moved.
pub struct Done {
    pub path: String,
    pub bytes: u64,
}

/// Send `source` (a file, or a folder zipped first) to `machine`'s inbox.
pub async fn send(machine: &Machine, source: &Path) -> Result<Done, String> {
    let meta = std::fs::metadata(source).map_err(|e| format!("cannot read {}: {e}", source.display()))?;
    let (file, name, temp) = if meta.is_dir() {
        let (zip, name) = zip_folder(source)?;
        (zip.clone(), name, Some(zip))
    } else if meta.is_file() {
        let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        (source.to_path_buf(), name, None)
    } else {
        return Err(format!("{} is neither a file nor a folder", source.display()));
    };
    let result = upload(machine, &file, &name).await;
    if let Some(temp) = temp {
        let _ = std::fs::remove_file(temp);
    }
    result
}

async fn upload(machine: &Machine, file: &Path, name: &str) -> Result<Done, String> {
    let size = std::fs::metadata(file).map_err(|e| e.to_string())?.len();
    let (mut writer, mut reader) = client::connect(machine).await?;
    let announced = (size as f64 / 3.0).ceil() as u64 * 4;
    let started = client::call(&mut writer, &mut reader, "transfer.startUpload", json!({ "name": name, "expectedBase64Length": announced })).await?;
    let id = started["uploadId"].as_str().ok_or("the machine did not return an upload id")?.to_string();
    match push_chunks(&mut writer, &mut reader, &id, file).await {
        Ok(()) => {
            let done = client::call(&mut writer, &mut reader, "transfer.commitUpload", json!({ "uploadId": id })).await?;
            Ok(Done { path: done["path"].as_str().unwrap_or("").to_string(), bytes: done["bytes"].as_u64().unwrap_or(size) })
        }
        Err(e) => {
            let _ = client::call(&mut writer, &mut reader, "transfer.abortUpload", json!({ "uploadId": id })).await;
            Err(e)
        }
    }
}

async fn push_chunks(writer: &mut Writer, reader: &mut Reader, id: &str, file: &Path) -> Result<(), String> {
    let mut source = std::fs::File::open(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let mut buffer = vec![0u8; UPLOAD_CHUNK_BYTES];
    let mut offset = 0u64;
    loop {
        let n = read_full(&mut source, &mut buffer).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
        if n == 0 {
            return Ok(());
        }
        // A chunk of whole 3-byte groups encodes without padding, so the base64 offsets line up.
        let text = STANDARD.encode(&buffer[..n]);
        client::call(writer, reader, "transfer.appendUploadChunk", json!({ "uploadId": id, "offset": offset, "contentBase64": text })).await?;
        offset += text.len() as u64;
    }
}

/// Fill `buffer` as far as the file goes: a short read is not the end.
fn read_full(source: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match source.read(&mut buffer[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Download `remote` (a path on `machine`) into the folder `into`, returning the file written.
pub async fn get(machine: &Machine, remote: &str, into: &Path) -> Result<Done, String> {
    std::fs::create_dir_all(into).map_err(|e| format!("cannot create {}: {e}", into.display()))?;
    let (mut writer, mut reader) = client::connect(machine).await?;
    let started = client::call(&mut writer, &mut reader, "transfer.startDownload", json!({ "path": remote })).await?;
    let id = started["downloadId"].as_str().ok_or("the machine did not return a download id")?.to_string();
    let name = safe_name(started["name"].as_str().unwrap_or("download"));
    let size = started["bytes"].as_u64().unwrap_or(0);
    let result = pull_chunks(&mut writer, &mut reader, &id, into, &name, size).await;
    let _ = client::call(&mut writer, &mut reader, "transfer.endDownload", json!({ "downloadId": id })).await;
    result
}

async fn pull_chunks(writer: &mut Writer, reader: &mut Reader, id: &str, into: &Path, name: &str, size: u64) -> Result<Done, String> {
    let (target, mut out) = create_unique(into, name)?;
    let mut offset = 0u64;
    let outcome: Result<(), String> = async {
        loop {
            let part = client::call(writer, reader, "transfer.readDownloadChunk", json!({ "downloadId": id, "offset": offset })).await?;
            let bytes = STANDARD
                .decode(part["contentBase64"].as_str().ok_or("the machine sent a chunk without content")?)
                .map_err(|_| "the machine sent a chunk that is not base64".to_string())?;
            out.write_all(&bytes).map_err(|e| format!("cannot write {}: {e}", target.display()))?;
            offset += bytes.len() as u64;
            if part["done"].as_bool().unwrap_or(false) {
                return Ok(());
            }
            if bytes.is_empty() {
                return Err("the machine stopped sending before the end of the file".into());
            }
        }
    }
    .await;
    drop(out);
    match outcome {
        Ok(()) if offset == size => Ok(Done { path: target.to_string_lossy().into_owned(), bytes: offset }),
        Ok(()) => {
            let _ = std::fs::remove_file(&target);
            Err(format!("received {offset} bytes but the machine announced {size}"))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&target);
            Err(e)
        }
    }
}

/// A file name from the other machine, cut to its last component so it cannot leave the target folder.
fn safe_name(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let name = last.trim().trim_start_matches('.').to_string();
    if name.is_empty() {
        "download".into()
    } else {
        name
    }
}

/// Create `<dir>/<name>` new, or `<stem> (n).<ext>` when that exists. Nothing is overwritten.
fn create_unique(dir: &Path, name: &str) -> Result<(PathBuf, std::fs::File), String> {
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    for n in 0..10_000u32 {
        let candidate = if n == 0 { name.to_string() } else { format!("{stem} ({n}){ext}") };
        let path = dir.join(candidate);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
        }
    }
    Err(format!("too many files named {name} in {}", dir.display()))
}

/// Zip the folder `dir` into a temporary file beside the system temp dir, returning it and `<folder>.zip`.
/// Symlinks are skipped, as on the bridge.
fn zip_folder(dir: &Path) -> Result<(PathBuf, String), String> {
    let base = std::fs::canonicalize(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "folder".into());
    let tmp = std::env::temp_dir().join(format!("sesync-send-{}-{}.zip", std::process::id(), base));
    let file = create_private(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    if let Err(e) = crate::bridge::transfer::write_zip_of(file, dir, &base) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok((tmp, format!("{base}.zip")))
}

/// What `send` prints for a finished upload.
pub fn describe_sent(machine: &str, done: &Done) -> String {
    format!("sent to {machine}: {} ({} bytes)", done.path, done.bytes)
}

/// What `get` prints for a finished download.
pub fn describe_got(done: &Done) -> String {
    format!("saved {} ({} bytes)", done.path, done.bytes)
}

/// Create `path` new and readable only by its owner: the zip of a folder sits in a shared temp directory.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
