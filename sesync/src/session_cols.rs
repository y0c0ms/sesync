//! The right-hand columns of the sessions list: team state, last team sync, when the session is next
//! due for the team, mirror targets, and when the source last changed. Pure functions over values the
//! caller reads from state, so every rule is testable without a terminal.
//!
//! "Next" is the earliest time a marked session becomes due (its last change plus the team quiet
//! period). The daemon has no fixed timer, so this is an honest estimate, not a scheduled time.

/// What the columns need, read from state by the caller.
pub struct Input {
    pub marked: bool,
    /// When the session was last pushed to the team, if it ever was.
    pub pushed_at: Option<f64>,
    /// When the source last changed (seconds since the epoch). 0 when unknown.
    pub last_change: f64,
    pub now: f64,
    pub quiet_secs: f64,
    /// Harness formats this session is mirrored into.
    pub mirror_targets: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Cols {
    /// "-" (not for the team), "marked" (will be pushed), or "shared" (pushed at least once).
    pub team: String,
    /// "never" or a relative time, e.g. "3m ago".
    pub last_sync: String,
    /// "-" when not marked, "in 2m" while quiet, "next tick" once due.
    pub next_sync: String,
    /// Comma-separated harness formats, or "-".
    pub mirrors: String,
    /// Relative time since the source last changed, or "-" when unknown.
    pub updated: String,
}

pub fn columns(i: &Input) -> Cols {
    let team = match (i.marked, i.pushed_at.is_some()) {
        (_, true) => "shared",
        (true, false) => "marked",
        (false, false) => "-",
    };
    let last_sync = match i.pushed_at {
        Some(t) => relative(i.now, t),
        None => "never".to_string(),
    };
    let next_sync = if !i.marked {
        "-".to_string()
    } else {
        let due_at = i.last_change + i.quiet_secs;
        if i.now < due_at {
            format!("in {}", short_span((due_at - i.now) as u64))
        } else {
            "next tick".to_string()
        }
    };
    let mirrors = if i.mirror_targets.is_empty() {
        "-".to_string()
    } else {
        let mut t = i.mirror_targets.clone();
        t.sort();
        t.join(",")
    };
    let updated = if i.last_change > 0.0 { relative(i.now, i.last_change) } else { "-".to_string() };
    Cols { team: team.into(), last_sync, next_sync, mirrors, updated }
}

/// "5s ago", "3m ago", "2h ago", "4d ago". A time in the future reads as "just now".
pub fn relative(now: f64, then: f64) -> String {
    let secs = (now - then).max(0.0) as u64;
    format!("{} ago", short_span(secs))
}

/// A compact span: "5s", "3m", "2h", "4d".
pub fn short_span(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// Fit text into a column of `width` characters: pad with spaces, or cut and end with "…".
pub fn fit(text: &str, width: usize) -> String {
    let n = text.chars().count();
    if n <= width {
        format!("{text:<width$}")
    } else if width == 0 {
        String::new()
    } else {
        let mut s: String = text.chars().take(width - 1).collect();
        s.push('…');
        s
    }
}
