//! The team trigger. A marked session is pushed to the team once its source has been quiet for the
//! team quiet period (2 minutes by default). The bytes sent are the canonical omp/Pi journal for
//! that session, whatever harness it came from, so every member can install it locally.
//!
//! Only marked sessions are eligible. A session whose content has not changed since the last push
//! is skipped, so a quiet tick never re-uploads. Pushes go through the sealed route, under a lease.

use crate::state::State;
use crate::team_push::{push_marked_sealed, HttpTeam, PushError};
use std::path::Path;

/// Default quiet period before a marked session is pushed. Local mirrors keep their own, shorter
/// rule (SESYNC_QUIET); this one is for the team copy only.
pub const DEFAULT_TEAM_QUIET_SECS: f64 = 120.0;

/// Whether a marked session is due: its source has been quiet long enough, and its content differs
/// from what was last pushed. Pure, so the rule is testable without a filesystem or a server.
pub fn is_due(now: f64, last_change: f64, quiet_secs: f64, content_hash: &str, pushed_hash: &str) -> bool {
    now - last_change >= quiet_secs && content_hash != pushed_hash
}

/// Where the canonical journal for a source comes from. omp and Pi sources are already journals;
/// Claude and O-penCode sources are converted first. The converter writes into `scratch`.
pub enum Canonical<'a> {
    /// Already an omp/Pi journal: the file is the canonical copy.
    Journal(&'a Path),
    /// A Claude transcript: converted with `sesync-conv from-claude`.
    Claude(&'a Path),
    /// A Gemini CLI session log: converted with `sesync-conv from-gemini`.
    Gemini(&'a Path),
}

/// Build the canonical journal bytes for a source. Errors name the step that failed, never the
/// content.
pub fn canonical_bytes(src: Canonical, sesync_conv: &Path, scratch: &Path) -> Result<Vec<u8>, String> {
    match src {
        Canonical::Journal(p) => std::fs::read(p).map_err(|e| format!("cannot read {}: {e}", p.display())),
        Canonical::Claude(p) => {
            std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
            let out = scratch.join("canonical.jsonl");
            let status = std::process::Command::new(sesync_conv)
                .arg("from-claude").arg(p).arg("--out").arg(&out)
                .status()
                .map_err(|e| format!("cannot run sesync-conv: {e}"))?;
            if !status.success() {
                return Err("sesync-conv could not convert this Claude session".into());
            }
            std::fs::read(&out).map_err(|e| format!("converted journal missing: {e}"))
        }
        Canonical::Gemini(p) => {
            std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
            let out = scratch.join("canonical.jsonl");
            let status = std::process::Command::new(sesync_conv)
                .arg("from-gemini").arg(p).arg("--out").arg(&out)
                .status()
                .map_err(|e| format!("cannot run sesync-conv: {e}"))?;
            if !status.success() {
                return Err("sesync-conv could not convert this Gemini session".into());
            }
            std::fs::read(&out).map_err(|e| format!("converted journal missing: {e}"))
        }
    }
}

/// The canonical journal for a session of any harness. omp and Pi are already journals; Claude and
/// Gemini are converted from their own session logs; O-penCode is exported with its CLI and
/// converted. Every result is the same format, so any member can install it into their own harness.
pub fn canonical_for_source(
    fmt: &str,
    key: &str,
    sesync_conv: &Path,
    opencode: &Path,
    scratch: &Path,
) -> Result<Vec<u8>, String> {
    match fmt {
        "omp" | "pi" => canonical_bytes(Canonical::Journal(Path::new(key)), sesync_conv, scratch),
        "claude" => canonical_bytes(Canonical::Claude(Path::new(key)), sesync_conv, scratch),
        "gemini" => canonical_bytes(Canonical::Gemini(Path::new(key)), sesync_conv, scratch),
        "opencode" => {
            // The key of an O-penCode source is its session id. The export is converted into scratch
            // and removed once read, so no copy of the session is left behind.
            let journal = crate::opencode::export_and_convert(opencode, sesync_conv, key, scratch)?;
            let bytes = std::fs::read(&journal).map_err(|e| format!("converted journal missing: {e}"));
            let _ = std::fs::remove_file(&journal);
            bytes
        }
        other => Err(format!("no canonical journal for a '{other}' session")),
    }
}

/// The outcome of one team pass over the marked sessions.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TeamPass {
    pub pushed: Vec<String>,
    pub skipped_unchanged: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Push every due marked session. `content_of` gives the canonical bytes for a session id, and
/// `last_change_of` its source's last write time. `seal` takes the session id, the lease version the
/// push was granted (the record's sequence) and the plaintext, and returns the bytes to send. A
/// session with no content or a failed push is recorded in `failed` and the pass goes on. A
/// successful push records the hash and time in state.
pub fn push_due<R, C, T, S>(
    state: &mut State,
    remote: &R,
    space: &str,
    now: f64,
    quiet_secs: f64,
    mut content_of: C,
    mut last_change_of: T,
    mut seal: S,
) -> TeamPass
where
    R: crate::team_push::TeamRemote,
    C: FnMut(&str) -> Result<Vec<u8>, String>,
    T: FnMut(&str) -> Option<f64>,
    S: FnMut(&str, u64, &[u8]) -> Result<Vec<u8>, String>,
{
    let mut out = TeamPass::default();
    for id in state.sessions_shared_with(space) {
        let Some(changed) = last_change_of(&id) else { continue };
        let plain = match content_of(&id) {
            Ok(b) => b,
            Err(e) => { out.failed.push((id, e)); continue; }
        };
        // Change is judged on the plaintext journal. Sealing draws a fresh key and nonce every
        // time, so hashing the sealed bytes would make every quiet tick look like a change.
        let hash = crate::mirror::file_hash_bytes(&plain);
        let pushed_hash = state.team_hash(space, &id).unwrap_or_default();
        if !is_due(now, changed, quiet_secs, &hash, &pushed_hash) {
            out.skipped_unchanged.push(id);
            continue;
        }
        // The lease is taken inside push_marked_sealed, and the version it grants is passed to
        // seal, so the record's sequence is the version the bytes are written under.
        let pushed = push_marked_sealed(state, remote, space, &id, |version| seal(&id, version, &plain));
        match pushed {
            Ok(()) => {
                state.record_team_push(space, &id, &hash, now);
                out.pushed.push(id);
            }
            Err(PushError::Held) => out.failed.push((id, "held by another machine; will retry".into())),
            Err(e) => out.failed.push((id, format!("{e:?}"))),
        }
    }
    out
}

/// The team copy's HTTP remote for a configured team. Settings come from the member credential.
pub fn remote_for(base: &str, token: &str, machine: &str) -> HttpTeam {
    HttpTeam { base: base.to_string(), token: token.to_string(), machine: machine.to_string() }
}
