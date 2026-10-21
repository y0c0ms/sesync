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

/// The outcome of the daemon's last attempts to write one mirror target of a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TargetSync {
    /// When a write to this target last succeeded, seconds since the epoch. 0 when never.
    pub ok_at: f64,
    /// The message of the last failed write; empty once a later write succeeds.
    pub problem: String,
    /// When that failure happened, seconds since the epoch.
    pub problem_at: f64,
}

fn parse_sync(v: Option<&Value>) -> BTreeMap<String, TargetSync> {
    v.and_then(Value::as_object).map(|m| m.iter().map(|(t, e)| (t.clone(), TargetSync {
        ok_at: e.get("ok_at").and_then(Value::as_f64).unwrap_or(0.0),
        problem: e.get("problem").and_then(Value::as_str).unwrap_or("").to_string(),
        problem_at: e.get("problem_at").and_then(Value::as_f64).unwrap_or(0.0),
    })).collect()).unwrap_or_default()
}

fn sync_json(sync: &BTreeMap<String, TargetSync>) -> Value {
    Value::Object(sync.iter().map(|(t, e)| (t.clone(), serde_json::json!({
        "ok_at": e.ok_at, "problem": e.problem, "problem_at": e.problem_at,
    }))).collect())
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
    /// What the daemon last learned about each mirror target of this session, keyed by target
    /// name. Written by the daemon after every pass that tried a target; empty in older files.
    pub sync: BTreeMap<String, TargetSync>,
}

impl Source {
    pub fn new(fmt: &str, key: &str) -> Self {
        Source { fmt: fmt.into(), key: key.into(), ..Default::default() }
    }
}

impl Source {
    /// Record the outcome of one write to `target`. A success clears the problem; a failure keeps
    /// the time of the last good write so the TUI can say when the target last worked.
    pub fn record_sync(&mut self, target: &str, outcome: Result<(), &str>, now: f64) {
        let e = self.sync.entry(target.to_string()).or_default();
        match outcome {
            Ok(()) => {
                e.ok_at = now;
                e.problem.clear();
                e.problem_at = 0.0;
            }
            Err(msg) => {
                e.problem = msg.to_string();
                e.problem_at = now;
            }
        }
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
    /// Sessions shared with teams, per conversation: the set of team spaces this conversation
    /// is shared into. Empty or absent means nowhere. A session is never pushed into a space
    /// it is not shared with, and one conversation can be shared into several teams.
    pub shares: BTreeMap<String, BTreeSet<String>>,
    /// Per (space, conversation) team push record: what was last sent and when. A conversation
    /// shared into two teams keeps one record per team.
    pub team: std::collections::BTreeMap<String, TeamPush>,
    /// Per (space, conversation), the highest team record sequence this device has accepted.
    /// A record below it is a rollback and is refused. Only ever raised.
    pub team_seen: BTreeMap<String, u64>,
    /// Runtime only: conversation ids read from a pre-multi-team `marked` list. The caller
    /// folds them into `shares` once it knows the machine's teams. Never saved.
    pub legacy_marked: BTreeSet<String>,
    /// Runtime only: every source key this process has held, from the file it loaded and from
    /// its own passes. A key missing from memory but present on disk is the TUI's new choice only
    /// when it is NOT in here; otherwise this process removed it (a promotion demoted it to a
    /// mirror, or a tangle collapsed) and adopting it back recreates the tangle. Never saved.
    pub seen_keys: BTreeSet<String>,
}

/// The key for the per-team maps: one conversation can be shared into more than one team, and
/// each team has its own pushes and its own rollback floors. A unit-separator cannot appear in
/// a space name, so the key never collides.
pub fn team_key(space: &str, conversation: &str) -> String {
    format!("{space}\u{1f}{conversation}")
}

impl State {
    /// The teams a conversation is shared with.
    pub fn shares_of(&self, conversation: &str) -> BTreeSet<String> {
        self.shares.get(conversation).cloned().unwrap_or_default()
    }

    /// Set the teams a conversation is shared with; an empty set removes the share entirely.
    pub fn set_shares(&mut self, conversation: &str, spaces: BTreeSet<String>) {
        if spaces.is_empty() {
            self.shares.remove(conversation);
        } else {
            self.shares.insert(conversation.to_string(), spaces);
        }
    }

    /// Whether a conversation is shared with any team at all.
    pub fn is_shared(&self, conversation: &str) -> bool {
        self.shares.get(conversation).map_or(false, |s| !s.is_empty())
    }

    /// Every conversation shared with at least one team.
    pub fn shared_sessions(&self) -> Vec<String> { self.shares.keys().cloned().collect() }

