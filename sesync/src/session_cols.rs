//! The right-hand columns of the sessions list: the mirror targets, when the session last reached
//! them (SYNCED), team state, and when the source last changed. Also the text helpers every view
//! draws with: `display_safe` turns session text into one line of glyphs whose width every
//! terminal agrees on, and `fit` cuts or pads it to a column by the width ratatui places glyphs
//! with. Pure functions over values the caller reads from state, so every rule is testable
//! without a terminal.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// How a mirror target is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorState {
    /// Written, and no write failed since.
    Mirrored,
    /// Chosen, not written yet.
    Pending,
    /// The last write was put off because the conversation was open in a harness (its lock, or
    /// Antigravity holding the database). The daemon retries on its own.
    Busy,
    /// The last write failed.
    Failing,
}

/// The mark a state carries after the target's name in the MIRRORS column.
pub fn mirror_mark(state: MirrorState) -> &'static str {
    match state {
        MirrorState::Mirrored => "",
        MirrorState::Pending => "?",
        MirrorState::Busy => "~",
        MirrorState::Failing => "!",
    }
}

/// Whether a recorded write problem is the transient kind: the target was in use and the daemon
/// retries by itself. Both such messages say so (`mirror_write`, `install`).
pub fn is_transient(problem: &str) -> bool {
    problem.contains("will retry")
}

/// What the columns need, read from state by the caller.
pub struct Input {
    pub marked: bool,
    /// When the session was last pushed to the team, if it ever was.
    pub pushed_at: Option<f64>,
    /// When the source last changed (seconds since the epoch). 0 when unknown.
    pub last_change: f64,
    pub now: f64,
    /// Harness formats this session is mirrored into, with how each one is doing.
    pub mirror_targets: Vec<(String, MirrorState)>,
    /// The oldest known successful write among the targets (seconds since the epoch): every
    /// target has the session at least as new as this. 0 when no write time is known.
    pub synced_at: f64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Cols {
    /// "-" (not for the team), "marked" (will be pushed), or "shared" (pushed at least once).
    pub team: String,
    /// "-" with no mirror target, "failing" while a target's last write failed, "waiting" while
    /// one is unwritten or busy, otherwise how long ago every target last took the session ("ok"
    /// when they all hold it but no write time is known yet).
    pub synced: String,
    /// Comma-separated harness labels with their mark (`pi`, `claude?`, `agy~`, `omp!`), or "-".
    pub mirrors: String,
    /// The same targets, one entry each, sorted by name, for drawing them in colour.
    pub mirror_parts: Vec<(String, MirrorState)>,
    /// Relative time since the source last changed, or "-" when unknown.
    pub updated: String,
}

pub fn columns(i: &Input) -> Cols {
    let team = match (i.marked, i.pushed_at.is_some()) {
        (_, true) => "shared",
        (true, false) => "marked",
        (false, false) => "-",
    };
    let synced = if i.mirror_targets.is_empty() {
        "-".to_string()
    } else if i.mirror_targets.iter().any(|(_, s)| *s == MirrorState::Failing) {
        "failing".to_string()
    } else if i.mirror_targets.iter().any(|(_, s)| matches!(s, MirrorState::Pending | MirrorState::Busy)) {
        "waiting".to_string()
    } else if i.synced_at > 0.0 {
        relative(i.now, i.synced_at)
    } else {
        "ok".to_string()
    };
    let mut mirror_parts = i.mirror_targets.clone();
    mirror_parts.sort_by(|a, b| a.0.cmp(&b.0));
    let mirrors = if mirror_parts.is_empty() {
        "-".to_string()
    } else {
        mirror_parts.iter()
            .map(|(t, s)| format!("{}{}", crate::harness::label(t), mirror_mark(*s)))
            .collect::<Vec<_>>()
            .join(",")
    };
    let updated = if i.last_change > 0.0 { relative(i.now, i.last_change) } else { "-".to_string() };
    Cols { team: team.into(), synced, mirrors, mirror_parts, updated }
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

/// Characters that hide text or change how a line is drawn: control characters, zero-width and
/// bidirectional-control characters, and the byte order mark.
fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(c as u32,
            0x00AD | 0x061C | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E
            | 0x2060..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB)
}

/// Glyphs whose width terminals disagree on, so drawing one shifts the rest of its line and
/// leaves stale characters behind: emoji and pictographs; the symbol blocks that turn into emoji
/// (U+2696 followed by U+FE0F is two cells in one terminal and one in another); the selectors,
/// joiner, keycap and tag marks that build emoji sequences; and private-use glyphs (icon fonts).
pub fn is_unsafe_glyph(c: char) -> bool {
    matches!(c as u32,
        0x200D | 0xFE0E | 0xFE0F | 0x20D0..=0x20FF
        | 0x231A..=0x231B | 0x23E9..=0x23F3 | 0x23F8..=0x23FA | 0x25FD..=0x25FE
        | 0x2600..=0x27BF | 0x2B00..=0x2BFF
        | 0xE000..=0xF8FF
        | 0x1F000..=0x1FAFF
        | 0xE0000..=0xE007F
        | 0xF0000..=0x10FFFF)
}

/// Session text as one line that draws the same in every terminal: hidden characters and
/// unsafe glyphs dropped, every run of whitespace (tabs and newlines included) one space, the
/// ends trimmed. For display only; stored titles and the rename input keep what the user typed.
pub fn display_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if is_hidden(c) || is_unsafe_glyph(c) {
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(c);
    }
    out
}

/// The width of a text in terminal cells, as ratatui counts it when it places the glyphs.
pub fn text_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Fit text into a column of `width` cells: pad with spaces, or cut and end with "…" so the
/// result is exactly `width` cells wide.
pub fn fit(text: &str, width: usize) -> String {
    let w = text_width(text);
    if w <= width {
        return format!("{text}{}", " ".repeat(width - w));
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > width - 1 {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    used += 1;
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}
