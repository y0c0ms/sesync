//! One team pass for the daemon loop: for a configured team, refresh the trust root, then push each
//! due marked session, sealed for the listed devices. Does nothing when no team is configured, so
//! an install without a team behaves as before.

use crate::device_keys::{load_or_create, DeviceFile};
use crate::sealed::{open_stored_for_writer, seal_for_devices, SealError, Trust};
use crate::state::State;
use crate::team_join;
use crate::team_push::HttpTeam;
use crate::team_sync::{push_due, Codec, ResealCheck, TeamPass, DEFAULT_TEAM_QUIET_SECS};
use crate::team_watch::fetch_and_verify;
use sesync_team::signed_records::Binding;
use std::path::Path;

/// Why a team pass could not run at all. A pass that runs reports per-session results instead.
#[derive(Debug, PartialEq, Eq)]
pub enum NotRun {
    /// No team is configured on this machine (no usable credential, or no server address).
    NoTeam,
    /// The credential is there but has no pinned trust root, so nothing can be sealed for it.
    NoTrustRoot,
    /// The server could not confirm the current list; nothing is pushed on an unverified list.
    ListUnverified,
}

/// Settings read from the environment, as the team side already uses them.
pub fn team_quiet_secs() -> f64 {
    std::env::var("SESYNC_TEAM_QUIET").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_TEAM_QUIET_SECS)
}

/// The daemon's codec: seals for the devices the verified list names, and opens a stored copy the way
/// this device's reader does (`open_stored_for_writer`).
struct DaemonCodec<'a> {
    writer: &'a DeviceFile,
    trust: &'a Trust,
    space: &'a str,
}

impl Codec for DaemonCodec<'_> {
    fn seal(&mut self, conversation: &str, version: u64, plain: &[u8]) -> Result<Vec<u8>, String> {
        seal_for_devices(self.writer, self.trust, self.space, conversation, version, plain)
            .map_err(|e| format!("cannot seal: {e:?}"))
    }

    fn open(&mut self, conversation: &str, floor: u64, stored: &[u8]) -> Result<Vec<u8>, String> {
        let expect = Binding {
            space: self.space.to_string(),
            conversation: conversation.to_string(),
            min_sequence: floor,
        };
        open_stored_for_writer(self.writer, self.trust, &expect, stored).map_err(|e| why_unreadable(&e))
    }
}

/// Why a stored copy could not be read, in words a person can act on. Never the content.
fn why_unreadable(e: &SealError) -> String {
    match e {
        SealError::StaleList { .. } => "it is sealed under an older member list, so the author must re-seal it".into(),
        SealError::UnknownAuthor => "it was written by a device the member list does not name".into(),
        SealError::BadSignature => "its signature does not verify".into(),
        SealError::NotForThisDevice => "it is not sealed to this device".into(),
        SealError::Rollback { .. } => "it is older than a record this machine already accepted".into(),
        SealError::WrongSpace | SealError::WrongConversation => "it is bound to another team or conversation".into(),
        SealError::Malformed | SealError::UnsupportedFormat => "it is not a readable record".into(),
        SealError::TooLarge => "it is larger than the session limit".into(),
        SealError::Crypto(_) => "its content does not decrypt".into(),
    }
}

/// Run a pass for every team this machine belongs to: each team pushes the sessions shared with
/// it, under its own lease and sequence. The canonical bytes for a conversation are built ONCE
/// and reused for every team it is shared with, so a session on two teams is converted once.
/// A team whose credential cannot be read, or that has no server address, is skipped; the other
/// teams are still pushed.
pub fn run(
    config_dir: &Path,
    state: &mut State,
    now: f64,
    mut content_of: impl FnMut(&str) -> Result<Vec<u8>, String>,
    mut last_change_of: impl FnMut(&str) -> Option<f64>,
) -> Result<Vec<TeamPass>, NotRun> {
    let env_base = std::env::var("SESYNC_TEAM_URL").ok();
    let spaces: Vec<String> = match std::env::var("SESYNC_TEAM_SPACE").ok() {
        Some(s) => vec![s],
        None => crate::team_watch::all_teams(config_dir),
    };
    // Build each conversation's canonical bytes once, whatever it is shared with.
    let mut canon = CanonCache::default();
    let mut passes = Vec::new();
    for space in spaces {
        let Ok(cred) = team_join::load(&team_join::credential_path(config_dir, &space)) else { continue };
        // The server the team was joined on; SESYNC_TEAM_URL for an older credential.
        let Some(base) = cred.server.clone().or_else(|| env_base.clone()) else { continue };
        let Some(pinned) = team_join::trust_of(&cred) else { continue };
        // Refresh before sealing: records must be signed under the current list version. A newer
        // list is kept, so the list held here only moves forward; a failure to keep it is the
        // pull's to report, the push still seals under the list it verified.
        let trust = fetch_and_verify(&base, &cred.token, &cred.machine, &space, &pinned).unwrap_or(pinned);
        let _ = team_join::ratchet_list(config_dir, &space, &trust);
        let Ok(writer) = load_or_create(config_dir) else { continue };
        let remote = HttpTeam { base: base.clone(), token: cred.token.clone(), machine: cred.machine.clone() };
        let quiet = team_quiet_secs();
        // After a list change, this machine's unchanged records are sealed again under the list it
        // now holds, so members who hold that list (and any member just added) can open them.
        let reseal = trust.version().ok()
            .map(|list| ResealCheck { list, author: writer.public_signing().to_vec() });
        let mut codec = DaemonCodec { writer: &writer, trust: &trust, space: &space };
        let mut content = |id: &str| canon.get(id, |id| content_of(id));
        passes.push(push_due(state, &remote, &space, now, quiet, content, &mut last_change_of, &mut codec,
                             reseal.as_ref()));
    }
    if passes.is_empty() { return Err(NotRun::NoTeam); }
    Ok(passes)
}

/// Canonical journal bytes, built once per pass: a session shared with more than one team is
/// converted once, not once per team.
#[derive(Default)]
pub struct CanonCache {
    map: std::collections::HashMap<String, Vec<u8>>,
}

impl CanonCache {
    pub fn get(&mut self, id: &str, build: impl FnOnce(&str) -> Result<Vec<u8>, String>) -> Result<Vec<u8>, String> {
        if let Some(b) = self.map.get(id) {
            return Ok(b.clone());
        }
        let b = build(id)?;
        self.map.insert(id.to_string(), b.clone());
        Ok(b)
    }
}

/// Whether a pass ran and what it did, for the daemon's log line. Each failure is named with its
/// reason: a bare count hides, say, that the server no longer accepts this device.
pub fn summary(pass: &TeamPass) -> String {
    let mut s = format!("team push: {} pushed, {} unchanged, {} failed",
        pass.pushed.len(), pass.skipped_unchanged.len(), pass.failed.len());
    if !pass.resealed.is_empty() {
        s.push_str(&format!(", {} re-sealed under the current member list", pass.resealed.len()));
    }
    if !pass.waiting_for_turn.is_empty() {
        s.push_str(&format!(", {} waiting for a first completed turn", pass.waiting_for_turn.len()));
    }
    if !pass.refused.is_empty() {
        s.push_str(&format!(", {} held back (the team copy is not written over)", pass.refused.len()));
    }
    for (id, why) in pass.refused.iter().chain(pass.failed.iter()) {
        s.push_str(&format!("; {id}: {why}"));
    }
    s
}
