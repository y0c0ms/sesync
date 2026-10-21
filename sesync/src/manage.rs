//! Session management: list, delete and rename. Ported from daemon/hs_manage.py.
//! Every file operation is limited to the session roots, every operation holds
//! the conversation lock, and deleting removes only the picked session.

use crate::harness::{self, Kind, Location, Locations, Tools};
use crate::lock::{with_claim, ClaimOutcome};
use crate::state::State;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const MAX_TITLE_CHARS: usize = 80;

/// Where the TUI finds and edits conversations: every harness's location, plus the user's home
/// and the converter (some harnesses rename through it).
pub struct Roots {
    pub locations: Locations,
    pub home: PathBuf,
    pub sesync_conv: PathBuf,
}

impl Roots {
    /// Every harness under `dir/<id>`: `dir/omp/sessions/...`, `dir/claude/projects/...`, a store
    /// harness's database at `dir/<id>/<id>.db`. The layout sandboxes and tests use; no
    /// environment variable is read.
    pub fn under(dir: &Path, sesync_conv: PathBuf) -> Roots {
        let mut locations = Locations::default();
        for h in harness::ALL {
            let root = match h.kind() {
                Kind::Files => dir.join(h.id()),
                Kind::Store => dir.join(h.id()).join(format!("{}.db", h.id())),
            };
            locations.set(h.id(), Location { root, cli: None });
        }
        Roots { locations, home: dir.to_path_buf(), sesync_conv }
    }

    /// True only when `path` resolves inside a file harness's conversation folder.
    pub fn contains(&self, path: &Path) -> bool {
        self.locations.owns(path)
    }

    fn tools(&self) -> Tools<'_> {
        Tools { home: &self.home, sesync_conv: &self.sesync_conv }
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
        let h = harness::get(fmt);
        let stamp = h.map_or_else(|| harness::stat(Path::new(path)), |h| h.title_stamp(Path::new(path)));
        if let Some((len, ns, title)) = self.entries.get(path) {
            if stamp == Some((*len, *ns)) {
                return title.clone();
            }
        }
        let Some((len, ns)) = stamp else { return "untitled".into() };
        let title = h.map(|h| h.title(Path::new(path))).filter(|t| !t.is_empty()).unwrap_or_else(|| "untitled".into());
        self.entries.insert(path.to_string(), (len, ns, title.clone()));
        title
    }
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

    // Files on disk, every file harness's conversations.
    let mut on_disk: Vec<(String, &str)> = Vec::new();
    for h in harness::ALL.iter().filter(|h| h.kind() == Kind::Files) {
        for p in h.sessions(roots.locations.of(h.id())) {
            on_disk.push((p.display().to_string(), h.id()));
        }
    }
    // A copy sesync keeps is shown under its source, never as a session of its own: a row for it
    // would let the user pick targets for a mirror, which makes it a source of its own copies.
    let recorded = state.mirror_paths();
    for (key, fmt) in on_disk {
        if state.mirror_keys.contains(&key) || recorded.contains(&key) {
            continue;
        }
        let title = cache.title(&key, fmt);
        let mut g = Group::file(fmt, key.clone(), &title);
        g.updated = crate::mirror::copy_mtime(fmt, Path::new(&key));
        by_key.insert(key.clone(), g);
    }

    // Sources recorded in state, with their mirrors. Keep orphans too.
    for (key, src) in &state.sources {
        if state.mirror_keys.contains(key) || state.is_recorded_mirror(key) {
            continue;
        }
        let g = by_key.entry(key.clone()).or_insert_with(|| {
            // The name the daemon recorded, else "untitled": never a file name or a store id.
            let name = if src.title.is_empty() { "untitled".to_string() } else { src.title.clone() };
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

/// The session id a source file belongs to: the uuid in its name (omp/Pi or Claude), the stem
/// of an Antigravity `<id>.db`, or the file stem for anything else. Subagent lists are keyed by
/// this id. The session id a group is marked and pushed under. Shared so every part names a
/// session the same way.
pub fn session_id_of(key: &str) -> String {
    let name = file_name(key);
    let stem = strip_session_ext(&name);
    if let Some((_, id)) = stem.rsplit_once('_') {
        if id.len() == 36 {
            return id.to_string();
        }
    }
    stem.to_string()
}

/// A session file name without its extension: `.jsonl` journals and transcripts, `.db`
/// Antigravity conversations.
fn strip_session_ext(name: &str) -> &str {
    name.strip_suffix(".jsonl").or_else(|| name.strip_suffix(".db")).unwrap_or(name)
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
        // Every file of the session: an Antigravity conversation is its database and its WAL.
        crate::mirror::remove_copy(&g.fmt, &src).map_err(|e| e.to_string())?;
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
    rename_group_noted(roots, g, title, lock_dir, sesync_lock).map(|(saved, _)| saved)
}

/// As `rename_group`, plus a note for the user when the new name is saved but shows only later:
/// an Antigravity rename made while agy runs applies when agy restarts.
pub fn rename_group_noted(roots: &Roots, g: &Group, title: &str, lock_dir: &Path, sesync_lock: &Path) -> Result<(String, Option<String>), String> {
    let new = validate_title(title)?;
    let h = harness::get(&g.fmt).ok_or_else(|| format!("no harness named '{}'", g.fmt))?;
    // A store harness's conversation has no file: its name lives in the harness's own store and
    // is changed through it, so it is never "on disk" or under a folder.
    if h.kind() == Kind::Files {
        if g.orphan {
            return Err("source is not on disk; nothing to rename".into());
        }
        let path = PathBuf::from(&g.key);
        if !roots.contains(&path) {
            return Err(format!("source outside session roots, not renaming: {}", path.display()));
        }
    }
    let conv = conversation_id(&g.key);
    let outcome = with_claim(sesync_lock, Some(lock_dir), &conv, "tui", std::process::id(), &g.key, || {
        h.rename(roots.locations.of(&g.fmt), &roots.tools(), &g.key, &new)
    });
    match outcome {
        ClaimOutcome::Skipped(holder) => Err(format!("conversation is in use by {holder}")),
        ClaimOutcome::Ran(r) => r.map(|note| (new, note)),
    }
}

pub fn conversation_id(key: &str) -> String {
    let name = file_name(key);
    let stem = strip_session_ext(&name);
    stem.rsplit('_').next().unwrap_or(stem).to_string()
}
