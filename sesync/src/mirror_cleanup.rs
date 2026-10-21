//! Clean up the mirror copies sesync wrote into other harness stores before it stopped writing
//! them. A mirror is removed only when that loses nothing:
//!   - sesync recorded it in its state, so sesync wrote it, not the user;
//!   - it is still exactly what sesync wrote (same hash), so there is no newer version;
//!   - its source is still on disk, so the session is not lost.
//! Anything else is kept, with the reason. Planning reads and changes nothing.

use crate::lock::{with_claim, ClaimOutcome};
use crate::mirror::{copy_hash, remove_copy};
use crate::state::State;
use std::path::Path;

#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Sesync's own copy, unchanged, with its source present: safe to delete.
    Remove,
    /// Kept, with the reason shown to the user.
    Keep(String),
    /// Already gone from disk: only the state record needs clearing.
    Gone,
}

#[derive(Debug, PartialEq)]
pub struct Planned {
    pub source: String,
    pub target: String,
    pub path: String,
    /// The hash sesync recorded when it wrote the mirror.
    pub installed_hash: String,
    pub verdict: Verdict,
}

/// Decide, for every mirror recorded in state, whether it can go. Reads only.
pub fn plan(state: &State) -> Vec<Planned> {
    let mut out: Vec<Planned> = Vec::new();
    for (key, src) in &state.sources {
        for (target, m) in &src.mirrors {
            out.push(Planned {
                source: key.clone(),
                target: target.clone(),
                path: m.path.clone(),
                installed_hash: m.installed_hash.clone(),
                verdict: verdict_for(key, target, &m.path, &m.installed_hash),
            });
        }
    }
    out.sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));
    out
}

fn verdict_for(source: &str, target: &str, path: &str, installed_hash: &str) -> Verdict {
    if crate::harness::is_store(target) {
        return Verdict::Keep(format!("{target} keeps this session in its own store: remove it through {target}, not by file"));
    }
    let p = Path::new(path);
    if !p.exists() {
        return Verdict::Gone;
    }
    if installed_hash.is_empty() {
        return Verdict::Keep("no recorded hash, so it cannot be shown unchanged".into());
    }
    match copy_hash(target, p) {
        Err(e) => Verdict::Keep(format!("cannot read: {e}")),
        Ok(h) if h != installed_hash => Verdict::Keep("changed since sesync wrote it: it has a newer version".into()),
        Ok(_) if !Path::new(source).exists() => Verdict::Keep("source is missing: this may be the only copy".into()),
        Ok(_) => Verdict::Remove,
    }
}

/// Run a plan. Each removal runs under the conversation's claim and re-checks the hash first, so a
/// file that changed after planning is kept. Returns one log line per mirror.
pub fn apply(state: &mut State, planned: &[Planned], lock_dir: &Path, sesync_lock: &Path) -> Vec<String> {
    let mut log = Vec::new();
    for p in planned {
        match &p.verdict {
            Verdict::Keep(why) => log.push(format!("kept    {} {}: {why}", p.target, p.path)),
            Verdict::Gone => {
                forget(state, p);
                log.push(format!("cleared {} {} (already gone)", p.target, p.path));
            }
            Verdict::Remove => {
                // The conversation's stable id: the name every copy's lock alias points at.
                let conv = state.team_id_of(&p.source);
                let path = Path::new(&p.path);
                let recorded = p.installed_hash.clone();
                let outcome = with_claim(sesync_lock, Some(lock_dir), &conv, "cleanup", std::process::id(), &p.source, || {
                    match copy_hash(&p.target, path) {
                        Ok(h) if h == recorded => remove_copy(&p.target, path).map_err(|e| e.to_string()),
                        Ok(_) => Err("changed since it was planned".to_string()),
                        Err(e) => Err(format!("cannot read: {e}")),
                    }
                });
                match outcome {
                    ClaimOutcome::Skipped(holder) => log.push(format!("kept    {} {}: held by {holder}", p.target, p.path)),
                    ClaimOutcome::Ran(Ok(())) => {
                        forget(state, p);
                        log.push(format!("removed {} {}", p.target, p.path));
                    }
                    ClaimOutcome::Ran(Err(e)) => log.push(format!("kept    {} {}: {e}", p.target, p.path)),
                }
            }
        }
    }
    log
}

/// The mirrors the user no longer wants: every recorded mirror whose target has left
/// `chosen_targets`. Un-choosing a target is how a session stops being mirrored somewhere, and
/// nothing else drops that copy: `plan()` only sees what is still recorded, and for a harness
/// that keeps its conversations in its own database it declines anyway (remove it through the
/// harness, not by file).
///
/// Verdicts are the same ones `plan()` uses, so a file that changed since sesync wrote it, or
/// whose source is gone, is still kept. The store-harness verdict is the signal for the caller to
/// remove the copy through the harness and forget the record.
pub fn plan_unchosen(state: &State) -> Vec<Planned> {
    let mut out: Vec<Planned> = Vec::new();
    for (key, src) in &state.sources {
        for (target, m) in &src.mirrors {
            if src.chosen_targets.contains(target) {
                continue;
            }
            out.push(Planned {
                source: key.clone(),
                target: target.clone(),
                path: m.path.clone(),
                installed_hash: m.installed_hash.clone(),
                verdict: verdict_for(key, target, &m.path, &m.installed_hash),
            });
        }
    }
    out.sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));
    out
}

/// Stop claiming a mirror without removing the copy. This is what an un-chosen target gets when
/// the copy cannot safely be deleted - it changed since sesync wrote it, has no recorded hash, or
/// its source is gone. Keeping the record would leave sesync pointing at a copy it will never
/// update again, shown as a mirror forever; releasing it lets the file be a session of its own and
/// makes the TUI honest. Returns one log line per mirror.
pub fn release(state: &mut State, planned: &[Planned]) -> Vec<String> {
    let mut log = Vec::new();
    for p in planned {
        let why = match &p.verdict {
            Verdict::Keep(r) => r.clone(),
            _ => "not removed".to_string(),
        };
        forget(state, p);
        log.push(format!("released {} {} (kept the copy: {why})", p.target, p.path));
    }
    log
}

/// Drop a mirror's bookkeeping: its record on the source and its `mirror_keys` entry. Used after
/// the copy itself is gone, whichever way it went.
pub fn forget(state: &mut State, p: &Planned) {
    if let Some(src) = state.sources.get_mut(&p.source) {
        src.mirrors.remove(&p.target);
    }
    state.mirror_keys.remove(&p.path);
}
