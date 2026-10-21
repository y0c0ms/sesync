//! OpenCode: conversations are rows in its SQLite database (read here, read-only) and are moved
//! only through its CLI: `opencode export` to read one out, `opencode import` of a bundle
//! `sesync-conv to-opencode` builds to write a copy, `opencode session delete` to remove one, and
//! its API for a rename. The key of a conversation is its session id.

pub mod rename;
pub mod store;

use super::{Harness, Kind, Location, Message, Row, Tools};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub struct OpenCode;

impl OpenCode {
    fn cli(loc: &Location) -> PathBuf {
        loc.cli.clone().unwrap_or_else(|| PathBuf::from("opencode"))
    }
}

impl Harness for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }
    fn icon(&self) -> &'static str {
        "◉"
    }
    fn kind(&self) -> Kind {
        Kind::Store
    }
    fn locate(&self, home: &Path) -> Location {
        Location {
            root: super::env_path("OPENCODE_DB", home.join(".local/share/opencode/opencode.db")),
            cli: Some(super::env_path("OPENCODE_BIN", PathBuf::from("opencode"))),
        }
    }
    /// Offered when its CLI can be run: a copy is an `opencode import`, not a file.
    fn present(&self, loc: &Location) -> bool {
        super::command_available(&Self::cli(loc))
    }
    fn newest_prompt(&self, loc: &Location, key: &str) -> Option<f64> {
        store::newest_prompt(&loc.root, key)
    }
    /// Exported with its CLI and converted; the export is removed once converted.
    fn to_journal(&self, loc: &Location, tools: &Tools, key: &str, scratch: &Path) -> Result<Option<PathBuf>, String> {
        store::export_and_convert(&Self::cli(loc), tools.sesync_conv, key, scratch).map(Some)
    }
    /// A session id in the shape OpenCode mints.
    fn mirror_id(&self, stable: &str) -> String {
        mirror_opencode_id(stable)
    }
    fn id_in(&self, key: &str) -> Option<String> {
        Some(key.to_string()).filter(|k| !k.is_empty())
    }
    /// The import goes by the bundle's directory, the conversation's own workspace. The
    /// delete-first runs where the store keeps the copy now, the only place the CLI finds it (a
    /// first write has no row yet).
    fn install(&self, loc: &Location, tools: &Tools, journal: &Path, mirror_id: &str, title: &str) -> Result<String, String> {
        let existing = store::session_directory(&loc.root, mirror_id).unwrap_or_default();
        store::install_opencode(&Self::cli(loc), tools.sesync_conv, journal, "", &existing, mirror_id, title, None)
    }
    /// Through OpenCode's API (PATCH /session/{id}), reusing a `serve` that is already running.
    fn rename(&self, _loc: &Location, _tools: &Tools, key: &str, title: &str) -> Result<Option<String>, String> {
        rename::rename_session(key, title, None, None).map(|_| None).map_err(|e| e.to_string())
    }
    fn copy_exists(&self, loc: &Location, key: &str) -> bool {
        self.fingerprint(loc, key).is_some()
    }
    fn rows(&self, loc: &Location) -> Vec<Row> {
        if !loc.root.exists() {
            return Vec::new();
        }
        store::sessions(&loc.root).into_iter()
            .map(|s| Row { id: s.id, updated: s.updated, title: s.title, directory: s.directory })
            .collect()
    }
    /// The store's title, or the first prompt when the store has only its default name.
    fn row_title(&self, loc: &Location, row: &Row) -> String {
        let title = store::clean_title(&row.title);
        if !title.is_empty() {
            return title;
        }
        let first = store::first_prompt(&loc.root, &row.id);
        if first.is_empty() { "untitled session".to_string() } else { first }
    }
    fn fingerprint(&self, loc: &Location, id: &str) -> Option<String> {
        store::content_fingerprint(&loc.root, id)
    }
    /// Runs in the session's own project: the CLI finds a session only from there.
    fn export(&self, loc: &Location, id: &str, out: &Path) -> Result<(), String> {
        let dir = store::session_directory(&loc.root, id);
        store::export(&Self::cli(loc), id, dir.as_deref(), out)
    }
    /// `opencode session delete` only finds a session from inside its own project: from any other
    /// directory it answers "Session not found", which reads as success. Run it where the store
    /// keeps the session.
    fn remove(&self, loc: &Location, id: &str) -> Result<(), String> {
        let dir = store::session_directory(&loc.root, id).unwrap_or_default();
        store::remove_opencode_session(&Self::cli(loc), id, &dir, None)
    }
    /// Read from the database: an export through the CLI takes seconds.
    fn recent_messages(&self, loc: &Location, _tools: &Tools, key: &str, _scratch: &Path, max: usize) -> Result<Vec<Message>, String> {
        store::recent_messages(&loc.root, key, max)
    }
}

const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// "ses_" + 12 hex of sha256("<key>|opencode") + 14 base62 characters, the shape
/// OpenCode uses for session ids. Mirrors Python's byte pairs at [12..40) step 2.
pub fn mirror_opencode_id(source_key: &str) -> String {
    let digest = Sha256::digest(format!("{source_key}|opencode").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let mut tail = String::new();
    for i in (12..40).step_by(2) {
        let v = u8::from_str_radix(&hex[i..i + 2], 16).expect("hex") as usize;
        tail.push(ALPHABET[v % 62] as char);
    }
    format!("ses_{}{}", &hex[..12], tail)
}
