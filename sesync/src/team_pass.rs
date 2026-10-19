//! One team pass for the daemon loop: for a configured team, refresh the trust root, then push each
//! due marked session, sealed for the listed devices. Does nothing when no team is configured, so
//! an install without a team behaves as before.

use crate::device_keys::load_or_create;
use crate::sealed::seal_for_devices;
use crate::state::State;
use crate::team_join;
use crate::team_push::HttpTeam;
use crate::team_sync::{push_due, TeamPass, DEFAULT_TEAM_QUIET_SECS};
use crate::team_watch::fetch_and_verify;
use std::path::Path;

/// Why a team pass could not run at all. A pass that runs reports per-session results instead.
#[derive(Debug, PartialEq, Eq)]
pub enum NotRun {
    /// No team is configured on this machine (no credential, or no server address).
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

/// Run a pass for every team this machine belongs to: each team pushes the sessions shared with
/// it, under its own lease and sequence. The canonical bytes for a conversation are built ONCE
/// and reused for every team it is shared with, so a session on two teams is converted once.
pub fn run(
    config_dir: &Path,
    state: &mut State,
    now: f64,
    mut content_of: impl FnMut(&str) -> Result<Vec<u8>, String>,
    mut last_change_of: impl FnMut(&str) -> Option<f64>,
) -> Result<Vec<TeamPass>, NotRun> {
    let base = std::env::var("SESYNC_TEAM_URL").map_err(|_| NotRun::NoTeam)?;
    let spaces: Vec<String> = match std::env::var("SESYNC_TEAM_SPACE").ok() {
        Some(s) => vec![s],
        None => {
            let all = crate::team_watch::all_teams(config_dir);
            if all.is_empty() { single_team(config_dir).into_iter().collect() } else { all }
        }
    };
    if spaces.is_empty() { return Err(NotRun::NoTeam); }
    // Build each conversation's canonical bytes once, whatever it is shared with.
    let mut canon = CanonCache::default();
    let mut passes = Vec::new();
    for space in spaces {
        let cred = team_join::load(&team_join::credential_path(config_dir, &space)).map_err(|_| NotRun::NoTeam)?;
        let Some(pinned) = team_join::trust_of(&cred) else { continue };
        // Refresh before sealing: records must be signed under the current list version.
        let trust = fetch_and_verify(&base, &cred.token, &cred.machine, &space, &pinned).unwrap_or(pinned);
        let Ok(writer) = load_or_create(config_dir) else { continue };
        let remote = HttpTeam { base: base.clone(), token: cred.token.clone(), machine: cred.machine.clone() };
        let quiet = team_quiet_secs();
        let seal = |session: &str, version: u64, plain: &[u8]| -> Result<Vec<u8>, String> {
            seal_for_devices(&writer, &trust, &space, session, version, plain)
                .map_err(|e| format!("cannot seal: {e:?}"))
        };
        let mut content = |id: &str| canon.get(id, |id| content_of(id));
        passes.push(push_due(state, &remote, &space, now, quiet, content, &mut last_change_of, seal));
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

/// The one credential on this machine, when there is exactly one team; otherwise none.
fn single_team(config_dir: &Path) -> Option<String> {
    let dir = config_dir.join("teams");
    let mut names = std::fs::read_dir(&dir).ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".cred").map(str::to_string));
    let first = names.next()?;
    if names.next().is_some() { None } else { Some(first) }
}

/// Whether a pass ran and what it did, for the daemon's log line.
pub fn summary(pass: &TeamPass) -> String {
    format!("team push: {} pushed, {} unchanged, {} failed",
        pass.pushed.len(), pass.skipped_unchanged.len(), pass.failed.len())
}
