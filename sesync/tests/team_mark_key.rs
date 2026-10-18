//! The TUI's mark key. `m` marks the selected session for the team, `m` again unmarks it, and
//! the list shows which sessions are marked. Other keys keep their meaning.
use sesync::app::{App, Ev};
use sesync::manage::Roots;
use sesync::state::State;
use std::path::PathBuf;
use std::time::Instant;

fn app_with_one_session(root: &PathBuf) -> App {
    // Roots are <agent>/sessions and <claude>/projects, as in the real layout.
    let roots = Roots {
        omp: root.join("omp").join("sessions"),
        pi: root.join("pi").join("sessions"),
        claude: root.join("claude").join("projects"),
        gemini: root.join("gemini").join("tmp"),
    };
    let dir = root.join("omp").join("sessions").join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let id = "11111111-2222-3333-4444-555555555555";
    std::fs::write(dir.join(format!("2026-10-01T00-00-00-000Z_{id}.jsonl")),
        "{\"type\":\"session\",\"id\":\"11111111-2222-3333-4444-555555555555\"}\n").unwrap();
    App::new(roots, State::default(), root.join("locks"), root.join("hs.lock"))
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-mark-key-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn m_marks_then_unmarks_the_selected_session() {
    let root = sandbox("toggle");
    let mut a = app_with_one_session(&root);
    assert!(a.rows().len() >= 1, "a session is listed");
    a.handle(Ev::Char('m'), Instant::now());
    assert!(a.is_selected_marked(), "m marks the selected session");
    a.handle(Ev::Char('m'), Instant::now());
    assert!(!a.is_selected_marked(), "m again unmarks it");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_mark_is_reported_in_the_status_line() {
    let root = sandbox("status");
    let mut a = app_with_one_session(&root);
    a.handle(Ev::Char('m'), Instant::now());
    assert!(a.status().contains("marked"), "status says marked: {}", a.status());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_rename_key_still_renames() {
    let root = sandbox("rename");
    let mut a = app_with_one_session(&root);
    a.handle(Ev::Char('r'), Instant::now());
    assert_eq!(a.mode(), sesync::app::Mode::Rename, "r still enters rename mode");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_marked_row_is_flagged_for_the_view() {
    let root = sandbox("flag");
    let mut a = app_with_one_session(&root);
    let g = a.rows()[0].clone();
    assert!(!a.is_marked(&g), "unmarked at first");
    a.handle(Ev::Char('m'), Instant::now());
    let g = a.rows()[0].clone();
    assert!(a.is_marked(&g), "the view sees the mark");
    let _ = std::fs::remove_dir_all(&root);
}
