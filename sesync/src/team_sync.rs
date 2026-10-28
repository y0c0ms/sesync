//! The team trigger. A marked session is pushed to the team once its source has been quiet for the
//! team quiet period (2 minutes by default), and only when its COMPLETED TURNS changed since the team
//! copy was last checked (`turns.rs`). Metadata appended to a journal, a trailing prompt and an
//! unfinished reply are not changes. The bytes sent are the canonical omp/Pi journal for that session,
//! whatever harness it came from, so every member can install it locally.
//!
//! Before a push writes, the stored team copy is read under the lease and compared with this machine's
//! turns (`check_stored`): a copy this machine is ahead of is replaced; an identical copy is left alone;
//! a copy that is ahead of this machine, diverged from it, or that cannot be verified is never written
//! over. That conversation is then held (`TeamConflict`) and pushes for it stay paused until its local
//! turns change, or the user asks for this machine's copy (`TeamForce`).
//!
//! Pushes go through the sealed route, under a lease.
//!
//! One exception re-sends unchanged content: when the member list moves to a new version, every
//! reader refuses records sealed under the older list (`StaleList`), and a member added since
//! cannot open them at all. So a machine re-seals, under the current list, each unchanged shared
//! conversation whose stored record is still its own (`ResealCheck`, `needs_reseal`).

use crate::harness::{self, Locations, Tools};
use crate::state::{ConflictKind, State, TeamConflict};
use crate::team_push::{push_marked_sealed_if, HttpTeam, PushError};
use crate::turns::{compare, fingerprint, turns_of, Relation, Turn};
use sesync_team::signed_records::{authorship, sequence_of, Binding};
use std::path::Path;

/// Default quiet period before a marked session is pushed. Local mirrors keep their own, shorter
/// rule (SESYNC_QUIET); this one is for the team copy only.
pub const DEFAULT_TEAM_QUIET_SECS: f64 = 120.0;

/// Whether a marked session is due: its source has been quiet long enough, and its completed turns
/// (`content_hash`, a turn fingerprint) differ from what the team copy was last checked against
/// (`pushed_hash`). Pure, so the rule is testable without a filesystem or a server.
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

/// What a push needs from the sealing side. The daemon's codec seals for the listed devices and opens a
/// stored record the way a reader of this device would; tests use a codec that does neither.
pub trait Codec {
    /// Seal `plain` as the record for `conversation`, under lease `version` (the record's sequence).
    fn seal(&mut self, conversation: &str, version: u64, plain: &[u8]) -> Result<Vec<u8>, String>;
    /// The session text of the stored record, checked as a reader would check it. `floor` is the lowest
    /// sequence accepted for the conversation. `Err` says why it cannot be read or verified, never its content.
    fn open(&mut self, conversation: &str, floor: u64, stored: &[u8]) -> Result<Vec<u8>, String>;
}

/// The outcome of one team pass over the marked sessions.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TeamPass {
    pub pushed: Vec<String>,
    /// Unchanged content sealed again under a newer member list (see `needs_reseal`).
    pub resealed: Vec<String>,
    pub skipped_unchanged: Vec<String>,
    /// Shared, but no turn is complete yet: nothing to send until one is.
    pub waiting_for_turn: Vec<String>,
    /// Held back by the guard: (conversation, reason). The team copy is ahead, diverged, or unverifiable.
    pub refused: Vec<(String, String)>,
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
/// sequence `floor` this machine has already accepted) and sealed under an older list, or when the
/// server stores none. A record by another author is theirs to re-seal: sending this machine's copy
/// over it could replace newer content. Callers ask under the conversation's lease, so the record
/// read here is the one their write would replace.
pub fn needs_reseal<R: crate::team_push::TeamRemote>(remote: &R, space: &str, conversation: &str, floor: u64,
                                                     check: &ResealCheck) -> Result<bool, String>
{
    let stored = remote.stored(space, conversation)
        .map_err(|e| format!("cannot read the stored record: {e:?}"))?;
    let Some(record) = stored else { return Ok(true) };
    let expect = Binding {
        space: space.to_string(),
        conversation: conversation.to_string(),
        min_sequence: floor,
    };
    Ok(matches!(authorship(&expect, &record),
        Ok(a) if a.author == check.author && a.list_version < check.list))
}

