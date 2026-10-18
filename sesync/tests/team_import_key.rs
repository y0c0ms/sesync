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
    let roots = Roots {
        omp: root.join("omp").join("sessions"),
        pi: root.join("pi").join("sessions"),
        claude: root.join("claude").join("projects"),
    };
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

#[test]
fn y_imports_the_confirmed_session() {
    let root = sandbox("yes");
    let mut a = app_with_team_session(&root);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    a.handle(Ev::Char('y'), Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert_eq!(a.imported(), vec![TEAM_ID.to_string()], "the confirmed session is imported");
    let listed = std::fs::read_dir(root.join("omp").join("sessions")).unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned())).count();
    assert_eq!(listed, 1, "the imported file is in a place the harness reads");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn i_with_no_team_session_does_nothing() {
    let root = sandbox("none");
    let roots = Roots { omp: root.join("omp"), pi: root.join("pi"), claude: root.join("claude") };
    let mut a = App::new(roots, State::default(), root.join("locks"), root.join("hs.lock"));
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    assert_eq!(a.mode(), Mode::Browse, "no team session, no confirmation");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_team_id_the_harness_cannot_list_is_refused() {
    let root = sandbox("badid");
    let roots = Roots { omp: root.join("omp").join("sessions"), pi: root.join("pi").join("sessions"),
                        claude: root.join("claude").join("projects") };
    let mut a = App::new(roots, State::default(), root.join("locks"), root.join("hs.lock"));
    a.set_team_sessions(vec![("ses_not_a_uuid".into(), b"x".to_vec())]);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    a.handle(Ev::Char('y'), Instant::now());
    assert!(a.imported().is_empty(), "an id the harness cannot list is not imported");
    assert!(a.status().contains("not imported"), "{}", a.status());
    let _ = std::fs::remove_dir_all(&root);
}
