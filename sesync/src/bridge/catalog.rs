//! Sessions as the phone sees them: one project per directory, newest project first. Built from the
//! same listing the TUI uses (`manage::list_groups`), read-only. Nothing is written, and the daemon's
//! save-time steps are not run, so a session the TUI hides only in memory may show here until the
//! daemon next saves the state.

use crate::harness::{self, Kind};
use crate::manage::{self, Roots};
use crate::state::State;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The harness id: `omp`, `claude`, and so on.
    pub harness: String,
    pub title: String,
    /// Seconds since the epoch; 0 when unknown.
    pub updated: f64,
}

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

/// Anything that can list projects. The server holds one behind an `Arc`, so a test can serve a
/// fixed list and the CLI can serve the real one.
pub trait Catalog: Send + Sync {
    fn projects(&self) -> Result<Vec<Project>, String>;
}

/// The real catalog: the saved sesync state plus the harness stores under `roots`.
pub struct StateCatalog {
    pub roots: Roots,
    pub state_file: PathBuf,
}

impl Catalog for StateCatalog {
    fn projects(&self) -> Result<Vec<Project>, String> {
        let state = State::load(&self.state_file)
            .map_err(|e| format!("cannot read the sesync state: {e}"))?;
        Ok(group(manage::list_groups(&self.roots, &state), self.store_directories()))
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
}

/// Group listed sessions by their directory. Orphans (recorded but gone from disk) are not
/// sessions the phone can open, so they are left out.
fn group(groups: Vec<manage::Group>, store_dirs: HashMap<(String, String), String>) -> Vec<Project> {
    let mut by_dir: BTreeMap<String, Vec<Session>> = BTreeMap::new();
    for g in groups.into_iter().filter(|g| !g.orphan) {
        let dir = match harness::get(&g.fmt) {
            Some(h) if h.kind() == Kind::Files => h.cwd(Path::new(&g.key)).unwrap_or_default(),
            Some(_) => store_dirs.get(&(g.fmt.clone(), g.key.clone())).cloned().unwrap_or_default(),
            None => String::new(),
        };
        by_dir.entry(dir).or_default().push(Session { harness: g.fmt, title: g.title, updated: g.updated });
    }
    let mut projects: Vec<Project> = by_dir.into_iter().map(|(dir, mut sessions)| {
        sessions.sort_by(|a, b| b.updated.total_cmp(&a.updated).then_with(|| a.title.cmp(&b.title)));
        Project { dir, sessions }
    }).collect();
    projects.sort_by(|a, b| b.newest().total_cmp(&a.newest()).then_with(|| a.dir.cmp(&b.dir)));
    projects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(fmt: &str, title: &str, updated: f64) -> manage::Group {
        let mut g = manage::Group::file(fmt, format!("/x/{title}"), title);
        g.updated = updated;
        g
    }

    #[test]
    fn sessions_group_by_directory_newest_first() {
        let mut orphan = session("omp", "gone", 99.0);
        orphan.orphan = true;
        let groups = vec![
            session("omp", "old", 10.0),
            // opencode is a store harness: its directory comes from the store's rows.
            session("opencode", "new", 30.0),
            orphan,
        ];
        let mut dirs = HashMap::new();
        dirs.insert(("opencode".to_string(), "/x/new".to_string()), "/work/b".to_string());
        // omp is a file harness whose cwd is read from a file that is not on disk here, so it lands
        // in the empty directory.
        let projects = group(groups, dirs);
        assert_eq!(projects.len(), 2, "the orphan is left out");
        assert_eq!(projects[0].dir, "/work/b");
        assert_eq!(projects[0].sessions[0].title, "new");
        assert_eq!(projects[1].dir, "");
        assert_eq!(projects[1].sessions[0].title, "old");
    }
}
