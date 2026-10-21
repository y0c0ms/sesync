//! Antigravity CLI (agy): one SQLite database per conversation, `<store>/conversations/<id>.db`,
//! with its title in `<store>/annotations/<id>.pbtxt`. agy appends live steps to the database's
//! write-ahead log and folds them in only when it exits, so a conversation is the database and
//! its WAL. Converted with `sesync-conv from-antigravity`; copies are written with
//! `to-antigravity`, and renamed with `antigravity-retitle`. sesync never runs agy itself.

pub mod store;

use super::{journal, Harness, Location, Tools};
use std::path::{Path, PathBuf};
use store::Retitled;

pub struct Antigravity;

impl Harness for Antigravity {
    fn id(&self) -> &'static str {
        "antigravity"
    }
    fn label(&self) -> &'static str {
        "agy"
    }
    fn icon(&self) -> &'static str {
        "▲"
    }
    fn locate(&self, home: &Path) -> Location {
        Location { root: store::store_root(home), cli: Some(store::agy_binary()) }
    }
    /// Offered when agy is installed; the copy is a file the converter writes into its store.
    fn present(&self, loc: &Location) -> bool {
        loc.cli.as_deref().is_some_and(super::command_available)
    }
    fn sessions(&self, loc: &Location) -> Vec<PathBuf> {
        if loc.root.is_dir() { store::antigravity_sessions(&loc.root) } else { Vec::new() }
    }
    /// The database, and its WAL when present: a turn typed in agy changes the WAL alone.
    fn files_of(&self, path: &Path) -> Vec<PathBuf> {
        let mut out = vec![path.to_path_buf()];
        let wal = store::with_suffix(path, "-wal");
        if wal.is_file() {
            out.push(wal);
        }
        out
    }
    /// The `-shm` index goes with the database; it is not content.
    fn side_files(&self, path: &Path) -> Vec<PathBuf> {
        vec![store::with_suffix(path, "-shm")]
    }
    fn title(&self, path: &Path) -> String {
        store::title(path)
    }
    /// The title lives beside the database (annotations) and its turns in the WAL: the stamp
    /// covers all of them, so a rename made in agy shows up.
    fn title_stamp(&self, path: &Path) -> Option<(u64, u128)> {
        store::title_stamp(path)
    }
    fn newest_prompt(&self, _loc: &Location, key: &str) -> Option<f64> {
        store::newest_prompt(Path::new(key))
    }
    fn to_journal(&self, _loc: &Location, tools: &Tools, key: &str, scratch: &Path) -> Result<Option<PathBuf>, String> {
        journal::convert_from(tools.sesync_conv, "from-antigravity", Path::new(key), scratch, "Antigravity conversation")
            .map(Some)
    }
    fn id_in(&self, key: &str) -> Option<String> {
        Path::new(key).file_name()?.to_str()?.strip_suffix(".db").filter(|stem| !stem.is_empty()).map(str::to_string)
    }
    fn install(&self, loc: &Location, tools: &Tools, journal: &Path, mirror_id: &str, title: &str) -> Result<String, String> {
        store::install_antigravity(tools.sesync_conv, journal, &loc.root, mirror_id, title)
            .map(|p| p.display().to_string())
    }
    /// agy's title lives in its annotations file and two caches sesync never edits itself:
    /// `sesync-conv antigravity-retitle` writes the one and drops this conversation from the
    /// others, so agy rebuilds them with the new name.
    fn rename(&self, _loc: &Location, tools: &Tools, key: &str, title: &str) -> Result<Option<String>, String> {
        let path = Path::new(key);
        let root = store::store_of(path).ok_or("not inside an Antigravity store")?;
        match store::retitle(tools.sesync_conv, &root, &store::conversation_id_of(path), title)? {
            Retitled::Done => Ok(None),
            Retitled::AfterRestart => Ok(Some("Antigravity is running: the new name applies when agy restarts".into())),
        }
    }
}
