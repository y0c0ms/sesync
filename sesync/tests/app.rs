//! TUI application, driven by key events against real files in a sandbox.
//! Covers browse, the two-press delete, and rename. The real stores and the real
//! lock are never used; the lock is the sesync_lock binary with a private directory.

use sesync::app::{App, Ev, Mode};
use sesync::manage::Roots;
use sesync::state::State;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-app-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

const SID: &str = "aaaaaaaa-1111-2222-3333-444444444444";

/// One omp session on disk, plus its roots.
fn fixture(sb: &PathBuf) -> (Roots, PathBuf) {
    // Roots are <agent>/sessions and <claude>/projects, as in the real layout.
    let roots = Roots {
        omp: sb.join("omp").join("sessions"),
        pi: sb.join("pi").join("sessions"),
        claude: sb.join("claude").join("projects"),
    };
    let dir = roots.omp.join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(&roots.pi).unwrap();
    fs::create_dir_all(&roots.claude).unwrap();
    let path = dir.join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"));
    fs::write(&path, format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{SID}\",\"cwd\":\"/tmp\",\"title\":\"Old\"}}\n")).unwrap();
    (roots, path)
}

fn app(sb: &PathBuf, roots: Roots) -> App {
    App::new(roots, State::default(), sb.join("locks"), sb.join("no-sesync_lock"))
}

#[test]
fn starts_in_browse_mode_with_the_sessions_listed() {
    let sb = sandbox("browse");
    let (roots, _) = fixture(&sb);
    let a = app(&sb, roots);
    assert_eq!(a.mode(), Mode::Browse);
    assert_eq!(a.rows().len(), 1);
}

#[test]
fn ctrl_d_once_arms_and_does_not_delete() {
    let sb = sandbox("arm");
    let (roots, path) = fixture(&sb);
    let mut a = app(&sb, roots);
    a.handle(Ev::CtrlD, Instant::now());
    assert!(path.exists(), "one ctrl+d must not delete");
    assert!(a.status().contains("Press ctrl+d again"), "{}", a.status());
}

#[test]
fn ctrl_d_twice_deletes_the_session_file() {
    let sb = sandbox("delete");
    let (roots, path) = fixture(&sb);
    let mut a = app(&sb, roots);
    let t = Instant::now();
    a.handle(Ev::CtrlD, t);
    a.handle(Ev::CtrlD, t + Duration::from_secs(1));
    assert!(!path.exists(), "two ctrl+d on the same row must delete the source");
    assert!(a.status().starts_with("deleted"), "{}", a.status());
}

#[test]
fn any_other_key_cancels_a_pending_delete() {
    let sb = sandbox("cancel");
    let (roots, path) = fixture(&sb);
    let mut a = app(&sb, roots);
    let t = Instant::now();
    a.handle(Ev::CtrlD, t);
    a.handle(Ev::Char('x'), t);
    a.handle(Ev::CtrlD, t + Duration::from_secs(1));
    assert!(path.exists(), "a key in between cancels, so the next ctrl+d only re-arms");
}

#[test]
fn rename_opens_an_input_and_saves_the_new_title() {
    let sb = sandbox("rename");
    let (roots, path) = fixture(&sb);
    let mut a = app(&sb, roots);
    a.handle(Ev::Char('r'), Instant::now());
    assert_eq!(a.mode(), Mode::Rename);
    for c in "New name".chars() {
        a.handle(Ev::Char(c), Instant::now());
    }
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert!(fs::read_to_string(&path).unwrap().contains("New name"));
}

#[test]
fn rename_with_an_invalid_title_keeps_the_input_open_and_changes_nothing() {
    let sb = sandbox("rename-bad");
    let (roots, path) = fixture(&sb);
    let before = fs::read_to_string(&path).unwrap();
    let mut a = app(&sb, roots);
    a.handle(Ev::Char('r'), Instant::now());
    // The input starts pre-filled with the current title; clear it to make it empty.
    for _ in 0..a.input().chars().count() {
        a.handle(Ev::Backspace, Instant::now());
    }
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Rename, "an invalid title must keep the input open");
    assert!(a.status().contains("empty"), "{}", a.status());
    assert_eq!(fs::read_to_string(&path).unwrap(), before, "nothing may be written");
}

#[test]
fn escape_cancels_rename_without_writing() {
    let sb = sandbox("rename-esc");
    let (roots, path) = fixture(&sb);
    let before = fs::read_to_string(&path).unwrap();
    let mut a = app(&sb, roots);
    a.handle(Ev::Char('r'), Instant::now());
    a.handle(Ev::Char('z'), Instant::now());
    a.handle(Ev::Esc, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
}

#[test]
fn q_quits() {
    let sb = sandbox("quit");
    let (roots, _) = fixture(&sb);
    let mut a = app(&sb, roots);
    a.handle(Ev::Char('q'), Instant::now());
    assert!(a.should_quit());
}

/// A second session in the same folder, so the list has two rows.
fn two_sessions(sb: &PathBuf) -> (Roots, PathBuf, PathBuf) {
    let (roots, first) = fixture(sb);
    let id2 = "bbbbbbbb-1111-2222-3333-444444444444";
    let second = roots.omp.join("-tmp").join(format!("2026-10-02T10-00-00-000Z_{id2}.jsonl"));
    fs::write(&second, format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id2}\",\"cwd\":\"/tmp\",\"title\":\"Second\"}}\n")).unwrap();
    (roots, first, second)
}

#[test]
fn arrow_keys_move_the_cursor_and_stop_at_the_ends() {
    let sb = sandbox("cursor");
    let (roots, _, _) = two_sessions(&sb);
    let mut a = app(&sb, roots);
    let now = Instant::now();
    assert_eq!(a.rows().len(), 2);
    assert_eq!(a.selected_index(), 0);
    a.handle(Ev::Down, now);
    assert_eq!(a.selected_index(), 1);
    a.handle(Ev::Down, now);
    assert_eq!(a.selected_index(), 1, "down on the last row stays there");
    a.handle(Ev::Up, now);
    assert_eq!(a.selected_index(), 0);
    a.handle(Ev::Up, now);
    assert_eq!(a.selected_index(), 0, "up on the first row stays there");
    a.handle(Ev::End, now);
    assert_eq!(a.selected_index(), 1);
    a.handle(Ev::Home, now);
    assert_eq!(a.selected_index(), 0);
}

#[test]
fn moving_the_cursor_cancels_an_armed_delete() {
    let sb = sandbox("cursor-cancel");
    let (roots, first, _) = two_sessions(&sb);
    let mut a = app(&sb, roots);
    let now = Instant::now();
    a.handle(Ev::CtrlD, now); // arms row 0
    a.handle(Ev::Down, now);  // moving cancels the arm
    a.handle(Ev::Up, now);    // back on row 0
    a.handle(Ev::CtrlD, now); // a fresh first press only re-arms
    assert!(first.exists(), "a delete must need two presses with no cursor move between them");
    assert!(a.status().contains("Press ctrl+d again"), "{}", a.status());
}
