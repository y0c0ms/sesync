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

/// Run one team pass. `content_of` gives canonical journal bytes for a marked session; `last_change_of`
/// gives its source's last write time (seconds). Both come from the caller, which owns the sources.
pub fn run(
    config_dir: &Path,
    state: &mut State,
    now: f64,
    content_of: impl FnMut(&str) -> Result<Vec<u8>, String>,
    last_change_of: impl FnMut(&str) -> Option<f64>,
) -> Result<TeamPass, NotRun> {
    let base = std::env::var("SESYNC_TEAM_URL").map_err(|_| NotRun::NoTeam)?;
    let space = std::env::var("SESYNC_TEAM_SPACE").ok();
    let cred_space = space.clone().or_else(|| single_team(config_dir)).ok_or(NotRun::NoTeam)?;
    let cred = team_join::load(&team_join::credential_path(config_dir, &cred_space)).map_err(|_| NotRun::NoTeam)?;
    let pinned = team_join::trust_of(&cred).ok_or(NotRun::NoTrustRoot)?;

    // Refresh before sealing: records must be signed under the current list version.
    let trust = fetch_and_verify(&base, &cred.token, &cred.machine, &cred_space, &pinned)
        .unwrap_or(pinned);
    let writer = load_or_create(config_dir).map_err(|_| NotRun::NoTeam)?;
    let remote = HttpTeam { base: base.clone(), token: cred.token.clone(), machine: cred.machine.clone() };
    let quiet = team_quiet_secs();

    // Each record is bound to the credential's space, its session id as the conversation, and the
    // lease version it is written under as the sequence.
    let seal = |session: &str, version: u64, plain: &[u8]| -> Result<Vec<u8>, String> {
        seal_for_devices(&writer, &trust, &cred_space, session, version, plain)
            .map_err(|e| format!("cannot seal: {e:?}"))
    };
    Ok(push_due(state, &remote, &cred_space, now, quiet, content_of, last_change_of, seal))
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