/// How the stored copy compares with this machine's turns, decided under the lease.
enum Verdict {
    /// Nothing stored, or this machine is ahead of it, or the user's request applies: write.
    Push,
    /// The stored copy holds exactly these turns: nothing to write, and the record is in step.
    Same,
    /// Held back: the team copy is ahead, diverged, or unverifiable. Never written over.
    Held(TeamConflict),
}

/// The guard's decision for one stored copy. `local` are this machine's completed turns; `fp` their
/// fingerprint. Pure apart from `codec.open`, so the rule is testable with a fake codec.
fn check_stored(codec: &mut dyn Codec, conversation: &str, floor: u64, stored: &[u8], local: &[Turn], fp: &str,
                now: f64) -> Verdict
{
    let held = |kind, count, local_new, team_new, why: String| Verdict::Held(TeamConflict {
        local: fp.to_string(),
        at: now,
        kind,
        count,
        local_new,
        team_new,
        why,
    });
    let text = match codec.open(conversation, floor, stored) {
        Ok(t) => t,
        Err(why) => return held(ConflictKind::Unverified, 0, 0, 0, why),
    };
    let team = match turns_of(&text) {
        Ok(t) => t,
        Err(_) => return held(ConflictKind::Unverified, 0, 0, 0, "the stored copy is not a readable journal".into()),
    };
    match compare(local, &team) {
        Relation::Identical => Verdict::Same,
        Relation::Ahead(_) => Verdict::Push,
        Relation::Behind(n) => held(ConflictKind::Behind, n, 0, 0, String::new()),
        Relation::Diverged { common, local_new, team_new } => {
            held(ConflictKind::Diverged, common, local_new.len(), team_new.len(), String::new())
        }
    }
}

