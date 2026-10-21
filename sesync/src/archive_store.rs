//! Compressed, verified copies of session files, kept under the session they belong to.
//!
//! Layout: <store>/<parent session id>/<file name>.hsar. The copy is written atomically
//! and read back through the archive's checksum, so a damaged copy is an error, never
//! silently returned. A parent id must be a plain name; it cannot climb out of the store.

use crate::archive::{pack, unpack, ArchiveError};
use std::fs;
use std::path::{Path, PathBuf};

/// Compression level: fast enough for live sync, small enough for transcripts.
const LEVEL: i32 = 6;

fn plain_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !s.starts_with('.')
}

/// Store a copy of `src` under `store/<parent>/`. Returns the path of the stored copy.
pub fn store_session(store: &Path, parent: &str, src: &Path) -> Result<PathBuf, String> {
    if !plain_name(parent) {
        return Err(format!("refusing parent id that is not a plain name: {parent:?}"));
    }
    let name = src.file_name().and_then(|n| n.to_str()).ok_or("source has no file name")?;
    let dir = store.join(parent);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let original = fs::read(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let packed = pack(&original, LEVEL);
    let dst = dir.join(format!("{name}.hsar"));
    let tmp = dir.join(format!(".{name}.hsar.tmp"));
    fs::write(&tmp, &packed).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &dst).map_err(|e| e.to_string())?;
    Ok(dst)
}

/// Keep bytes that are about to be replaced or deleted: `<store>/<parent>/<name>.<ms>.hsar`.
/// The millisecond stamp means a later copy of the same file never overwrites an earlier one.
/// The copy is read back and compared before this returns, so a caller that destroys the
/// original only after `Ok` can never lose it to a short write.
pub fn keep_before_replace(store: &Path, parent: &str, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    if !plain_name(parent) {
        return Err(format!("refusing parent id that is not a plain name: {parent:?}"));
    }
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return Err(format!("refusing archive name {name:?}"));
    }
    let dir = store.join(parent);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut dst = dir.join(format!("{name}.{ms}.hsar"));
    let mut n = 1;
    while dst.exists() {
        dst = dir.join(format!("{name}.{ms}-{n}.hsar"));
        n += 1;
    }
    let tmp = dir.join(format!(".{name}.{ms}.hsar.tmp"));
    fs::write(&tmp, pack(bytes, LEVEL)).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &dst).map_err(|e| e.to_string())?;
    match read_session(&dst) {
        Ok(back) if back == bytes => Ok(dst),
        Ok(_) => Err(format!("{}: the archived copy does not match", dst.display())),
        Err(e) => Err(format!("{}: {e:?}", dst.display())),
    }
}

/// Read a stored copy back. A damaged or foreign copy is an error.
pub fn read_session(copy: &Path) -> Result<Vec<u8>, ArchiveError> {
    let packed = fs::read(copy).map_err(|e| ArchiveError::Io(e.to_string()))?;
    unpack(&packed)
}
