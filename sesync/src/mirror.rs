//! Deterministic mirror ids and content hashes. Ported from daemon/sesync.py.
//! These must match the Python values exactly: a different id creates a second
//! copy of every mirror instead of replacing the existing one.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Same namespace as the Python daemon (`NAMESPACE`).
const NAMESPACE: &str = "5e55a0b1-7c3e-4d2a-9f1b-0a12ab34cd56";

/// uuid5 of "<source_key>|<target>", the id of a jsonl mirror.
pub fn mirror_uuid(source_key: &str, target: &str) -> String {
    let ns = Uuid::parse_str(NAMESPACE).expect("constant namespace");
    Uuid::new_v5(&ns, format!("{source_key}|{target}").as_bytes()).to_string()
}

/// Lowercase sha256 hex of a byte slice (the Python file_hash, for one buffer).
pub fn file_hash_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a source or mirror key names a file in a harness store, as opposed to an OpenCode
/// session id (`ses_...`), which is opaque and lives in the database.
///
/// This used to be spelled `key.starts_with('/')`. A Windows path is absolute but never starts
/// with `/`, so the test was false for every path on Windows: the demoted source in `promote`
/// was never recorded in `mirror_keys`, the scan re-adopted it as a source, and a conversation
/// ended up claiming to be both the source and its own mirror.
pub fn is_file_key(key: &str) -> bool {
    std::path::Path::new(key).is_absolute()
}

