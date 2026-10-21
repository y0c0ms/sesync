//! The coding harnesses sesync keeps in step, one module each, behind one interface.
//!
//! Everything sesync knows about a harness lives in its module: where it keeps conversations,
//! how to list and read them, how to turn one into the canonical journal and back, where a copy
//! goes, and how one is renamed or removed. The rest of sesync asks the registry ([`ALL`],
//! [`get`]) and never names a harness. Two storage models exist ([`Kind`]):
//!
//! - `Files`: a conversation is a file in the harness's store (with its write-ahead log, when it
//!   has one) and its key is the file's path. sesync stats, hashes and archives those files.
//! - `Store`: conversations live in the harness's own database and the key is its session id.
//!   Every read and write goes through the harness: its database read-only, its CLI.
//!
//! Locks, the archive kept before a replace, and the state records are shared machinery outside
//! the harnesses.
//!
//! Adding a harness, once `sesync-conv` converts it (`from-<id>` into the canonical journal,
//! `to-<id>` back): write `harness/<id>.rs` (or `harness/<id>/` when it needs its own store or
//! CLI code) implementing [`Harness`], and list it in [`ALL`]. Removing one: take it out of
//! `ALL`, add it to [`RETIRED`], and delete its module; `State::retire_harness` releases what an
//! older run recorded for it. Nothing outside this module names a harness.

pub mod antigravity;
pub mod claude;
pub mod journal;
pub mod omp;
pub mod opencode;
pub mod pi;
mod retired;

pub use retired::{Retired, RETIRED};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How a harness keeps its conversations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One file per conversation (plus its write-ahead log, if any); the key is the path.
    Files,
    /// Rows in the harness's own database; the key is the session id.
    Store,
}

/// Where one harness keeps its conversations on this machine, and its command when sesync
/// drives one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Location {
    /// A directory for a file harness; the database for a store harness.
    pub root: PathBuf,
    /// The harness's own command: run for import, export and removal, and to tell whether the
    /// harness is installed.
    pub cli: Option<PathBuf>,
}

/// Every harness's location, built once per run from the environment: each harness reads its own
/// variables in [`Harness::locate`].
#[derive(Clone, Debug, Default)]
pub struct Locations {
    by_id: BTreeMap<String, Location>,
    none: Location,
}

impl Locations {
    pub fn from_env(home: &Path) -> Locations {
        Locations { by_id: ALL.iter().map(|h| (h.id().to_string(), h.locate(home))).collect(), none: Location::default() }
    }

    /// The location of harness `id`. An unknown id has an empty location, which lists nothing.
    pub fn of(&self, id: &str) -> &Location {
        self.by_id.get(id).unwrap_or(&self.none)
    }

    /// Point one harness somewhere else (tests, sandboxes).
    pub fn set(&mut self, id: &str, loc: Location) {
        self.by_id.insert(id.to_string(), loc);
    }

    /// The folder harness `id` keeps its conversation files in; empty for an unknown id.
    pub fn sessions_dir(&self, id: &str) -> PathBuf {
        get(id).map(|h| h.sessions_dir(self.of(id))).unwrap_or_default()
    }

    /// Whether `path` lies inside the conversation folder of a file harness (`sessions_dir`).
    /// Canonicalised first, so `..` and links cannot escape: the TUI renames and deletes only there.
    pub fn owns(&self, path: &Path) -> bool {
        let Ok(resolved) = path.canonicalize() else { return false };
        ALL.iter()
            .filter(|h| h.kind() == Kind::Files)
            .any(|h| self.sessions_dir(h.id()).canonicalize().map(|r| resolved.starts_with(r)).unwrap_or(false))
    }
}

/// What a harness may use besides its own store.
pub struct Tools<'a> {
    /// The user's home: some harnesses file conversations by a home-relative project bucket.
    pub home: &'a Path,
    /// The converter between the canonical journal and every harness's format.
    pub sesync_conv: &'a Path,
}

/// One conversation of a store harness, as its database lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    /// Last update, seconds since the epoch.
    pub updated: f64,
    pub title: String,
    /// The project directory the harness files it under.
    pub directory: String,
}

