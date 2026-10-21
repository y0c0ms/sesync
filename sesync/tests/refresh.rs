//! The TUI's space refresh: Ctrl+R asks the team server for the selected space's sessions and
//! shows any that are new. The network is behind a trait, so these tests need no server.

use sesync::app::{App, Ev};
use sesync::remote::{RemoteError, RemoteSessions, SpaceSession};
use sesync::state::State;
use sesync::manage::Roots;
use std::cell::RefCell;
use std::time::Instant;

struct Fake {
    sessions: Vec<SpaceSession>,
    fail: bool,
    calls: RefCell<Vec<String>>,
}

impl RemoteSessions for Fake {
    fn list(&self, space: &str) -> Result<Vec<SpaceSession>, RemoteError> {
        self.calls.borrow_mut().push(space.to_string());
        if self.fail { Err(RemoteError::Unreachable) } else { Ok(self.sessions.clone()) }
    }
}

fn app() -> App {
    let root = std::env::temp_dir().join(format!("refresh-test-{}", std::process::id()));
    let roots = Roots::under(&root, root.join("no-sesync-conv"));
    App::new(roots, State::default(), root.join("locks"), root.join("hs.lock"))
}

fn session(id: &str) -> SpaceSession {
    SpaceSession { id: id.into(), title: format!("title {id}"), updated_at: 1 }
}

#[test]
fn ctrl_r_refresh_asks_the_server_for_the_selected_space() {
    let mut a = app();
    let fake = Fake { sessions: vec![session("s1")], fail: false, calls: RefCell::new(vec![]) };
    a.select_space("team-a");
    a.refresh_space(&fake, Instant::now());
    assert_eq!(fake.calls.borrow().as_slice(), ["team-a".to_string()]);
    assert_eq!(a.remote_rows().len(), 1, "the refreshed session is shown");
}

#[test]
fn a_refresh_shows_only_the_sessions_that_are_new() {
    let mut a = app();
    let first = Fake { sessions: vec![session("s1")], fail: false, calls: RefCell::new(vec![]) };
    a.select_space("team-a");
    a.refresh_space(&first, Instant::now());
    let second = Fake { sessions: vec![session("s1"), session("s2")], fail: false, calls: RefCell::new(vec![]) };
    a.refresh_space(&second, Instant::now());
    assert_eq!(a.new_remote_ids(), vec!["s2".to_string()], "only s2 is new since the last refresh");
}

#[test]
fn an_unreachable_server_leaves_the_list_alone_and_says_so() {
    let mut a = app();
    let ok = Fake { sessions: vec![session("s1")], fail: false, calls: RefCell::new(vec![]) };
    a.select_space("team-a");
    a.refresh_space(&ok, Instant::now());
    let down = Fake { sessions: vec![], fail: true, calls: RefCell::new(vec![]) };
    a.refresh_space(&down, Instant::now());
    assert_eq!(a.remote_rows().len(), 1, "the last good list is kept");
    assert!(a.status().contains("server unreachable"), "the status says the refresh failed: {}", a.status());
}

#[test]
fn refresh_with_no_space_selected_does_nothing() {
    let mut a = app();
    let fake = Fake { sessions: vec![session("s1")], fail: false, calls: RefCell::new(vec![]) };
    a.refresh_space(&fake, Instant::now());
    assert!(fake.calls.borrow().is_empty(), "no space, no request");
    assert!(a.status().contains("no space"), "the status explains why: {}", a.status());
}

#[test]
fn ctrl_r_is_a_distinct_event_from_rename() {
    // The rename key 'r' must keep working; the refresh key is Ctrl+R.
    let mut a = app();
    a.handle(Ev::CtrlR, Instant::now());
    assert_eq!(a.mode(), sesync::app::Mode::Browse, "Ctrl+R must not enter rename mode");
}

