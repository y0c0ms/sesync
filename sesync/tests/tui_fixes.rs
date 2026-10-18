//! Regression tests for TUI bugs found in review. Each test states one bug and fails on the
//! code as it was. Every file is written in a private temp directory.

use ratatui::{backend::TestBackend, Terminal};
use sesync::app::{App, Ev, Mode};
use sesync::manage::Roots;
use sesync::state::State;
use sesync::ui::draw;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const SID_A: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const SID_B: &str = "bbbbbbbb-1111-2222-3333-444444444444";
const SID_C: &str = "cccccccc-1111-2222-3333-444444444444";
const TEAM_ID: &str = "11111111-2222-3333-4444-555555555555";
const TEAM_ID_2: &str = "22222222-2222-3333-4444-555555555555";

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-tuifix-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn roots(sb: &Path) -> Roots {
    Roots {
        omp: sb.join("omp").join("sessions"),
        pi: sb.join("pi").join("sessions"),
        claude: sb.join("claude").join("projects"),
    }
}

fn write_session(sb: &Path, sid: &str, title: &str) -> PathBuf {
    let dir = sb.join("omp").join("sessions").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let p = dir.join(format!("2026-10-01T10-00-00-000Z_{sid}.jsonl"));
    fs::write(&p, format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{sid}\",\"cwd\":\"/tmp\",\"title\":\"{title}\"}}\n")).unwrap();
    p
}

fn app(sb: &Path) -> App {
    App::new(roots(sb), State::default(), sb.join("locks"), sb.join("no-sesync_lock"))
}

fn screen(app: &App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(f, app)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        for x in 0..w {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn y_imports_the_team_session_that_was_shown_when_i_was_pressed() {
    let sb = sandbox("confirm-target");
    let mut a = app(&sb);
    a.set_team_sessions(vec![(TEAM_ID.into(), b"first".to_vec())]);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    // A later pull replaces the list before the user answers.
    a.set_team_sessions(vec![(TEAM_ID_2.into(), b"second".to_vec())]);
    a.handle(Ev::Char('y'), Instant::now());
    assert_eq!(a.imported(), vec![TEAM_ID.to_string()], "the session the user confirmed is the one imported");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_pending_delete_is_cancelled_by_i_and_n() {
    let sb = sandbox("arm-i");
    let src = write_session(&sb, SID_A, "keep me");
    let mut a = app(&sb);
    a.set_team_sessions(vec![(TEAM_ID.into(), b"x".to_vec())]);
    a.handle(Ev::CtrlD, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    a.handle(Ev::Char('n'), Instant::now());
    a.handle(Ev::CtrlD, Instant::now());
    assert!(src.exists(), "a delete armed before i must not fire on a later ctrl+d");
    assert!(a.status().contains("Press ctrl+d again"), "the second ctrl+d only re-arms: {}", a.status());
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn ctrl_r_cancels_a_pending_delete() {
    let sb = sandbox("arm-ctrlr");
    let src = write_session(&sb, SID_A, "keep me");
    let mut a = app(&sb);
    a.handle(Ev::CtrlD, Instant::now());
    a.handle(Ev::CtrlR, Instant::now());
    a.handle(Ev::CtrlD, Instant::now());
    assert!(src.exists(), "ctrl+r is another key: it cancels the armed delete");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn moving_the_cursor_while_armed_says_the_delete_was_cancelled() {
    let sb = sandbox("arm-move");
    write_session(&sb, SID_A, "one");
    write_session(&sb, SID_B, "two");
    let mut a = app(&sb);
    a.handle(Ev::CtrlD, Instant::now());
    a.handle(Ev::Down, Instant::now());
    assert!(a.status().contains("cancelled"), "the stale prompt must not stay: {}", a.status());
    assert!(!a.status().contains("Press ctrl+d again"), "{}", a.status());
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_refused_rename_is_drawn_on_the_screen() {
    let sb = sandbox("refuse");
    write_session(&sb, SID_A, "one");
    let mut a = app(&sb);
    a.handle(Ev::Char('r'), Instant::now());
    while !a.input().is_empty() {
        a.handle(Ev::Backspace, Instant::now());
    }
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Rename, "a refused rename stays open");
    let text = screen(&a, 80, 6);
    assert!(text.contains("refused"), "the refusal must be visible on screen:\n{text}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_cursor_stays_on_the_same_row_after_a_delete() {
    let sb = sandbox("cursor");
    write_session(&sb, SID_A, "one");
    write_session(&sb, SID_B, "two");
    write_session(&sb, SID_C, "three");
    let mut a = app(&sb);
    assert_eq!(a.rows().len(), 3);
    a.handle(Ev::Down, Instant::now());
    assert_eq!(a.selected_index(), 1);
    a.handle(Ev::CtrlD, Instant::now());
    a.handle(Ev::CtrlD, Instant::now());
    assert_eq!(a.rows().len(), 2, "one session removed");
    assert_eq!(a.selected_index(), 1, "the cursor stays on its row, not back at row 0");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn ctrl_r_in_browse_asks_for_a_refresh_once() {
    let sb = sandbox("refresh-ask");
    let mut a = app(&sb);
    a.handle(Ev::CtrlR, Instant::now());
    assert!(a.take_refresh_request(), "ctrl+r in browse asks for a refresh");
    assert!(!a.take_refresh_request(), "the request is consumed once");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn ctrl_r_during_an_import_question_cancels_it_and_asks_for_nothing() {
    let sb = sandbox("refresh-confirm");
    let mut a = app(&sb);
    a.set_team_sessions(vec![(TEAM_ID.into(), b"x".to_vec())]);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    a.handle(Ev::CtrlR, Instant::now());
    assert_eq!(a.mode(), Mode::Browse, "ctrl+r cancels the question");
    assert!(!a.take_refresh_request(), "and does not start a refresh behind it");
    a.handle(Ev::Char('y'), Instant::now());
    assert!(a.imported().is_empty(), "a later y does not import after the cancel");
    let _ = fs::remove_dir_all(&sb);
}