    /// Every conversation shared with this team.
    pub fn sessions_shared_with(&self, space: &str) -> Vec<String> {
        self.shares.iter().filter(|(_, s)| s.contains(space)).map(|(c, _)| c.clone()).collect()
    }

    /// The content hash last pushed to a team for a conversation, if any.
    pub fn team_hash(&self, space: &str, conversation: &str) -> Option<String> {
        self.team.get(&team_key(space, conversation)).map(|t| t.hash.clone())
    }

    /// Record a successful push: the content hash sent and when.
    pub fn record_team_push(&mut self, space: &str, conversation: &str, hash: &str, at: f64) {
        self.team.insert(team_key(space, conversation), TeamPush { hash: hash.to_string(), pushed_at: at });
    }

    /// When the last successful push for this team happened, if any.
    pub fn team_pushed_at(&self, space: &str, conversation: &str) -> Option<f64> {
        self.team.get(&team_key(space, conversation)).map(|t| t.pushed_at)
    }

    /// The highest accepted sequence for this team's copy of the conversation (0 = none).
    pub fn seen_floor(&self, space: &str, conversation: &str) -> u64 {
        self.team_seen.get(&team_key(space, conversation)).copied().unwrap_or(0)
    }

    /// Share a conversation into one team.
    pub fn share(&mut self, conversation: &str, space: &str) {
        self.shares.entry(conversation.to_string()).or_default().insert(space.to_string());
    }

    /// Stop sharing a conversation into one team. Other teams are untouched.
    pub fn unshare(&mut self, conversation: &str, space: &str) {
        if let Some(spaces) = self.shares.get_mut(conversation) {
            spaces.remove(space);
            if spaces.is_empty() {
                self.shares.remove(conversation);
            }
        }
    }

    /// Whether a conversation is shared into this team.
    pub fn is_shared_with(&self, conversation: &str, space: &str) -> bool {
        self.shares.get(conversation).map_or(false, |s| s.contains(space))
    }

    /// Fold a pre-multi-team `marked` list into shares: the ids go to the one team the machine
    /// belongs to, or to the first of several (sorted, so the outcome is deterministic and the
    /// user can adjust with the share picker). With no team configured the ids stay pending.
    /// The same pass re-keys the older conversation-keyed push records and sequence floors.
    pub fn migrate_legacy_team_state(&mut self, teams: &[String]) {
        if teams.is_empty() {
            return;
        }
        let space = teams.iter().min().cloned().unwrap_or_default();
        for conv in std::mem::take(&mut self.legacy_marked) {
            self.share(&conv, &space);
        }
        let rekey = |old: &str| -> String {
            if old.contains('\u{1f}') { old.to_string() } else { team_key(&space, old) }
        };
        let pushed = std::mem::take(&mut self.team);
        for (k, v) in pushed {
            self.team.insert(rekey(&k), v);
        }
        let floors = std::mem::take(&mut self.team_seen);
        for (k, v) in floors {
            self.team_seen.insert(rekey(&k), v);
        }
    }

    /// Every team's floors at once: space → conversation → highest accepted sequence. Used by
    /// callers that pull several teams (and by a worker thread, which takes this map instead of
    /// the whole state).
    pub fn floors_all(&self) -> BTreeMap<String, BTreeMap<String, u64>> {
        let mut out: BTreeMap<String, BTreeMap<String, u64>> = Default::default();
        for (k, v) in &self.team_seen {
            if let Some((space, conv)) = k.split_once('\u{1f}') {
                out.entry(space.to_string()).or_default().insert(conv.to_string(), *v);
            }
        }
        out
    }

    /// The sequence floors this device holds for one team, as the pull expects them: per
    /// conversation, the highest accepted sequence. Other teams' floors are not included: a
    /// record received under one team must never raise another team's floor.
    pub fn floors_for_space(&self, space: &str) -> BTreeMap<String, u64> {
        let prefix = format!("{space}\u{1f}");
        self.team_seen.iter()
            .filter_map(|(k, v)| k.strip_prefix(&prefix).map(|conv| (conv.to_string(), *v)))
            .collect()
    }

    /// Raise the accepted sequence for this team's copy. Never lowers it: a record below the
    /// floor is a rollback and must stay refused.
    pub fn raise_seen(&mut self, space: &str, conversation: &str, sequence: u64) {
        let e = self.team_seen.entry(team_key(space, conversation)).or_insert(0);
        if sequence > *e {
            *e = sequence;
        }
    }

