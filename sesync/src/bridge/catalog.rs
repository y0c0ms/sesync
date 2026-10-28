//! Sessions as the phone sees them. A conversation is one sesync session (its parent) with the
//! mirror copies sesync keeps of it in other harnesses. Conversations are built from the same listing
//! the TUI uses (`manage::list_groups`), read-only. Nothing is written, and the daemon's save-time
//! steps are not run, so a session the TUI hides only in memory may show here until the daemon next
//! saves the state.

use crate::harness::{self, Kind};
use crate::manage::{self, Roots};
use crate::state::State;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// One session, as a group in a project.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The harness id: `omp`, `claude`, and so on.
    pub harness: String,
    pub title: String,
    /// Seconds since the epoch; 0 when unknown.
    pub updated: f64,
}

/// The sessions of one project directory, newest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    /// The project directory. Empty for sessions whose harness records no directory.
    pub dir: String,
    /// Newest first.
    pub sessions: Vec<Session>,
}

impl Project {
    pub fn newest(&self) -> f64 {
        self.sessions.first().map_or(0.0, |s| s.updated)
    }
}

/// One sesync conversation: the parent session and what the phone needs to open it.
#[derive(Debug, Clone, PartialEq)]
pub struct Conversation {
    /// The parent session's harness id.
    pub harness: String,
    /// The parent session's key: its file path, or a store's session id. Stable across listings, so
    /// the worktree id derived from it is too.
    pub key: String,
    pub title: String,
    /// The project directory. Empty when the harness records none.
    pub dir: String,
    /// Seconds since the epoch; 0 when unknown.
    pub updated: f64,
    /// Every session identifier a herdr pane runs for this conversation: the parent's, then each
    /// mirror's. A pane runs the conversation when one of its foreground arguments carries one.
    pub idents: Vec<String>,
}

impl Conversation {
    /// The repo this conversation groups under: the same id `repo.list` reports for its directory.
    pub fn repo_id(&self) -> String {
        repo_id(&self.dir)
    }

    /// The worktree id the phone uses: `<repo id>::<conversation id>`. Both halves are hashes, so the
    /// id never carries path characters a route or a URL would have to escape, and the conversation
    /// half is the same for as long as the parent session's key is.
    pub fn worktree_id(&self) -> String {
        format!("{}::{}", self.repo_id(), conversation_id(&self.harness, &self.key))
    }

    /// The id sesync-lock keys this conversation's claim by: the same id the TUI and the daemon
    /// claim under (`manage::conversation_id`), so a phone turn excludes them and they exclude it.
    pub fn lock_id(&self) -> String {
        crate::manage::conversation_id(&self.key)
    }
}

/// A stable id for a project directory. Hashed, so an id never carries path characters.
pub fn repo_id(dir: &str) -> String {
    format!("sesync-{}", hex16(dir.as_bytes()))
}

/// A stable id for a conversation, from its parent's harness and key.
pub fn conversation_id(harness: &str, key: &str) -> String {
    format!("conv-{}", hex16(format!("{harness}\0{key}").as_bytes()))
}

