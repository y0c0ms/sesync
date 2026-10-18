//! Divergence guard. Ported from daemon/sesync.py mirror_fingerprint and the
//! freeze rule in sync_source. A mirror is fingerprinted when it is installed; if a
//! harness writes to it later, the fingerprint changes and the mirror is frozen so
//! the daemon never overwrites the user's continued conversation.

use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MirrorCheck {
    /// The mirror is exactly as the daemon left it; it may be refreshed.
    Current,
    /// A harness wrote to it, or it has no recorded fingerprint. It is frozen.
    Diverged,
    /// The mirror file is gone.
    Missing,
}

/// Cheap identity of a mirror's content: size and modification time in nanoseconds.
/// Returns "missing" when the file does not exist.
pub fn fingerprint(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{}:{}", m.len(), mtime)
        }
        Err(_) => "missing".to_string(),
    }
}

/// The file's mtime in seconds since the epoch, 0.0 when it cannot be read.
pub fn mtime_secs(path: &Path) -> f64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Compare a mirror's current content with the fingerprint recorded at install.
/// An empty recorded fingerprint means the mirror predates fingerprinting, so it
/// cannot be shown untouched and is frozen.
pub fn check_mirror(path: &Path, recorded: &str) -> MirrorCheck {
    if !path.exists() {
        return MirrorCheck::Missing;
    }
    if recorded.is_empty() {
        return MirrorCheck::Diverged;
    }
    if fingerprint(path) == recorded {
        MirrorCheck::Current
    } else {
        MirrorCheck::Diverged
    }
}