    /// The stable team id for a source key: the conversation's origin id (`Source::conv`) when
    /// the scan has pinned it, else the key's own session id. Everything the team keys on —
    /// the mark, the pushed hash, the server conversation, the seen floors — uses THIS id, so
    /// a turn-taking promotion (which changes the key) never changes the conversation.
    pub fn team_id_of(&self, key: &str) -> String {
        self.sources.get(key)
            .map(|s| s.conv.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| crate::manage::session_id_of(key))
    }

    /// The source key currently holding a team conversation id: the entry whose `conv` is it,
    /// else the old id rule (the key's own session id).
    pub fn key_of_team_id(&self, id: &str) -> Option<String> {
        self.sources.iter()
            .find(|(_, s)| s.conv == id || crate::manage::session_id_of(&s.key) == id || s.key == id)
            .map(|(k, _)| k.clone())
    }

    /// Every copy recorded as some source's mirror: file paths and OpenCode session ids alike.
    /// `mirror_keys` deliberately holds files only, so this is the set to ask "is this a copy?".
    pub fn mirror_paths(&self) -> std::collections::HashSet<String> {
        self.sources.values().flat_map(|s| s.mirrors.values().map(|m| m.path.clone())).collect()
    }

    /// Whether `key` is recorded as a mirror of another source.
    pub fn is_recorded_mirror(&self, key: &str) -> bool {
        self.sources.iter().any(|(k, s)| k != key && s.mirrors.values().any(|m| m.path == key))
    }

    /// Retire a harness sesync no longer supports (`harness::RETIRED`). Its files on disk are
    /// never touched; only sesync's records move:
    /// * a source in the retired format is re-keyed to its best surviving copy, the first harness
    ///   in `harness::ALL` whose recorded copy `exists` and is not already a
    ///   source of its own. It keeps its conversation id (so mirror ids, locks and the team
    ///   conversation stay), its title and its shares; its chosen targets lose the retired harness
    ///   and the new format, and its mirrors lose the new format's record (that copy is the
    ///   source now). With no surviving copy the entry is dropped;
    /// * every other source drops the retired harness from its chosen targets and its mirrors (a
    ///   release: the copy stays where it is, as a session of that harness);
    /// * `mirror_keys` loses every path of the retired harness, and the new source's own key.
    ///
    /// Idempotent, so it is safe on every load and after every merge. Returns one line per change.
    pub fn retire_harness(&mut self, retired: &crate::harness::Retired, exists: impl Fn(&str, &str) -> bool) -> Vec<String> {
        let fmt = retired.id;
        let mut log = Vec::new();
        let mut retired_paths: BTreeSet<String> = BTreeSet::new();
        let mut retired_keys: Vec<String> = self.sources.iter()
            .filter(|(_, s)| s.fmt == fmt)
            .map(|(k, _)| k.clone())
            .collect();
        retired_keys.sort();
        for key in retired_keys {
            let conv = self.team_id_of(&key);
            let Some(old) = self.sources.remove(&key) else { continue };
            retired_paths.insert(key.clone());
            let successor = crate::harness::ALL.iter().map(|h| h.id()).find_map(|t| {
                let m = old.mirrors.get(t)?;
                (!m.path.is_empty() && !self.sources.contains_key(&m.path) && exists(t, &m.path))
                    .then(|| (t.to_string(), m.path.clone()))
            });
            let Some((new_fmt, new_key)) = successor else {
                log.push(format!("dropped the {fmt} session {key}: no copy of it survives in another harness"));
                continue;
            };
            let mut new = Source::new(&new_fmt, &new_key);
            new.conv = conv;
            new.title = old.title.clone();
            new.last_seen_change = old.last_seen_change;
            new.turn_at = old.turn_at;
            new.chosen_targets = old.chosen_targets.iter()
                .filter(|t| *t != fmt && **t != new_fmt)
                .cloned()
                .collect();
            new.mirrors = old.mirrors.into_iter()
                .filter(|(t, _)| *t != fmt && *t != new_fmt)
                .collect();
            self.mirror_keys.remove(&new_key);
            self.seen_keys.insert(new_key.clone());
            log.push(format!("moved the {fmt} session {key} to its {new_fmt} copy {new_key}"));
            self.sources.insert(new_key, new);
        }
        let mut keys: Vec<String> = self.sources.keys().cloned().collect();
        keys.sort();
        for key in keys {
            let src = self.sources.get_mut(&key).expect("listed above");
            if src.chosen_targets.remove(fmt) {
                log.push(format!("stopped mirroring {key} into {fmt}"));
            }
            if let Some(m) = src.mirrors.remove(fmt) {
                log.push(format!("released the {fmt} copy {} of {key} (left on disk)", m.path));
                retired_paths.insert(m.path);
            }
        }
        let before = self.mirror_keys.len();
        self.mirror_keys.retain(|p| !retired_paths.contains(p) && !(retired.is_session_path)(p));
        if self.mirror_keys.len() != before {
            log.push(format!("forgot {} {fmt} path(s) sesync had recorded as copies", before - self.mirror_keys.len()));
        }
        log
    }
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
                    sync: parse_sync(v.get("sync")),
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
        // Shares: per conversation, the teams it is shared into. A state file written before
        // teams were per-session carries a legacy `marked` list instead; those ids are held
        // until the caller names the machine's teams (migrate_legacy_team_state).
        if let Some(shares) = raw.get("shares").and_then(Value::as_object) {
            for (conv, spaces) in shares {
                let set: BTreeSet<String> = spaces.as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                if !set.is_empty() {
                    st.shares.insert(conv.clone(), set);
                }
            }
        } else if let Some(marked) = raw.get("marked").and_then(Value::as_array) {
            st.legacy_marked = marked.iter().filter_map(Value::as_str).map(str::to_string).collect();
        }
        st.seen_keys = st.sources.keys().cloned().collect();
        Ok(st)
    }

