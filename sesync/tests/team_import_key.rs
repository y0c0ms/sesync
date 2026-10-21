//! The TUI import key. `i` asks for confirmation; only `y` imports, anything else cancels. The
//! import only ever writes a team session that has been pulled and confirmed; with no team
//! session selected, `i` does nothing.
use sesync::app::{App, Ev, Mode};
use sesync::manage::Roots;
use sesync::state::State;
use std::path::PathBuf;
use std::time::Instant;

/// A harness session id: the form the harness lists. Team ids must be written in this form.
const TEAM_ID: &str = "11111111-2222-3333-4444-555555555555";

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-imp-key-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn app_with_team_session(root: &PathBuf) -> App {
    let roots = Roots::under(&root, root.join("no-sesync-conv"));
    let mut a = App::new(roots, State::default(), root.join("locks"), root.join("hs.lock"));
    a.set_team_sessions(vec![(TEAM_ID.into(), b"team body".to_vec())]);
    a
}

#[test]
fn i_asks_for_confirmation_before_anything_is_written() {
    let root = sandbox("ask");
    let mut a = app_with_team_session(&root);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    assert_eq!(a.mode(), Mode::Confirm, "i enters confirmation");
    assert!(a.status().contains("import"), "the question is shown: {}", a.status());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn only_y_imports_anything_else_cancels() {
    let root = sandbox("yn");
    let mut a = app_with_team_session(&root);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    a.handle(Ev::Char('n'), Instant::now());
    assert_eq!(a.mode(), Mode::Browse, "n cancels");
    assert!(a.imported().is_empty(), "nothing imported after n");
    let _ = std::fs::remove_dir_all(&root);
}

/// A team session as a pull returns it: a canonical journal whose header says where and when the
/// conversation started.
fn team_journal(id: &str) -> Vec<u8> {
    format!("{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"/home/u/proj\",\"timestamp\":\"2026-10-01T10:00:00.000Z\"}}\n\
             {{\"type\":\"message\",\"id\":\"m1\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n").into_bytes()
}

#[test]
fn y_imports_the_confirmed_session_where_the_harness_lists_it() {
    let root = sandbox("yes");
    let mut a = app_with_team_session(&root);
    a.set_team_sessions(vec![(TEAM_ID.into(), team_journal(TEAM_ID))]);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    a.handle(Ev::Char('y'), Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert_eq!(a.imported(), vec![TEAM_ID.to_string()], "the confirmed session is imported");
    // omp files a project outside its home and temp root under the absolute form, and names the
    // file after the start time; its own listing reads only `sessions/<folder>/<name>`.
    let expected = root.join("omp").join("sessions").join("--home-u-proj--")
        .join(format!("2026-10-01T10-00-00-000Z_{TEAM_ID}.jsonl"));
    assert!(expected.is_file(), "the session is filed in its project folder: {}", expected.display());
    let roots = Roots::under(&root, root.join("no-sesync-conv"));
    let omp = sesync::harness::get("omp").unwrap();
    assert_eq!(omp.sessions(roots.locations.of("omp")), vec![expected], "omp's own listing finds it");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_session_the_harness_already_holds_is_not_imported_again() {
    let root = sandbox("again");
    let mut a = app_with_team_session(&root);
    a.set_team_sessions(vec![(TEAM_ID.into(), team_journal(TEAM_ID))]);
    let own = root.join("omp").join("sessions").join("-elsewhere")
        .join(format!("2026-09-01T10-00-00-000Z_{TEAM_ID}.jsonl"));
    std::fs::create_dir_all(own.parent().unwrap()).unwrap();
    std::fs::write(&own, "this machine's own copy").unwrap();
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    a.handle(Ev::Char('y'), Instant::now());
    assert!(a.imported().is_empty(), "the id is already a session here");
    assert!(a.status().contains("not imported"), "{}", a.status());
    let roots = Roots::under(&root, root.join("no-sesync-conv"));
    let omp = sesync::harness::get("omp").unwrap();
    assert_eq!(omp.sessions(roots.locations.of("omp")), vec![own.clone()], "no second copy was written");
    assert_eq!(std::fs::read_to_string(&own).unwrap(), "this machine's own copy");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn i_with_no_team_session_does_nothing() {
    let root = sandbox("none");
    let roots = Roots::under(&root, root.join("no-sesync-conv"));
    let mut a = App::new(roots, State::default(), root.join("locks"), root.join("hs.lock"));
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    assert_eq!(a.mode(), Mode::Browse, "no team session, no confirmation");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_team_id_the_harness_cannot_list_is_refused() {
    let root = sandbox("badid");
    let roots = Roots::under(&root, root.join("no-sesync-conv"));
    let mut a = App::new(roots, State::default(), root.join("locks"), root.join("hs.lock"));
    a.set_team_sessions(vec![("ses_not_a_uuid".into(), b"x".to_vec())]);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    a.handle(Ev::Char('y'), Instant::now());
    assert!(a.imported().is_empty(), "an id the harness cannot list is not imported");
    assert!(a.status().contains("not imported"), "{}", a.status());
    let _ = std::fs::remove_dir_all(&root);
}
