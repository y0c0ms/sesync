//! Session management: list, delete and rename. Ported from daemon/hs_manage.py.
//! Every file operation is limited to the session roots, every operation holds
//! the conversation lock, and deleting removes only the picked session.

use crate::lock::{with_claim, ClaimOutcome};
use crate::state::State;
use std::fs;
use std::path::{Path, PathBuf};

pub const MAX_TITLE_CHARS: usize = 80;

/// The three directories session files may live under.
pub struct Roots {
    pub omp: PathBuf,
    pub pi: PathBuf,
    pub claude: PathBuf,
}

impl Roots {
    fn all(&self) -> [&Path; 3] {
        [&self.omp, &self.pi, &self.claude]
    }

    /// True only when `path` resolves under one of the roots. Canonicalising first
    /// stops `..` and symlinks from escaping.
    pub fn contains(&self, path: &Path) -> bool {
        let Ok(resolved) = path.canonicalize() else { return false };
        self.all().iter().any(|r| r.canonicalize().map(|rr| resolved.starts_with(rr)).unwrap_or(false))
    }
}

#[derive(Debug, Clone)]
pub struct Group {
    pub fmt: String,
    pub key: String,
    pub title: String,
    pub mirrors: Vec<(String, String)>,
    /// Recorded in state but no longer on disk. Its mirrors can still be deleted.
    pub orphan: bool,
    /// Subagent transcripts of this session, read-only: shown, never synced or deleted.
    pub subagents: Vec<String>,
}

impl Group {
    pub fn file(fmt: &str, key: String, title: &str) -> Group {
        Group { fmt: fmt.into(), key, title: title.into(), mirrors: Vec::new(), orphan: false,
                subagents: Vec::new() }
    }
    pub fn add_mirror(&mut self, target: &str, path: String) {
        self.mirrors.push((target.into(), path));
    }
}

/// Trim and check a new name: 1 to 80 characters, no control characters.
pub fn validate_title(title: &str) -> Result<String, String> {
    let t = title.trim();
    if t.is_empty() {
        return Err("title is empty".into());
    }
    if t.chars().any(|c| (c as u32) < 32 || c as u32 == 127) {
        return Err("title contains control characters".into());
    }
    if t.chars().count() > MAX_TITLE_CHARS {
        return Err(format!("title is longer than {MAX_TITLE_CHARS} characters"));
    }
    Ok(t.to_string())
}

/// Every source on disk, with the mirrors recorded for it in state. Mirror files
/// are never listed as sources. A source only in state (no file on disk) is kept
/// as an orphan so it can still be cleaned up.
pub fn list_groups(roots: &Roots, state: &State) -> Vec<Group> {
    let mut by_key: std::collections::BTreeMap<String, Group> = std::collections::BTreeMap::new();

    // Files on disk: omp and Pi journals, plus Claude transcripts.
    let mut on_disk: Vec<(String, &str)> = Vec::new();
    // The roots are `<agent>/sessions` and `<claude>/projects`; discovery takes the
    // agent or config directory above them, so step up one level.
    let up = |p: &PathBuf| p.parent().map(Path::to_path_buf).unwrap_or_else(|| p.clone());
    for p in crate::discovery::jsonl_sessions(&up(&roots.omp)) {
        on_disk.push((p.display().to_string(), "omp"));
    }
    for p in crate::discovery::jsonl_sessions(&up(&roots.pi)) {
        on_disk.push((p.display().to_string(), "pi"));
    }
    for p in crate::discovery::claude_sessions(&up(&roots.claude)) {
        on_disk.push((p.display().to_string(), "claude"));
    }
    for (key, fmt) in on_disk {
        if state.mirror_keys.contains(&key) {
            continue;
        }
        let title = session_title(&key, fmt);
        by_key.insert(key.clone(), Group::file(fmt, key.clone(), &title));
    }

    // Sources recorded in state, with their mirrors. Keep orphans too.
    for (key, src) in &state.sources {
        if state.mirror_keys.contains(key) {
            continue;
        }
        let g = by_key.entry(key.clone()).or_insert_with(|| {
            // Prefer the name the daemon recorded (O-penCode sessions have no file name to show).
            let name = if src.title.is_empty() { file_name(key) } else { src.title.clone() };
            let mut g = Group::file(&src.fmt, key.clone(), &name);
            // Not found among the files on disk: keep it, flagged as an orphan.
            g.orphan = true;
            g
        });
        for (t, m) in &src.mirrors {
            g.add_mirror(t, m.path.clone());
        }
    }
    // Attach each session's recorded subagent transcripts, read-only.
    for (key, g) in by_key.iter_mut() {
        if let Some(subs) = state.subagents.get(&session_id_of(key)) {
            g.subagents = subs.clone();
        }
    }
    by_key.into_values().collect()
}

