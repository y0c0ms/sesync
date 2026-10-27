//! The team trigger. A marked session is pushed to the team once its source has been quiet for the
//! team quiet period (2 minutes by default). The bytes sent are the canonical omp/Pi journal for
//! that session, whatever harness it came from, so every member can install it locally.
//!
//! Only marked sessions are eligible. A session whose content has not changed since the last push
//! is skipped, so a quiet tick never re-uploads. Pushes go through the sealed route, under a lease.
//!
//! One exception re-sends unchanged content: when the member list moves to a new version, every
//! reader refuses records sealed under the older list (`StaleList`), and a member added since
//! cannot open them at all. So a machine re-seals, under the current list, each unchanged shared
//! conversation whose stored record is still its own (`ResealCheck`, `needs_reseal`).

use crate::harness::{self, Locations, Tools};
use crate::state::State;
use crate::team_push::{push_marked_sealed, HttpTeam, PushError};
use sesync_team::signed_records::{authorship, Binding};
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
    /// Unchanged content sealed again under a newer member list (see `needs_reseal`).
    pub resealed: Vec<String>,
    pub skipped_unchanged: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// What a pass needs to keep its stored records readable when the member list changes: the list
/// version it seals under, and this machine's signing key, which is the author of its records.
#[derive(Debug, Clone)]
pub struct ResealCheck {
    pub list: u64,
    pub author: Vec<u8>,
}

/// Whether an unchanged conversation must be sealed again under `check.list`. True only when the
/// record the server stores is this machine's own (proven by its signature, at or above the
/// sequence this machine has already accepted) and sealed under an older list, or when the server
/// stores none although this machine pushed one. A record by another author is theirs to re-seal:
/// sending this machine's copy over it could replace newer content. Once the answer for this list
/// is known it is recorded, so the server is asked once per conversation and list change.
pub fn needs_reseal<R: crate::team_push::TeamRemote>(state: &mut State, remote: &R, space: &str,
                                                     conversation: &str, check: &ResealCheck)
    -> Result<bool, String>
{
    if state.team_list(space, conversation) >= check.list {
        return Ok(false);
    }
    let stored = remote.stored(space, conversation)
        .map_err(|e| format!("cannot read the stored record: {e:?}"))?;
    let Some(record) = stored else { return Ok(true) };
    let expect = Binding {
        space: space.to_string(),
        conversation: conversation.to_string(),
        min_sequence: state.seen_floor(space, conversation),
    };
    let own_and_stale = matches!(authorship(&expect, &record),
        Ok(a) if a.author == check.author && a.list_version < check.list);
    if !own_and_stale {
        // Someone else's record, already current, or one that does not verify (a bad signature,
        // or a sequence below what this machine accepted): nothing for this machine to re-seal.
        state.set_team_list(space, conversation, check.list);
    }
    Ok(own_and_stale)
}

/// Push every due marked session. `content_of` gives the canonical bytes for a session id, and
/// `last_change_of` its source's last write time. `seal` takes the session id, the lease version the
/// push was granted (the record's sequence) and the plaintext, and returns the bytes to send. A
/// session with no content or a failed push is recorded in `failed` and the pass goes on. A
/// successful push records the hash and time in state. With `reseal`, an unchanged session is
/// also sent when `needs_reseal` says its stored record is this machine's and sealed under an
/// older member list; `None` (no verified list) never re-seals.
#[allow(clippy::too_many_arguments)]
pub fn push_due<R, C, T, S>(
    state: &mut State,
    remote: &R,
    space: &str,
    now: f64,
    quiet_secs: f64,
    mut content_of: C,
    mut last_change_of: T,
    mut seal: S,
    reseal: Option<&ResealCheck>,
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
        let mut resealing = false;
        if !is_due(now, changed, quiet_secs, &hash, &pushed_hash) {
            // Only content identical to the last push may be re-sealed: content still changing
            // waits for its quiet period, and is then sealed under the current list anyway.
            let check = reseal.filter(|_| hash == pushed_hash);
            match check.map(|c| needs_reseal(state, remote, space, &id, c)) {
                Some(Ok(true)) => resealing = true,
                Some(Err(e)) => { out.failed.push((id, e)); continue; }
                Some(Ok(false)) | None => { out.skipped_unchanged.push(id); continue; }
            }
        }
        // The lease is taken inside push_marked_sealed, and the version it grants is passed to
        // seal, so the record's sequence is the version the bytes are written under.
        let pushed = push_marked_sealed(state, remote, space, &id, |version| seal(&id, version, &plain));
        match pushed {
            Ok(()) => {
                state.record_team_push(space, &id, &hash, now);
                if let Some(c) = reseal {
                    state.set_team_list(space, &id, c.list);
                }
                if resealing { out.resealed.push(id) } else { out.pushed.push(id) }
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
