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

#[derive(Debug, PartialEq)]
pub enum ClaimOutcome<T> {
    /// The claim was taken, the work ran, and the claim was released.
    Ran(T),
    /// Another harness holds the conversation. The work did not run. Holds "harness pid".
    Skipped(String),
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

/// Releases the claim when dropped, so a panic in the work cannot leave the
/// conversation locked. The Python daemon does the same in a `finally`.
struct Release<'a> {
    sesync_lock: &'a Path,
    lock_dir: Option<&'a Path>,
    conv: &'a str,
    harness: &'a str,
    pid: u32,
    active: bool,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        if self.active && self.sesync_lock.exists() {
            let mut cmd = Command::new(self.sesync_lock);
            if let Some(d) = self.lock_dir {
                cmd.env("SESYNC_LOCK_DIR", d);
            }
            let _ = cmd.args(["release", self.conv, self.harness, &self.pid.to_string()]).output();
        }
    }
}

/// Claim `conv` for the duration of `work`, the way a harness turn does.
/// If the binary is missing, the work runs with no claim. If another harness
/// holds it, the work is skipped and its holder is returned.
pub fn with_claim<T>(
    sesync_lock: &Path,
    lock_dir: Option<&Path>,
    conv: &str,
    harness: &str,
    pid: u32,
    target: &str,
    work: impl FnOnce() -> T,
) -> ClaimOutcome<T> {
    if !sesync_lock.exists() {
        return ClaimOutcome::Ran(work());
    }
    let mut cmd = Command::new(sesync_lock);
    if let Some(d) = lock_dir {
        cmd.env("SESYNC_LOCK_DIR", d);
    }
    let out = cmd.args(["claim", conv, harness, &pid.to_string(), target]).output();
    match out {
        Ok(o) if o.status.code() == Some(2) => {
            return ClaimOutcome::Skipped(String::from_utf8_lossy(&o.stdout).trim().to_string());
        }
        Ok(o) if o.status.success() => {}
        // Any other failure: the daemon never blocks on the lock layer, so run the work.
        _ => return ClaimOutcome::Ran(work()),
    }
    // Dropped at the end of this scope, on success or panic alike.
    let _guard = Release { sesync_lock, lock_dir, conv, harness, pid, active: true };
    ClaimOutcome::Ran(work())
}