fn hex16(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Anything that can list conversations. The server holds one behind an `Arc`, so a test can serve a
/// fixed list and the CLI can serve the real one.
pub trait Catalog: Send + Sync {
    /// Every conversation the phone can open, newest first.
    fn conversations(&self) -> Result<Vec<Conversation>, String>;

    /// The same conversations grouped by project directory, for `repo.list`.
    fn projects(&self) -> Result<Vec<Project>, String> {
        Ok(group_by_dir(self.conversations()?))
    }

    /// A stamp that changes whenever the conversation's stored content does, read without converting it: a
    /// file's size and change time, or a store's own counts for the session. None when the source cannot be read.
    fn source_stamp(&self, _conv: &Conversation) -> Option<String> {
        None
    }

    /// The conversation as a canonical journal, converted into `scratch` by its harness's converter. The file
    /// it returns is the caller's to keep or remove.
    fn canonical_journal(&self, conv: &Conversation, _scratch: &Path) -> Result<PathBuf, String> {
        Err(format!("cannot convert a '{}' session here", conv.harness))
    }
}

/// The real catalog: the saved sesync state plus the harness stores under `roots`.
pub struct StateCatalog {
    pub roots: Roots,
    pub state_file: PathBuf,
}

impl Catalog for StateCatalog {
    fn conversations(&self) -> Result<Vec<Conversation>, String> {
        let state = State::load(&self.state_file)
            .map_err(|e| format!("cannot read the sesync state: {e}"))?;
        let groups = manage::list_groups(&self.roots, &state);
        let store_dirs = self.store_directories();
        let mut out = Vec::new();
        for g in groups.into_iter().filter(|g| !g.orphan) {
            let dir = match harness::get(&g.fmt) {
                Some(h) if h.kind() == Kind::Files => h.cwd(Path::new(&g.key)).unwrap_or_default(),
                Some(_) => store_dirs.get(&(g.fmt.clone(), g.key.clone())).cloned().unwrap_or_default(),
                None => String::new(),
            };
            let mut idents = Vec::new();
            if let Some(ident) = self.ident(&g.fmt, &g.key) {
                idents.push(ident);
            }
            for (target, path) in &g.mirrors {
                if let Some(ident) = self.ident(target, path) {
                    idents.push(ident);
                }
            }
            out.push(Conversation { harness: g.fmt, key: g.key, title: g.title, dir, updated: g.updated, idents });
        }
        out.sort_by(|a, b| b.updated.total_cmp(&a.updated).then_with(|| a.key.cmp(&b.key)));
        Ok(out)
    }

    fn source_stamp(&self, conv: &Conversation) -> Option<String> {
        let h = harness::get(&conv.harness)?;
        match h.kind() {
            Kind::Files => harness::stat(Path::new(&conv.key)).map(|(len, nanos)| format!("{len}:{nanos}")),
            Kind::Store => h.fingerprint(self.roots.locations.of(&conv.harness), &conv.key),
        }
    }

    fn canonical_journal(&self, conv: &Conversation, scratch: &Path) -> Result<PathBuf, String> {
        let h = harness::get(&conv.harness).ok_or_else(|| format!("no converter for a '{}' session", conv.harness))?;
        let tools = harness::Tools { home: &self.roots.home, sesync_conv: &self.roots.sesync_conv };
        h.to_journal(self.roots.locations.of(&conv.harness), &tools, &conv.key, scratch)?
            .ok_or_else(|| format!("a '{}' session is already a canonical journal", conv.harness))
    }
}

impl StateCatalog {
    /// Each store harness's session id to its project directory, from one read per store.
    fn store_directories(&self) -> HashMap<(String, String), String> {
        let mut dirs = HashMap::new();
        for h in harness::ALL.iter().filter(|h| h.kind() == Kind::Store) {
            for row in h.rows(self.roots.locations.of(h.id())) {
                dirs.insert((h.id().to_string(), row.id), row.directory);
            }
        }
        dirs
    }

    /// The identifier a herdr pane's resume command carries for this session: the last argument, as
    /// `herdr_open::plan` names it. None when the harness cannot resume the session.
    fn ident(&self, fmt: &str, key: &str) -> Option<String> {
        let argv = harness::get(fmt)?.resume_argv(self.roots.locations.of(fmt), key).ok()?;
        argv.last().filter(|ident| !ident.is_empty()).cloned()
    }
}

/// Group conversations by their directory, newest project first. Each project lists its sessions
/// newest first.
pub fn group_by_dir(conversations: Vec<Conversation>) -> Vec<Project> {
    let mut by_dir: BTreeMap<String, Vec<Session>> = BTreeMap::new();
    for c in conversations {
        by_dir.entry(c.dir).or_default().push(Session { harness: c.harness, title: c.title, updated: c.updated });
    }
    let mut projects: Vec<Project> = by_dir
        .into_iter()
        .map(|(dir, mut sessions)| {
            sessions.sort_by(|a, b| b.updated.total_cmp(&a.updated).then_with(|| a.title.cmp(&b.title)));
            Project { dir, sessions }
        })
        .collect();
    projects.sort_by(|a, b| b.newest().total_cmp(&a.newest()).then_with(|| a.dir.cmp(&b.dir)));
    projects
}

/// The name shown for a project: its last path component, or a plain label for sessions whose
/// harness records no directory.
pub fn project_name(dir: &str) -> String {
    if dir.is_empty() {
        return "no project directory".into();
    }
    Path::new(dir).file_name().map_or_else(|| dir.to_string(), |n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(harness: &str, key: &str, dir: &str, updated: f64) -> Conversation {
        Conversation {
            harness: harness.into(),
            key: key.into(),
            title: key.rsplit('/').next().unwrap_or(key).into(),
            dir: dir.into(),
            updated,
            idents: vec![key.into()],
        }
    }

    #[test]
    fn conversations_group_by_directory_newest_first() {
        let projects = group_by_dir(vec![
            conv("omp", "/x/old", "/work/a", 10.0),
            conv("opencode", "ses_new", "/work/b", 30.0),
            conv("claude", "/x/mid", "/work/b", 20.0),
            conv("omp", "/x/none", "", 5.0),
        ]);
        assert_eq!(projects.len(), 3);
        assert_eq!(projects[0].dir, "/work/b");
        assert_eq!(projects[0].sessions[0].title, "ses_new");
        assert_eq!(projects[0].sessions[1].harness, "claude");
        assert_eq!(projects[1].dir, "/work/a");
        assert_eq!(projects[2].dir, "");
    }

    #[test]
    fn a_conversation_id_is_stable_and_keyed_by_the_parent_session() {
        let a = conv("omp", "/x/one.jsonl", "/work/a", 1.0);
        let moved_listing = Conversation { updated: 99.0, title: "renamed".into(), ..a.clone() };
        assert_eq!(a.worktree_id(), moved_listing.worktree_id(), "title and time do not change the id");
        let other = conv("omp", "/x/two.jsonl", "/work/a", 1.0);
        assert_ne!(a.worktree_id(), other.worktree_id());
        assert!(a.worktree_id().starts_with(&format!("{}::conv-", a.repo_id())));
        assert_eq!(a.repo_id(), repo_id("/work/a"), "the repo id matches what repo.list reports");
        assert!(!a.worktree_id().contains('/'), "no path characters in the id");
    }

    #[test]
    fn a_directory_less_project_has_a_plain_name() {
        assert_eq!(project_name(""), "no project directory");
        assert_eq!(project_name("/home/u/work/alpha"), "alpha");
    }
}
