//! Harnesses sesync no longer syncs. Removing a harness moves it from `ALL` to `RETIRED`, with the
//! shape of its conversation files: on every load the daemon and the TUI release what an older
//! run recorded for it (`State::retire_harness`). Its sources move to a surviving copy, its copies
//! stop being targets, and none of its files is ever touched.

use std::path::Path;

/// A harness sesync used to sync.
pub struct Retired {
    pub id: &'static str,
    /// Whether a path has the shape of one of its conversation files, so a path sesync once
    /// recorded as a copy is forgotten even when no source names it any more.
    pub is_session_path: fn(&str) -> bool,
}

/// Every retired harness. Gemini CLI was replaced by Antigravity on 2026-06-18.
pub static RETIRED: &[Retired] = &[Retired { id: "gemini", is_session_path: gemini_session_path }];

/// The shape of a session file the Gemini CLI kept, `<...>/tmp/<project>/chats/
/// session-<stamp>-<short>.jsonl`.
fn gemini_session_path(p: &str) -> bool {
    let path = Path::new(p);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let parent = path.parent().and_then(Path::file_name).and_then(|n| n.to_str()).unwrap_or("");
    parent == "chats" && name.starts_with("session-") && name.ends_with(".jsonl")
}