/// The name shown for a session: its title from the file, or a neutral name when it
/// has none. Never the file name, which carries a timestamp.
fn session_title(path: &str, fmt: &str) -> String {
    let Ok(text) = std::fs::read_to_string(path) else { return "untitled".into() };
    let lines: Vec<&str> = text.lines().collect();
    let title = match fmt {
        "claude" => crate::titles::claude_title_from_lines(&lines).0,
        _ => crate::titles::omp_title_from_lines(&lines).0,
    };
    if title.is_empty() { "untitled".into() } else { title }
}

/// The session id a source file belongs to: the uuid in its name (omp/Pi or Claude), or
/// the file stem for anything else. Subagent lists are keyed by this id.
/// The session id a group is marked and pushed under. Shared so every part names a session the same way.
pub fn session_id_of(key: &str) -> String {
    let name = file_name(key);
    let stem = name.strip_suffix(".jsonl").unwrap_or(&name);
    if let Some((_, id)) = stem.rsplit_once('_') {
        if id.len() == 36 {
            return id.to_string();
        }
    }
    stem.to_string()
}

fn file_name(p: &str) -> String {
    Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.into())
}

/// Delete a source and its mirrors. Refused if the source or any mirror is outside
/// the roots. Returns a log of what was removed, mirrors first.
pub fn delete_group(roots: &Roots, g: &Group, lock_dir: &Path, sesync_lock: &Path) -> Result<Vec<String>, String> {
    let src = PathBuf::from(&g.key);
    if !roots.contains(&src) {
        return Err(format!("source outside session roots, not deleting: {}", src.display()));
    }
    // Only the picked session is removed. Mirror copies in other harnesses are never touched here;
    // `sesync mirrors` handles those, and only when that loses nothing.
    let conv = conversation_id(&g.key);
    let outcome = with_claim(sesync_lock, Some(lock_dir), &conv, "tui", std::process::id(), &g.key, || {
        let mut log = Vec::new();
        if src.exists() {
            fs::remove_file(&src).map_err(|e| e.to_string())?;
        }
        log.push(format!("source {}", g.key));
        Ok::<Vec<String>, String>(log)
    });
    match outcome {
        ClaimOutcome::Skipped(h) => Err(format!("conversation is in use by {h}; try again when its turn ends")),
        ClaimOutcome::Ran(r) => r,
    }
}

/// Rename a source by rewriting only its title record. Refused outside the roots.
pub fn rename_group(roots: &Roots, g: &Group, title: &str, lock_dir: &Path, sesync_lock: &Path) -> Result<String, String> {
    let new = validate_title(title)?;
    if g.orphan {
        return Err("source is not on disk; nothing to rename".into());
    }
    let path = PathBuf::from(&g.key);
    if !roots.contains(&path) {
        return Err(format!("source outside session roots, not renaming: {}", path.display()));
    }
    let conv = conversation_id(&g.key);
    let outcome = with_claim(sesync_lock, Some(lock_dir), &conv, "tui", std::process::id(), &g.key, || {
        let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        // Claude session files are named by their uuid; that is the session id.
        let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        let rewritten = rewrite_title(&text, &g.fmt, &new, &session_id);
        let tmp = path.with_extension("rename.tmp");
        fs::write(&tmp, &rewritten).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        // A rename that leaves the new name out of the file must not report success.
        let check = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        if !check.contains(&new) {
            return Err(format!("rename did not write the new title into {}", path.display()));
        }
        Ok(())
    });
    match outcome {
        ClaimOutcome::Skipped(h) => Err(format!("conversation is in use by {h}")),
        ClaimOutcome::Ran(r) => r.map(|_| new),
    }
}

/// The file content after a rename, for the format's native rule:
///   omp:    the session header's title is replaced (omp reads it first);
///   claude: a custom-title record is appended (what /rename and -n write);
///   pi:     a session_info record with the name is appended, history kept.
/// The transcript is never rewritten for claude or pi.
fn rewrite_title(text: &str, fmt: &str, title: &str, session_id: &str) -> String {
    if fmt == "claude" {
        let mut out = text.to_string();
        if !out.ends_with('\n') && !out.is_empty() { out.push('\n'); }
        out.push_str(&serde_json::json!({
            "type": "custom-title", "customTitle": title, "sessionId": session_id,
        }).to_string());
        out.push('\n');
        return out;
    }
    if fmt == "pi" {
        let mut out = text.to_string();
        if !out.ends_with('\n') && !out.is_empty() { out.push('\n'); }
        out.push_str(&serde_json::json!({
            "type": "session_info", "name": title,
        }).to_string());
        out.push('\n');
        return out;
    }
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if fmt == "omp" {
        // The header carries the title; a `title` record is the slot omp reads first.
        for l in lines.iter_mut() {
            if l.starts_with("{\"type\":\"session\"") {
                if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(l) {
                    v["title"] = serde_json::Value::String(title.into());
                    *l = v.to_string();
                }
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

pub fn conversation_id(key: &str) -> String {
    let name = file_name(key);
    let stem = name.strip_suffix(".jsonl").unwrap_or(&name);
    stem.rsplit('_').next().unwrap_or(stem).to_string()
}