/// Everything sesync needs from one harness. The defaults suit a file harness that keeps no
/// project directory; a store harness overrides the row and fingerprint hooks.
pub trait Harness: Send + Sync {
    /// The name in state, in mirror records and in the converter's verbs ("omp", "opencode").
    fn id(&self) -> &'static str;
    /// The name in the sessions list: at most 8 cells.
    fn label(&self) -> &'static str {
        self.id()
    }
    /// One narrow glyph for the mirror picker.
    fn icon(&self) -> &'static str;
    fn kind(&self) -> Kind {
        Kind::Files
    }

    /// This harness's location, from its own environment variables or their defaults.
    fn locate(&self, home: &Path) -> Location;
    /// Whether the harness is installed here, so it is offered as a mirror target.
    fn present(&self, loc: &Location) -> bool;

    // ----- a file harness's conversations ------------------------------------------------------

    /// The folder its conversations live under: the only place the TUI renames or deletes a file.
    fn sessions_dir(&self, loc: &Location) -> PathBuf {
        loc.root.clone()
    }
    /// The conversations in the store. The caller skips the copies sesync wrote.
    fn sessions(&self, _loc: &Location) -> Vec<PathBuf> {
        Vec::new()
    }
    /// Transcripts that belong to another conversation and are shown under it, never synced:
    /// (parent session id, file).
    fn subagents(&self, _loc: &Location) -> Vec<(String, PathBuf)> {
        Vec::new()
    }
    /// Every file one conversation is made of: the file, and its write-ahead log when it has one.
    fn files_of(&self, path: &Path) -> Vec<PathBuf> {
        vec![path.to_path_buf()]
    }
    /// Files removed together with a conversation that are not its content (a database index).
    fn side_files(&self, _path: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
    /// The name the harness shows for a conversation, or "" when it has none.
    fn title(&self, _path: &Path) -> String {
        String::new()
    }
    /// Size and change time of everything the title is read from, so a list re-reads a title
    /// only when it moved. None when the conversation is gone.
    fn title_stamp(&self, path: &Path) -> Option<(u64, u128)> {
        stat(path)
    }
    /// The project directory recorded in a conversation, when the harness keeps one.
    fn cwd(&self, _path: &Path) -> Option<String> {
        None
    }
    /// Whether a copy's place in the store depends on the project directory, so `misplaced`
    /// needs the source's `cwd`.
    fn placed_by_cwd(&self) -> bool {
        false
    }
    /// Whether a recorded copy sits where this harness will never list it (an older write into
    /// the wrong project folder). Conservative: false whenever the right place is unknown.
    fn misplaced(&self, _path: &str, _cwd: Option<&str>, _home: &str) -> bool {
        false
    }

    // ----- both kinds --------------------------------------------------------------------------

    /// The newest user prompt in a conversation, in seconds since the epoch.
    fn newest_prompt(&self, loc: &Location, key: &str) -> Option<f64>;
    /// Whether a conversation file of this harness already is a canonical journal, so it is
    /// installed into other harnesses as it is (omp, Pi).
    fn native_journal(&self) -> bool {
        false
    }
    /// The conversation as a canonical journal: None when the key already is one, else the
    /// converted file, written in `scratch` (the caller removes it).
    fn to_journal(&self, loc: &Location, tools: &Tools, key: &str, scratch: &Path) -> Result<Option<PathBuf>, String>;
    /// A new copy's id, minted from the conversation's stable id.
    fn mirror_id(&self, stable: &str) -> String {
        crate::mirror::mirror_uuid(stable, self.id())
    }
    /// The id a recorded copy goes by in this harness, read from its key. None when the key
    /// carries none.
    fn id_in(&self, key: &str) -> Option<String>;
    /// Write a canonical journal into the store as the copy `mirror_id`, replacing an earlier
    /// write of the same id, and return the copy's key. An error that says "will retry" means the
    /// copy was in use (see `session_cols::is_transient`).
    fn install(&self, loc: &Location, tools: &Tools, journal: &Path, mirror_id: &str, title: &str) -> Result<String, String>;
    /// Give a conversation a new name, the way the harness itself records one. Some(note) when
    /// the name shows only later.
    fn rename(&self, loc: &Location, tools: &Tools, key: &str, title: &str) -> Result<Option<String>, String>;
    /// Whether a recorded copy still exists.
    fn copy_exists(&self, _loc: &Location, key: &str) -> bool {
        Path::new(key).is_file()
    }

    // ----- a store harness's conversations -----------------------------------------------------

    /// The conversations in the database, newest updates included.
    fn rows(&self, _loc: &Location) -> Vec<Row> {
        Vec::new()
    }
    /// The name to show for a row: its own title, or what the harness falls back to.
    fn row_title(&self, _loc: &Location, row: &Row) -> String {
        row.title.clone()
    }
    /// What a stored copy holds, to tell sesync's own write from a user's turn. None when the
    /// copy is not in the store.
    fn fingerprint(&self, _loc: &Location, _id: &str) -> Option<String> {
        None
    }
    /// Export a stored copy to `out`, to keep it before it is replaced.
    fn export(&self, _loc: &Location, id: &str, _out: &Path) -> Result<(), String> {
        Err(format!("{} cannot export {id}", self.id()))
    }
    /// Remove a stored copy.
    fn remove(&self, _loc: &Location, id: &str) -> Result<(), String> {
        Err(format!("{} cannot remove {id}", self.id()))
    }
}

