//! The team trigger. A marked session is pushed to the team once its source has been quiet for the
//! team quiet period (2 minutes by default). The bytes sent are the canonical omp/Pi journal for
//! that session, whatever harness it came from, so every member can install it locally.
//!
//! Only marked sessions are eligible. A session whose content has not changed since the last push
//! is skipped, so a quiet tick never re-uploads. Pushes go through the sealed route, under a lease.

use crate::harness::{self, Locations, Tools};
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

/// The canonical journal bytes for a session of any harness. A harness whose files are journals
/// sends the file as it is; every other one is converted first (`Harness::to_journal`) into
/// `scratch`, and the converted file is removed once read, so no copy of the session is left
/// behind. Every result is the same format, so any member can install it into their own harness.
/// Errors name the step that failed, never the content.
pub fn canonical_for_source(fmt: &str, key: &str, locations: &Locations, tools: &Tools, scratch: &Path) -> Result<Vec<u8>, String> {
    let h = harness::get(fmt).ok_or_else(|| format!("no canonical journal for a '{fmt}' session"))?;
    match h.to_journal(locations.of(fmt), tools, key, scratch)? {
        None => std::fs::read(key).map_err(|e| format!("cannot read {key}: {e}")),
        Some(journal) => {
            let bytes = std::fs::read(&journal).map_err(|e| format!("converted journal missing: {e}"));
            let _ = std::fs::remove_file(&journal);
            bytes
        }
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
