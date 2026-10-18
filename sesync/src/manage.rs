//! Session management: list, delete and rename. Ported from daemon/hs_manage.py.
//! Every file operation is limited to the session roots, every operation holds
//! the conversation lock, and deleting removes only the picked session.

use crate::lock::{with_claim, ClaimOutcome};
use crate::state::State;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const MAX_TITLE_CHARS: usize = 80;

/// The directories session files may live under.
pub struct Roots {
    pub omp: PathBuf,
    pub pi: PathBuf,
    pub claude: PathBuf,
    /// Gemini's per-project session tree, `<GEMINI_CLI_HOME>/tmp`.
    pub gemini: PathBuf,
}

impl Roots {
    fn all(&self) -> [&Path; 4] {
        [&self.omp, &self.pi, &self.claude, &self.gemini]
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
    /// Last activity, seconds since the epoch; the list is sorted newest first.
    pub updated: f64,
}

impl Group {
    pub fn file(fmt: &str, key: String, title: &str) -> Group {
        Group { fmt: fmt.into(), key, title: title.into(), mirrors: Vec::new(), orphan: false,
                subagents: Vec::new(), updated: 0.0 }
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

/// Titles are read from file contents, and the TUI re-lists every few seconds. The cache
/// keeps an unchanged file (same size and mtime) from being read again; a changed one is
/// re-read, so a rename made in the harness shows up without a restart.
#[derive(Default)]
pub struct TitleCache {
    entries: HashMap<String, (u64, u128, String)>,
}

impl TitleCache {
    fn title(&mut self, path: &str, fmt: &str) -> String {
        let stamp = fs::metadata(path).ok()
            .and_then(|m| m.modified().ok().map(|t| (m.len(), mtime_nanos(t))));
        if let Some((len, ns, title)) = self.entries.get(path) {
            if stamp == Some((*len, *ns)) {
                return title.clone();
            }
        }
        let Some((len, ns)) = stamp else { return "untitled".into() };
        let title = session_title(path, fmt);
        self.entries.insert(path.to_string(), (len, ns, title.clone()));
        title
    }
}

fn mtime_nanos(t: std::time::SystemTime) -> u128 {
    t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// Every source on disk, with the mirrors recorded for it in state. Mirror files
/// are never listed as sources. A source only in state (no file on disk) is kept
/// as an orphan so it can still be cleaned up.
pub fn list_groups(roots: &Roots, state: &State) -> Vec<Group> {
    list_groups_cached(roots, state, &mut TitleCache::default())
}

/// The same list, reusing titles already read for unchanged files. The TUI holds one
/// cache for its lifetime; callers that list once can use [`list_groups`].
pub fn list_groups_cached(roots: &Roots, state: &State, cache: &mut TitleCache) -> Vec<Group> {
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
    // The gemini root is `<gemini>/tmp`; discovery wants the config directory above it.
    for p in crate::discovery::gemini_sessions(&up(&roots.gemini)) {
        on_disk.push((p.display().to_string(), "gemini"));
    }
    for (key, fmt) in on_disk {
        if state.mirror_keys.contains(&key) {
            continue;
        }
        let title = cache.title(&key, fmt);
        let mut g = Group::file(fmt, key.clone(), &title);
        g.updated = fs::metadata(&key).and_then(|m| m.modified()).ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        by_key.insert(key.clone(), g);
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
        g.updated = g.updated.max(src.last_seen_change);
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
    let mut out: Vec<Group> = by_key.into_values().collect();
    out.sort_by(|a, b| b.updated.total_cmp(&a.updated).then_with(|| a.key.cmp(&b.key)));
    out
}

/// The name shown for a session: its title from the file, or a neutral name when it
/// has none. Never the file name, which carries a timestamp.
fn session_title(path: &str, fmt: &str) -> String {
    let Some(text) = crate::titles::read_title_text(Path::new(path)) else { return "untitled".into() };
    let lines: Vec<&str> = text.lines().collect();
    let title = match fmt {
        "claude" => crate::titles::claude_title_from_lines(&lines).0,
        "gemini" => crate::titles::gemini_title_from_lines(&lines).0,
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
///   omp:    the 256-byte title slot (the first line, which omp reads first) is
///           rewritten, and a `title_change` entry is appended — exactly what omp
///           records when the rename is made inside it; the header title is kept in
///           step for readers that fall back to it;
///   claude: a custom-title record is appended (what /rename and -n write);
///   pi:     a session_info record with the name is appended, history kept;
///   gemini: a `$set` summary update is appended, as Gemini's own client renames.
/// The transcript is never rewritten for claude, pi or gemini.
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
    if fmt == "gemini" {
        // Gemini's session file is an event log; its own client renames by appending a
        // `$set` metadata update, which the replay applies to the session summary.
        let mut out = text.to_string();
        if !out.ends_with('\n') && !out.is_empty() { out.push('\n'); }
        out.push_str(&serde_json::json!({
            "$set": { "summary": title },
        }).to_string());
        out.push('\n');
        return out;
    }
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if fmt == "omp" {
        let previous = {
            let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
            crate::titles::omp_title_from_lines(&refs).0
        };
        let now = now_iso_ms();
        let mut slot_written = false;
        let mut header_written = false;
        if let Some(first) = lines.first_mut() {
            // omp always has the slot as the first line. Any title record there is the
            // slot; a file with no slot and no header cannot show the name, and the
            // caller's read-back check turns that into an error.
            let is_slot = serde_json::from_str::<serde_json::Value>(first)
                .ok()
                .and_then(|v| v.get("type").and_then(serde_json::Value::as_str).map(str::to_string))
                .as_deref() == Some("title");
            if is_slot {
                *first = slot_line(title, &now);
                slot_written = true;
            }
        }
        for l in lines.iter_mut() {
            if l.starts_with("{\"type\":\"session\"") {
                if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(l) {
                    v["title"] = serde_json::Value::String(title.into());
                    *l = v.to_string();
                    header_written = true;
                }
            }
        }
        if slot_written || header_written {
            // omp chains each entry to the one before it; parentId is that entry's id.
            let parent = lines.iter().rev().find_map(|l| {
                serde_json::from_str::<serde_json::Value>(l).ok()
                    .and_then(|v| v.get("id").and_then(serde_json::Value::as_str).map(str::to_string))
            });
            let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, format!("{now}|{previous}|{title}").as_bytes())
                .simple().to_string();
            let mut rec = serde_json::Map::new();
            rec.insert("type".into(), serde_json::Value::String("title_change".into()));
            rec.insert("id".into(), serde_json::Value::String(id[..8].to_string()));
            if let Some(p) = parent {
                rec.insert("parentId".into(), serde_json::Value::String(p));
            }
            rec.insert("source".into(), serde_json::Value::String("user".into()));
            rec.insert("timestamp".into(), serde_json::Value::String(now.clone()));
            if !previous.is_empty() {
                rec.insert("previousTitle".into(), serde_json::Value::String(previous));
            }
            rec.insert("title".into(), serde_json::Value::String(title.into()));
            lines.push(serde_json::Value::Object(rec).to_string());
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

const TITLE_SLOT_BYTES: usize = 256;

/// omp's fixed-width title slot line (exactly 256 bytes including the newline),
/// matching the serializer in omp 18.8.6: keys in this order, `source` is
/// `"auto"` or `"user"`, and the title is trimmed until it fits.
fn slot_line(title: &str, updated_at: &str) -> String {
    let mut t = title.to_string();
    loop {
        let mut m = serde_json::Map::new();
        m.insert("type".into(), serde_json::Value::String("title".into()));
        m.insert("v".into(), serde_json::Value::from(1));
        m.insert("title".into(), serde_json::Value::String(t.clone()));
        m.insert("source".into(), serde_json::Value::String("user".into()));
        m.insert("updatedAt".into(), serde_json::Value::String(updated_at.into()));
        m.insert("pad".into(), serde_json::Value::String(String::new()));
        let base = serde_json::to_string(&m).unwrap();
        if base.len() + 1 <= TITLE_SLOT_BYTES {
            let pad = TITLE_SLOT_BYTES - 1 - base.len();
            m.insert("pad".into(), serde_json::Value::String(" ".repeat(pad)));
            return serde_json::to_string(&m).unwrap();
        }
        if t.pop().is_none() {
            return base;
        }
    }
}

/// UTC now as `2026-10-18T17:01:16.266Z`, the shape omp stamps on entries
/// (days-from-civil inverse; no timezone other than Z is ever written here).
fn now_iso_ms() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let (month, year) = if mp < 10 { (mp + 3, y) } else { (mp - 9, y + 1) };
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{mi:02}:{s:02}.{:03}Z", d.subsec_millis())
}

pub fn conversation_id(key: &str) -> String {
    let name = file_name(key);
    let stem = name.strip_suffix(".jsonl").unwrap_or(&name);
    stem.rsplit('_').next().unwrap_or(stem).to_string()
}