/// Every harness sesync syncs between. The order is the order lists and pickers show them in.
pub static ALL: &[&dyn Harness] = &[&omp::Omp, &pi::Pi, &claude::Claude, &opencode::OpenCode, &antigravity::Antigravity];

/// The harness with this id.
pub fn get(id: &str) -> Option<&'static dyn Harness> {
    ALL.iter().copied().find(|h| h.id() == id)
}

/// A harness's list label, or the id itself for one sesync does not know (a record left by a
/// retired harness).
pub fn label(id: &str) -> &str {
    get(id).map_or(id, |h| h.label())
}

/// The picker glyph for a harness, or a dot for one sesync does not know.
pub fn icon(id: &str) -> &'static str {
    get(id).map_or("•", |h| h.icon())
}

/// Whether harness `id` keeps its conversations in its own database (the key is a session id).
pub fn is_store(id: &str) -> bool {
    get(id).is_some_and(|h| h.kind() == Kind::Store)
}

/// The update time of one row of a store harness. None when the row is gone.
pub fn row_updated(h: &dyn Harness, loc: &Location, id: &str) -> Option<f64> {
    h.rows(loc).into_iter().find(|r| r.id == id).map(|r| r.updated)
}

/// Size and modification time of a file, for title stamps.
pub fn stat(path: &Path) -> Option<(u64, u128)> {
    let m = std::fs::metadata(path).ok()?;
    let ns = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    Some((m.len(), ns))
}

/// A path from the environment, or `default` when the variable is unset.
pub(crate) fn env_path(key: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(key).map(PathBuf::from).unwrap_or(default)
}

/// Whether a tool can be run: an explicit path is checked directly, a bare name is looked up on
/// PATH. Used for the installed-harness probe, which must not start the tool.
///
/// Windows executables carry `.exe`, and the configured name is usually bare (`opencode`), so
/// every candidate is also tried with the platform's executable suffixes. Without this, a harness
/// whose CLI is plainly installed is never offered as a mirror target.
pub fn command_available(tool: &Path) -> bool {
    let candidates = executable_candidates(tool);
    if tool.parent().map_or(false, |d| !d.as_os_str().is_empty()) {
        return candidates.iter().any(|c| c.is_file());
    }
    std::env::var_os("PATH").map_or(false, |paths| {
        std::env::split_paths(&paths).any(|d| candidates.iter().any(|c| d.join(c).is_file()))
    })
}

/// A tool's name as given, then with each executable suffix when it has none.
pub fn executable_candidates(tool: &Path) -> Vec<PathBuf> {
    let mut out = vec![tool.to_path_buf()];
    if tool.extension().is_none() {
        for ext in ["exe", "cmd", "bat"] {
            out.push(tool.with_extension(ext));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_harness_has_its_own_id_and_a_label_that_fits_the_list() {
        let mut ids: Vec<&str> = ALL.iter().map(|h| h.id()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), ALL.len(), "ids are unique");
        for h in ALL {
            assert!(crate::session_cols::text_width(h.label()) <= 8, "{} label fits 8 cells", h.id());
            assert_eq!(crate::session_cols::text_width(h.icon()), 1, "{} icon is one cell", h.id());
            assert!(get(h.id()).is_some());
        }
    }
}