/// Lowercase sha256 hex of a file, read in 64 KiB chunks (the Python file_hash). The chunk is
/// small on purpose: a daemon that hashes every session each pass must not hold a 1 MiB buffer per
/// file in a process that can run out of memory.
pub fn file_hash(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The files that hold one file-backed conversation copy, main file first, as its harness names
/// them (an Antigravity conversation is its database and its write-ahead log). A format sesync
/// does not know is its one file.
pub fn copy_files(fmt: &str, path: &std::path::Path) -> Vec<std::path::PathBuf> {
    crate::harness::get(fmt).map_or_else(|| vec![path.to_path_buf()], |h| h.files_of(path))
}

/// The content hash of a copy, over every file `copy_files` names. A copy that is one file
/// hashes exactly as `file_hash`, so records written before this existed still match.
pub fn copy_hash(fmt: &str, path: &std::path::Path) -> std::io::Result<String> {
    let hashes = copy_files(fmt, path).iter().map(|f| file_hash(f)).collect::<std::io::Result<Vec<_>>>()?;
    Ok(combine_hashes(&hashes))
}

/// One hash for a copy from the hashes of its files, in `copy_files` order: the file's own hash
/// for a one-file copy, else the hash of the `:`-joined list.
pub fn combine_hashes(hashes: &[String]) -> String {
    match hashes {
        [one] => one.clone(),
        many => file_hash_bytes(many.join(":").as_bytes()),
    }
}

/// The cheap stamp of a copy: `size:mtime_ns` of each file `copy_files` names, joined with `+`.
/// "missing" when the main file is gone. One file stamps exactly as `divergence::fingerprint`.
pub fn copy_stamp(fmt: &str, path: &std::path::Path) -> String {
    let files = copy_files(fmt, path);
    let main = crate::divergence::fingerprint(&files[0]);
    if main == "missing" {
        return main;
    }
    files[1..].iter().fold(main, |acc, f| format!("{acc}+{}", crate::divergence::fingerprint(f)))
}

/// The newest modification time among a copy's files, in seconds since the epoch (0.0 when
/// none can be read).
pub fn copy_mtime(fmt: &str, path: &std::path::Path) -> f64 {
    copy_files(fmt, path).iter().map(|f| crate::divergence::mtime_secs(f)).fold(0.0, f64::max)
}

/// What one stat per file of a copy tells the daemon: the newest modification time of the copy in
/// seconds (as `copy_mtime`), the size of its main file (None when that file is gone), and its
/// identity, the size, modification time in nanoseconds and inode of each file `copy_files` names
/// ("missing" for a main file that is gone). Two identities that are equal name the same bytes as
/// far as a stat can tell.
pub struct CopyStat {
    pub mtime: f64,
    pub size: Option<u64>,
    pub identity: String,
}

/// The `CopyStat` of a copy, with one stat per file.
pub fn copy_stat(fmt: &str, path: &std::path::Path) -> CopyStat {
    let mut mtime = 0.0_f64;
    let mut size = None;
    let mut parts = Vec::new();
    for (i, f) in copy_files(fmt, path).iter().enumerate() {
        match std::fs::metadata(f) {
            Ok(m) => {
                let since = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
                mtime = mtime.max(since.map_or(0.0, |d| d.as_secs_f64()));
                if i == 0 {
                    size = Some(m.len());
                }
                parts.push(identity_of(&m));
            }
            Err(_) => parts.push("missing".to_string()),
        }
    }
    CopyStat { mtime, size, identity: parts.join("+") }
}

/// The identity of one file: size, modification time in nanoseconds and inode, as `copy_stat`
/// records it. None when the file cannot be read.
pub fn file_identity(path: &std::path::Path) -> Option<String> {
    std::fs::metadata(path).ok().map(|m| identity_of(&m))
}

fn identity_of(m: &std::fs::Metadata) -> String {
    let nanos = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    format!("{}:{nanos}:{}", m.len(), inode(m))
}

#[cfg(unix)]
fn inode(m: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::ino(m)
}

/// Windows has no stable inode in std, so the identity there is size and modification time.
#[cfg(not(unix))]
fn inode(_m: &std::fs::Metadata) -> u64 {
    0
}

/// Content hashes of the copies a daemon scans, carried from one pass to the next. A copy is read
/// again only when its identity (`copy_stat`) moved since it was last hashed, so an unchanged
/// session is never read twice. The hash is `copy_hash`'s. A copy the pass no longer sees is
/// dropped, so the map holds only live files.
#[derive(Default)]
pub struct CopyHashes {
    hashed: HashMap<PathBuf, (String, String)>,
    seen: HashMap<PathBuf, (String, String)>,
}

impl CopyHashes {
    /// The content hash of `path`, given its identity from this pass's `copy_stat`.
    pub fn hash(&mut self, fmt: &str, path: &Path, identity: &str) -> std::io::Result<String> {
        let hash = match self.hashed.get(path) {
            Some((known, hash)) if known == identity => hash.clone(),
            _ => copy_hash(fmt, path)?,
        };
        self.seen.insert(path.to_path_buf(), (identity.to_string(), hash.clone()));
        Ok(hash)
    }

    /// Keep only the copies this pass hashed; call once per pass, after every `hash`.
    pub fn end_pass(&mut self) {
        self.hashed = std::mem::take(&mut self.seen);
    }
}

/// Remove a copy's files: the main file first, then the rest `copy_files` names, plus the files
/// its harness keeps beside it that are not content (a database index). A file already gone is
/// not an error.
pub fn remove_copy(fmt: &str, path: &std::path::Path) -> std::io::Result<()> {
    let mut files = copy_files(fmt, path);
    if let Some(h) = crate::harness::get(fmt) {
        files.extend(h.side_files(path));
    }
    for f in files {
        match std::fs::remove_file(&f) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sesync-mirror-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Hash through the cache, the way the scan does: the identity is taken first, from this pass's stat.
    fn hash(cache: &mut CopyHashes, path: &Path) -> String {
        let id = copy_stat("omp", path).identity;
        cache.hash("omp", path, &id).unwrap()
    }

    #[test]
    fn a_copy_is_read_again_only_when_its_size_mtime_or_inode_moves() {
        let d = scratch("identity");
        let f = d.join("a.jsonl");
        std::fs::write(&f, b"abc").unwrap();
        let at = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(at).unwrap();
        let mut cache = CopyHashes::default();
        let first = hash(&mut cache, &f);
        assert_eq!(first, file_hash(&f).unwrap());
        cache.end_pass();

        // Same size and mtime: the stored hash is returned without reading the file again.
        std::fs::write(&f, b"xyz").unwrap();
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(at).unwrap();
        assert_eq!(hash(&mut cache, &f), first);
        cache.end_pass();

        // A size change is read again.
        std::fs::write(&f, b"abcdef").unwrap();
        assert_eq!(hash(&mut cache, &f), file_hash(&f).unwrap());
        cache.end_pass();

        // An mtime change with the same size is read again.
        std::fs::write(&f, b"uvwxyz").unwrap();
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(at + Duration::from_secs(5)).unwrap();
        assert_eq!(hash(&mut cache, &f), file_hash(&f).unwrap());
        cache.end_pass();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_replaced_file_with_the_same_size_and_mtime_is_read_again() {
        let d = scratch("inode");
        let f = d.join("a.jsonl");
        let next = d.join("next.jsonl");
        std::fs::write(&f, b"abc").unwrap();
        std::fs::write(&next, b"xyz").unwrap();
        let at = SystemTime::now() - Duration::from_secs(3600);
        for p in [&f, &next] {
            std::fs::File::options().write(true).open(p).unwrap().set_modified(at).unwrap();
        }
        let mut cache = CopyHashes::default();
        assert_eq!(hash(&mut cache, &f), file_hash(&f).unwrap());
        cache.end_pass();
        // An atomic replace keeps size and mtime; only the inode moves.
        std::fs::rename(&next, &f).unwrap();
        assert_eq!(hash(&mut cache, &f), file_hash(&f).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_copy_the_pass_did_not_see_is_forgotten() {
        let d = scratch("forget");
        let f = d.join("a.jsonl");
        std::fs::write(&f, b"abc").unwrap();
        let mut cache = CopyHashes::default();
        hash(&mut cache, &f);
        cache.end_pass();
        assert_eq!(cache.hashed.len(), 1);
        cache.end_pass();
        assert!(cache.hashed.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
