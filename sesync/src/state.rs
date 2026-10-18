//! Persisted daemon state. Same JSON layout as daemon/sesync.py State, so
//! an existing state.json keeps working across the port.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Mirror {
    pub target: String,
    pub path: String,
    #[serde(default)]
    pub source_hash: String,
    #[serde(default)]
    pub installed_hash: String,
    #[serde(default)]
    pub diverged: bool,
    /// size:mtime fingerprint from the last pass that found the content unchanged, so an
    /// untouched mirror costs one stat per pass, not one read. Empty means never fingerprinted.
    #[serde(default)]
    pub edit_stamp: String,
    /// The store's last-update time when the daemon wrote an OpenCode mirror, so a later
    /// `updated` on that row means the user wrote there. 0.0 for file-backed mirrors.
    #[serde(default)]
    pub target_updated: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TeamPush {
    pub hash: String,
    pub pushed_at: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Source {
    pub fmt: String,
    pub key: String,
    /// The conversation's stable id: the uuid of the store copy that started it (omp/Pi/Claude)
    /// or the OpenCode session id. Mirror ids in every other store are minted from THIS, so a
    /// turn-taking promotion (which changes `key`) never re-mints a copy in another store —
    /// each copy lives at one deterministic path and is replaced in place. Empty in states
    /// written before this existed: the source key is used instead.
    pub conv: String,
    pub last_seen_change: f64,
    pub last_hash: String,
    pub last_stamp: String,
    pub failures: u32,
    /// When the last attempt failed, seconds since the epoch. Retries back off from it;
    /// a source that changes is retried regardless.
    pub last_failure: f64,
    pub mirrors: HashMap<String, Mirror>,
    /// Harness targets the user chose to mirror this session into. Empty by default: nothing
    /// is written anywhere until the user picks a target.
    pub chosen_targets: std::collections::BTreeSet<String>,
    /// Runtime only: the entry was created by the TUI (a choice for a session the daemon has
    /// not scanned yet). Such an entry survives a prune that adopts the daemon's deletions;
    /// adopted ones do not, so a stale in-memory copy cannot resurrect a session the daemon
    /// removed. Never saved.
    pub tui_created: bool,
    /// The wall time of this session's last turn-taking promotion, plus a grace second. A
    /// mirror edit at or below this time cannot be a user turn — it is a copy the promotion
    /// itself just rewrote (or a resurrected leftover of an older binary) — and never
    /// qualifies to turn the session back. Saved.
    pub turn_at: f64,
    /// The name to show for this session, when its source does not carry one on disk (O-penCode).
    pub title: String,
}

impl Source {
    pub fn new(fmt: &str, key: &str) -> Self {
        Source { fmt: fmt.into(), key: key.into(), ..Default::default() }
    }
}

#[derive(Debug, Default)]
pub struct State {
    pub sources: HashMap<String, Source>,
    /// Every path or id the daemon created as a mirror. Changes to these are ignored.
    pub mirror_keys: BTreeSet<String>,
    /// Subagent transcript files, keyed by the parent session id. They belong to the
    /// parent: shown under it, never synced as conversations of their own.
    pub subagents: BTreeMap<String, Vec<String>>,
    /// Sessions the user marked for the team. Only these are eligible to push. A session is
    /// never pushed unless it is in this set.
    pub marked: BTreeSet<String>,
    /// Per-session team push record: what was last sent and when.
    pub team: std::collections::BTreeMap<String, TeamPush>,
    /// Per conversation, the highest team record sequence this device has accepted. A record
    /// below it is a rollback and is refused. Only ever raised.
    pub team_seen: BTreeMap<String, u64>,
}

impl State {
    pub fn mark(&mut self, session: &str) { self.marked.insert(session.to_string()); }
    pub fn unmark(&mut self, session: &str) { self.marked.remove(session); }
    pub fn is_marked(&self, session: &str) -> bool { self.marked.contains(session) }
    /// The content hash last pushed to the team for a session, if any.
    pub fn team_hash(&self, session: &str) -> Option<String> {
        self.team.get(session).map(|t| t.hash.clone())
    }
    /// Record a successful team push: the content hash pushed and when.
    pub fn record_team_push(&mut self, session: &str, hash: &str, at: f64) {
        self.team.insert(session.to_string(), TeamPush { hash: hash.to_string(), pushed_at: at });
    }
    pub fn marked_sessions(&self) -> Vec<String> { self.marked.iter().cloned().collect() }
}

fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

impl State {
    /// Missing file means empty state. A corrupt file is an error: silently
    /// dropping it would forget every mirror and risk duplicate writes.
    pub fn load(path: &Path) -> io::Result<State> {
        if !path.exists() {
            return Ok(State::default());
        }
        let raw: Value = serde_json::from_str(&fs::read_to_string(path)?).map_err(invalid)?;
        let mut st = State::default();
        if let Some(srcs) = raw.get("sources").and_then(Value::as_object) {
            for (k, v) in srcs {
                let s = |f: &str| v.get(f).and_then(Value::as_str).unwrap_or("").to_string();
                let mut src = Source {
                    fmt: s("fmt"),
                    key: s("key"),
                    last_seen_change: v.get("last_seen_change").and_then(Value::as_f64).unwrap_or(0.0),
                    last_hash: s("last_hash"),
                    last_stamp: s("last_stamp"),
                    failures: v.get("failures").and_then(Value::as_u64).unwrap_or(0) as u32,
                    last_failure: v.get("last_failure").and_then(Value::as_f64).unwrap_or(0.0),
                    mirrors: HashMap::new(),
                    title: s("title"),
                    chosen_targets: v.get("chosen_targets").and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default(),
                    tui_created: false,
                    turn_at: v.get("turn_at").and_then(Value::as_f64).unwrap_or(0.0),
                    conv: s("conv"),
                };
                if let Some(ms) = v.get("mirrors").and_then(Value::as_object) {
                    for (t, mv) in ms {
                        src.mirrors.insert(t.clone(), serde_json::from_value(mv.clone()).map_err(invalid)?);
                    }
                }
                st.sources.insert(k.clone(), src);
            }
        }
        if let Some(subs) = raw.get("subagents").and_then(Value::as_object) {
            for (parent, files) in subs {
                let list = files.as_array().map(|a| a.iter().filter_map(Value::as_str)
                    .map(str::to_string).collect()).unwrap_or_default();
                st.subagents.insert(parent.clone(), list);
            }
        }
        if let Some(keys) = raw.get("mirror_keys").and_then(Value::as_array) {
            st.mirror_keys = keys.iter().filter_map(Value::as_str).map(str::to_string).collect();
        }
        // Team push records are optional: a state file written before they existed has none.
        if let Some(team) = raw.get("team").and_then(Value::as_object) {
            for (id, v) in team {
                st.team.insert(id.clone(), TeamPush {
                    hash: v.get("hash").and_then(Value::as_str).unwrap_or("").to_string(),
                    pushed_at: v.get("pushed_at").and_then(Value::as_f64).unwrap_or(0.0),
                });
            }
        }
        // The team sequence floors are optional: a state file written before they existed has none.
        if let Some(seen) = raw.get("team_seen").and_then(Value::as_object) {
            for (conv, seq) in seen {
                if let Some(n) = seq.as_u64() {
                    st.team_seen.insert(conv.clone(), n);
                }
            }
        }
        // Marks are optional: a state file written before they existed has none.
        if let Some(marked) = raw.get("marked").and_then(Value::as_array) {
            for m in marked {
                if let Some(id) = m.as_str() {
                    st.marked.insert(id.to_string());
                }
            }
        }
        Ok(st)
    }

    /// Adopt the fields the user owns from the file on disk: the marked set and each source's
    /// chosen mirror targets. The daemon calls this before a pass and again just before saving,
    /// so a choice made in a running TUI is never overwritten by the daemon's next save.
    /// A missing file changes nothing; a corrupt file is an error the caller reports.
    /// Whether a state entry the daemon has not scanned can be adopted from disk: store-backed
    /// keys always (OpenCode has no file); file-backed keys never when the daemon already
    /// created that file as a mirror — a promotion superseded that copy, and its stale
    /// fingerprints would read as a fresh edit every pass.
    fn adoptable(&self, key: &str) -> bool {
        !self.mirror_keys.contains(key)
    }

    pub fn absorb_user_edits(&mut self, path: &Path) -> io::Result<()> {        if !path.exists() {
            return Ok(());
        }
        let disk = State::load(path)?;
        self.marked = disk.marked;
        for (key, dsrc) in &disk.sources {
            if let Some(s) = self.sources.get_mut(key) {
                s.chosen_targets = dsrc.chosen_targets.clone();
            } else if self.adoptable(key) {
                // The TUI records a choice for a session the daemon has not scanned yet.
                // A file-backed key the daemon already created as a mirror is a copy a
                // promotion superseded; its stale fingerprints would read as a fresh edit
                // every pass. The component resolution in turn.rs still absorbs leftovers.
                self.sources.insert(key.clone(), dsrc.clone());
            }
        }
        Ok(())
    }

    /// Save from the app (the TUI): everything the daemon owns comes from the file on disk, so
    /// this never rolls back a scan, a mirror record or a team floor. Only the fields the user
    /// owns — marks and chosen targets — come from this state. A source the app knows before
    /// the daemon does is added so the choice is not lost.
    pub fn save_user(&self, path: &Path) -> io::Result<()> {
        let mut disk = State::load(path)?;
        disk.marked = self.marked.clone();
        for (key, mine) in &self.sources {
            let d = disk.sources.entry(key.clone())
                .or_insert_with(|| Source::new(&mine.fmt, &mine.key));
            d.chosen_targets = mine.chosen_targets.clone();
            if d.fmt.is_empty() { d.fmt = mine.fmt.clone(); }
            if d.key.is_empty() { d.key = mine.key.clone(); }
            if d.title.is_empty() { d.title = mine.title.clone(); }
        }
        disk.save(path)
    }

    /// Write atomically: a temp file, then rename over the state file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let sources: BTreeMap<&String, Value> = self.sources.iter().map(|(k, s)| {
            let mirrors: BTreeMap<&String, &Mirror> = s.mirrors.iter().collect();
            (k, serde_json::json!({
                "fmt": s.fmt, "key": s.key, "last_seen_change": s.last_seen_change,
                "last_hash": s.last_hash, "last_stamp": s.last_stamp, "failures": s.failures,
                "last_failure": s.last_failure,
                "mirrors": mirrors, "chosen_targets": s.chosen_targets.iter().collect::<Vec<_>>(),
                "title": s.title, "turn_at": s.turn_at, "conv": s.conv,
            }))
        }).collect();
        let data = serde_json::json!({
            "sources": sources,
            "mirror_keys": self.mirror_keys.iter().collect::<Vec<_>>(),
            "subagents": self.subagents,
            "marked": self.marked.iter().collect::<Vec<_>>(),
            "team": self.team.iter().map(|(k, v)| (k.clone(), serde_json::json!({"hash": v.hash, "pushed_at": v.pushed_at}))).collect::<serde_json::Map<_, _>>(),
            "team_seen": self.team_seen.iter().map(|(k, v)| (k.clone(), serde_json::json!(v))).collect::<serde_json::Map<_, _>>(),
        });
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&data).map_err(invalid)?)?;
        fs::rename(&tmp, path)
    }
}
