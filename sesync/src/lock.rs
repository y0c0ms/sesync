//! Lock client for the daemon. Ported from daemon/sesync.py lock_holder and
//! sync_source. The daemon uses the `sesync_lock` binary; it never blocks on the lock
//! layer itself.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where claims and their sweep live. `SESYNC_LOCK_DIR` wins; otherwise the stored directory that
/// sesync-lock uses by default. The daemon, the TUI and the sweep all call this, so they agree.
pub fn lock_dir_for(home: &Path, override_dir: Option<PathBuf>) -> PathBuf {
    override_dir.unwrap_or_else(|| home.join(".local").join("share").join("harness-sync").join("locks"))
}

/// The characters sesync-lock keeps in an id; it drops the rest, so an alias must be named the
/// same way or the lock binary never finds it.
fn lock_name(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

/// Point a copy's own session id at its conversation's lock name. A harness hook inside a copy
/// claims the copy's id, and sesync-lock follows `map/<id>` one hop; without the alias the hook
/// and the daemon's write each took a lock of their own and never excluded each other. A no-op
/// when the id already is the conversation's name or the alias is in place. The conversation's
/// own name is never aliased away, or every claim on it would resolve somewhere else.
pub fn write_alias(lock_dir: &Path, id: &str, conv: &str) -> std::io::Result<()> {
    let (id, conv) = (lock_name(id), lock_name(conv));
    if id.is_empty() || conv.is_empty() || id == conv {
        return Ok(());
    }
    let dir = lock_dir.join("map");
    std::fs::create_dir_all(&dir)?;
    let own = dir.join(&conv);
    if own.exists() {
        std::fs::remove_file(&own)?;
    }
    let path = dir.join(&id);
    if std::fs::read_to_string(&path).map(|s| s.trim() == conv).unwrap_or(false) {
        return Ok(());
    }
    let tmp = dir.join(format!(".{id}.tmp"));
    std::fs::write(&tmp, &conv)?;
    std::fs::rename(&tmp, &path)
}

#[derive(Debug, PartialEq)]
pub enum ClaimOutcome<T> {
    /// The claim was taken, the work ran, and the claim was released.
    Ran(T),
    /// Another harness holds the conversation. The work did not run. Holds "harness pid".
    Skipped(String),
    /// The lock tool exists but could not claim (it failed or could not run). The work did
    /// not run: a write without the claim could race a live turn. Holds the reason.
    Failed(String),
}

/// "harness pid" if a live process holds the conversation, else None.
/// A missing binary or a free conversation gives None. `lock_dir` is passed to
/// the child only, so callers never depend on the process environment.
pub fn holder_of(sesync_lock: &Path, lock_dir: Option<&Path>, conversation_id: &str) -> Option<String> {
    if !sesync_lock.exists() {
        return None;
    }
    let mut cmd = Command::new(sesync_lock);
    if let Some(d) = lock_dir {
        cmd.env("SESYNC_LOCK_DIR", d);
    }
    let out = cmd.args(["check", conversation_id]).output().ok()?;
    if out.status.code() == Some(2) {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

/// Take the claim on `conv` for `harness` and `pid` and return at once. `Ran(())` means the claim
/// is held, or that the lock tool is not installed and there is nothing to hold. A caller that
/// gets `Ran` must release it with [`release`]; `Skipped` names the holder.
pub fn claim(
    sesync_lock: &Path,
    lock_dir: Option<&Path>,
    conv: &str,
    harness: &str,
    pid: u32,
    target: &str,
) -> ClaimOutcome<()> {
    if !sesync_lock.exists() {
        return ClaimOutcome::Ran(());
    }
    let mut cmd = Command::new(sesync_lock);
    if let Some(d) = lock_dir {
        cmd.env("SESYNC_LOCK_DIR", d);
    }
    match cmd.args(["claim", conv, harness, &pid.to_string(), target]).output() {
        Ok(o) if o.status.code() == Some(2) => {
            ClaimOutcome::Skipped(String::from_utf8_lossy(&o.stdout).trim().to_string())
        }
        Ok(o) if o.status.success() => ClaimOutcome::Ran(()),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            ClaimOutcome::Failed(format!("lock tool exited {}: {err}", o.status))
        }
        Err(e) => ClaimOutcome::Failed(format!("cannot run lock tool: {e}")),
    }
}

/// Release a claim `harness` and `pid` hold on `conv`. Nothing is removed for another holder, and a
/// missing lock tool or lock is not an error.
pub fn release(sesync_lock: &Path, lock_dir: Option<&Path>, conv: &str, harness: &str, pid: u32) {
    if !sesync_lock.exists() {
        return;
    }
    let mut cmd = Command::new(sesync_lock);
    if let Some(d) = lock_dir {
        cmd.env("SESYNC_LOCK_DIR", d);
    }
    let _ = cmd.args(["release", conv, harness, &pid.to_string()]).output();
}

/// Releases the claim when dropped, so a panic in the work cannot leave the
/// conversation locked. The Python daemon does the same in a `finally`.
struct Release<'a> {
    sesync_lock: &'a Path,
    lock_dir: Option<&'a Path>,
    conv: &'a str,
    harness: &'a str,
    pid: u32,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        release(self.sesync_lock, self.lock_dir, self.conv, self.harness, self.pid);
    }
}

/// Claim `conv` for the duration of `work`, the way a harness turn does.
/// If the binary is not installed, the work runs with no claim. If another harness
/// holds it, the work is skipped and its holder is returned. If the binary is installed
/// but fails or cannot run, the work is skipped and the reason is returned.
pub fn with_claim<T>(
    sesync_lock: &Path,
    lock_dir: Option<&Path>,
    conv: &str,
    harness: &str,
    pid: u32,
    target: &str,
    work: impl FnOnce() -> T,
) -> ClaimOutcome<T> {
    match claim(sesync_lock, lock_dir, conv, harness, pid, target) {
        ClaimOutcome::Ran(()) => {
            // Dropped at the end of this scope, on success or panic alike.
            let _guard = Release { sesync_lock, lock_dir, conv, harness, pid };
            ClaimOutcome::Ran(work())
        }
        ClaimOutcome::Skipped(holder) => ClaimOutcome::Skipped(holder),
        ClaimOutcome::Failed(reason) => ClaimOutcome::Failed(reason),
    }
}