/// Push every due marked session, through the guard (see the module note). `content_of` gives the
/// canonical bytes for a session id, and `last_change_of` its source's last write time. `codec` seals
/// the plaintext for the lease version it is granted and opens stored copies. A session with no content
/// or a failed push is recorded in `failed` and the pass goes on. A successful push records the bytes,
/// the turn fingerprint and the time in state. With `reseal`, an unchanged session is also sent when
/// `needs_reseal`, asked under the session's lease, says its stored record is this machine's and sealed
/// under an older member list. `None` (no verified list) never re-seals.
#[allow(clippy::too_many_arguments)]
pub fn push_due<R, C, T, K>(
    state: &mut State,
    remote: &R,
    space: &str,
    now: f64,
    quiet_secs: f64,
    mut content_of: C,
    mut last_change_of: T,
    codec: &mut K,
    reseal: Option<&ResealCheck>,
) -> TeamPass
where
    R: crate::team_push::TeamRemote,
    C: FnMut(&str) -> Result<Vec<u8>, String>,
    T: FnMut(&str) -> Option<f64>,
    K: Codec,
{
    let no_turns = fingerprint(&[]);
    let mut out = TeamPass::default();
    for id in state.sessions_shared_with(space) {
        let Some(changed) = last_change_of(&id) else { continue };
        let plain = match content_of(&id) {
            Ok(b) => b,
            Err(e) => { out.failed.push((id, e)); continue; }
        };
        // Change is judged on the completed turns of the plaintext journal. Sealing draws a fresh key
        // and nonce every time, so neither sealed bytes nor a metadata append may count. The journal is
        // parsed only when its bytes moved since the record was last checked.
        let bytes = crate::mirror::file_hash_bytes(&plain);
        let rec = state.team_record(space, &id);
        let parsed = if !rec.turns.is_empty() && rec.hash == bytes {
            None
        } else {
            match turns_of(&plain) {
                Ok(t) => Some(t),
                Err(e) => { out.failed.push((id, format!("cannot read the journal: {e}"))); continue; }
            }
        };
        let fp = match &parsed {
            Some(t) => fingerprint(t),
            None => rec.turns.clone(),
        };
        // Migration from bytes-only state: the recorded bytes are the bytes last pushed, so their turns are
        // what the team holds. Record the turns and send nothing; no mass re-push on upgrade.
        if rec.turns.is_empty() && !rec.hash.is_empty() && rec.hash == bytes {
            state.record_team_turns(space, &id, &bytes, &fp);
            out.skipped_unchanged.push(id);
            continue;
        }
        if fp == no_turns {
            out.waiting_for_turn.push(id);
            continue;
        }
        // The user's "keep mine" applies to exactly the local turns and team record they reviewed.
        let forced = state.team_force(space, &id).filter(|f| f.local == fp).cloned();
        let pushed_fp = rec.turns.clone();
        let mut verify = None;
        if forced.is_none() {
            if !pushed_fp.is_empty() && pushed_fp == fp {
                // Unchanged turns: nothing to send, unless the member list moved and the stored record is
                // this machine's own under an older one.
                if rec.hash != bytes {
                    state.record_team_turns(space, &id, &bytes, &fp);
                }
                match reseal.filter(|c| state.team_list(space, &id) < c.list) {
                    Some(c) => verify = Some(c),
                    None => { out.skipped_unchanged.push(id); continue; }
                }
            } else if !is_due(now, changed, quiet_secs, &fp, &pushed_fp) {
                out.skipped_unchanged.push(id);
                continue;
            } else if let Some(c) = state.team_conflict(space, &id).filter(|c| c.local == fp) {
                // Held, and this machine's turns have not changed since: stays paused until they do or
                // the user decides.
                out.refused.push((id, c.describe()));
                continue;
            }
        }
        let local = match parsed {
            Some(t) => t,
            None => match turns_of(&plain) {
                Ok(t) => t,
                Err(e) => { out.failed.push((id, format!("cannot read the journal: {e}"))); continue; }
            },
        };
        // The lease is taken first. A re-seal or a guard check reads the stored record under it, so no write
        // from another member can land between that read and this one. The version the lease grants is
        // the sequence the record is sealed at.
        let floor = state.seen_floor(space, &id);
        let mut resealing = false;
        let mut verdict = Verdict::Push;
        let pushed = push_marked_sealed_if(state, remote, space, &id, |version| {
            if let Some(c) = verify {
                if !needs_reseal(remote, space, &id, floor, c)? {
                    return Ok(None);
                }
                resealing = true;
                return codec.seal(&id, version, &plain).map(Some);
            }
            let Some(stored) = remote.stored(space, &id)
                .map_err(|e| format!("cannot read the stored record: {e:?}"))? else {
                return codec.seal(&id, version, &plain).map(Some);
            };
            if let Some(f) = &forced {
                if sequence_of(&stored) == Some(f.team_seq) {
                    // The copy the user reviewed is still the team's: this machine's copy goes over it.
                    return codec.seal(&id, version, &plain).map(Some);
                }
            }
            match check_stored(codec, &id, floor, &stored, &local, &fp, now) {
                Verdict::Push => codec.seal(&id, version, &plain).map(Some),
                other => { verdict = other; Ok(None) }
            }
        });
        match pushed {
            Ok(true) => {
                state.record_team_push(space, &id, &bytes, now);
                state.set_team_turns(space, &id, &fp);
                state.set_team_conflict(space, &id, None);
                if let Some(c) = reseal {
                    state.set_team_list(space, &id, c.list);
                }
                if resealing { out.resealed.push(id) } else { out.pushed.push(id) }
            }
            // Declined under the lease: nothing was written. The verdict says why.
            Ok(false) => match verdict {
                Verdict::Same => {
                    state.record_team_turns(space, &id, &bytes, &fp);
                    state.set_team_conflict(space, &id, None);
                    out.skipped_unchanged.push(id);
                }
                Verdict::Held(c) => {
                    out.refused.push((id.clone(), c.describe()));
                    state.set_team_conflict(space, &id, Some(c));
                }
                Verdict::Push => {
                    if let Some(c) = verify {
                        state.set_team_list(space, &id, c.list);
                    }
                    out.skipped_unchanged.push(id);
                }
            },
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
