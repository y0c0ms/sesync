//! One sync pass and the status view. Ported from daemon/sesync.py one_pass
//! and status. The per-source work is a callback so the pass logic is testable.

use crate::state::State;
use std::path::Path;

pub struct PassSource {
    pub key: String,
    pub title: String,
    /// Set when a turn-taking promotion moved the session here: its mirrors are written in
    /// the same pass, without the quiet window, so no harness shows a stale copy.
    pub urgent: bool,
}

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Run `sync` for every changed source. A failure is counted on that source and
/// the pass continues with the rest. Returns how many sources were attempted.
pub fn run_pass<F>(state: &mut State, sources: &[PassSource], mut sync: F) -> usize
where
    F: FnMut(&PassSource) -> Result<(), String>,
{
    let mut attempted = 0;
    for s in sources {
        attempted += 1;
        match sync(s) {
            Ok(()) => {
                // A later pass that succeeds clears the record; the failure count also drives
                // the retry backoff in scan, so it must not stick around forever.
                if let Some(src) = state.sources.get_mut(&s.key) {
                    src.failures = 0;
                    src.last_failure = 0.0;
                }
            }
            Err(e) => {
                if let Some(src) = state.sources.get_mut(&s.key) {
                    src.failures += 1;
                    src.last_failure = now_secs();
                }
                eprintln!("prepare {} failed: {e}", s.key);
            }
        }
    }
    attempted
}

/// The `--status` view: sources newest first, each followed by its mirrors.
pub fn format_status(state: &State) -> String {
    let mut keys: Vec<&String> = state.sources.keys().collect();
    keys.sort_by(|a, b| {
        state.sources[*b].last_seen_change.partial_cmp(&state.sources[*a].last_seen_change)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out = String::new();
    for key in keys {
        let src = &state.sources[key];
        let name = if src.fmt == "opencode" { key.clone() } else { file_name(key) };
        out.push_str(&format!("{:<9} {}\n", src.fmt, name));
        let mut targets: Vec<&String> = src.mirrors.keys().collect();
        targets.sort();
        for t in targets {
            let m = &src.mirrors[t];
            let shown = if t == "opencode" { m.path.clone() } else { file_name(&m.path) };
            out.push_str(&format!("    -> {:<9} {}\n", t, shown));
        }
        if src.failures > 0 {
            out.push_str(&format!("    failures: {}\n", src.failures));
        }
    }
    out
}

fn file_name(p: &str) -> String {
    Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string())
}