    /// Whether a disk entry this process does not hold can be adopted. Only when it is a choice
    /// the TUI made for a session this process never held, and no copy sesync keeps. A key the
    /// daemon removed (a promotion turned it into a mirror) must stay removed: adopting it back
    /// is what made a conversation both the source and the mirror of its own copy, and the two
    /// entries then promoted each other on every pass.
    fn adoptable(&self, key: &str) -> bool {
        !self.seen_keys.contains(key) && !self.mirror_keys.contains(key) && !self.is_recorded_mirror(key)
    }

    /// Adopt the fields the user owns from the file on disk: the shares and each source's chosen
    /// mirror targets. The daemon calls this before a pass and again just before saving, so a
    /// choice made in a running TUI is never overwritten by the daemon's next save. A missing
    /// file changes nothing; a corrupt file is an error the caller reports.
    pub fn absorb_user_edits(&mut self, path: &Path) -> io::Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let disk = State::load(path)?;
        // Shares are the user's: take them whole. A legacy `marked` list on disk (an older
        // binary's save) is folded in by the caller once the machine's teams are known.
        self.shares = disk.shares.clone();
        if self.legacy_marked.is_empty() {
            self.legacy_marked = disk.legacy_marked.clone();
        }
        self.seen_keys.extend(self.sources.keys().cloned());
        for (key, dsrc) in &disk.sources {
            if let Some(s) = self.sources.get_mut(key) {
                s.chosen_targets = dsrc.chosen_targets.clone();
            } else if self.adoptable(key) {
                // The TUI recorded a choice for a session this process has not scanned yet.
                self.sources.insert(key.clone(), dsrc.clone());
                self.seen_keys.insert(key.clone());
            }
        }
        Ok(())
    }

    /// Save from the app (the TUI): everything the daemon owns comes from the file on disk, so
    /// this never rolls back a scan, a mirror record or a team floor. Only the fields the user
    /// owns (shares and chosen targets) come from this state. A source the app created itself,
    /// before the daemon scanned it, is added so the choice is not lost; any other key missing
    /// on disk was removed by the daemon and is not brought back, and a copy the daemon keeps
    /// as a mirror never becomes a source.
    pub fn save_user(&self, path: &Path) -> io::Result<()> {
        let mut disk = State::load(path)?;
        disk.shares = self.shares.clone();
        for (key, mine) in &self.sources {
            if !disk.sources.contains_key(key)
                && (!mine.tui_created || disk.mirror_keys.contains(key) || disk.is_recorded_mirror(key))
            {
                continue;
            }
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
                "sync": sync_json(&s.sync),
            }))
        }).collect();
        let data = serde_json::json!({
            "sources": sources,
            "mirror_keys": self.mirror_keys.iter().collect::<Vec<_>>(),
            "subagents": self.subagents,
            "shares": self.shares.iter().map(|(c, spaces)| (c.clone(), serde_json::json!(spaces.iter().collect::<Vec<_>>()))).collect::<serde_json::Map<_, _>>(),
            "team": self.team.iter().map(|(k, v)| (k.clone(), serde_json::json!({"hash": v.hash, "pushed_at": v.pushed_at}))).collect::<serde_json::Map<_, _>>(),
            "team_seen": self.team_seen.iter().map(|(k, v)| (k.clone(), serde_json::json!(v))).collect::<serde_json::Map<_, _>>(),
        });
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&data).map_err(invalid)?)?;
        fs::rename(&tmp, path)
    }
}
