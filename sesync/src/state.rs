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
    pub last_seen_change: f64,
    pub last_hash: String,
    pub last_stamp: String,
    pub failures: u32,
    pub mirrors: HashMap<String, Mirror>,
    /// Harness targets the user chose to mirror this session into. Empty by default: nothing
    /// is written anywhere until the user picks a target.
    pub chosen_targets: std::collections::BTreeSet<String>,
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
                    mirrors: HashMap::new(),
                    title: s("title"),
                    chosen_targets: v.get("chosen_targets").and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default(),
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
                "mirrors": mirrors, "chosen_targets": s.chosen_targets.iter().collect::<Vec<_>>(),
                "title": s.title,
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
