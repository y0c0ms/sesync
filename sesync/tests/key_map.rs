//! The key map from terminal events to app events. Raw mode turns Ctrl+C into an ordinary key,
//! so the map must send it to the clipboard-copy handler, and must not let a control or alt
//! letter act as a plain letter (Ctrl+Q used to quit, because the modifier was dropped).

use crossterm::event::{KeyCode, KeyModifiers};
use sesync::app::{App, Ev};
use sesync::keys::key_to_ev;
use sesync::manage::Roots;
use sesync::state::State;
use std::path::PathBuf;
use std::time::Instant;

#[test]
fn ctrl_d_and_ctrl_r_keep_their_meaning() {
    assert_eq!(key_to_ev(KeyCode::Char('d'), KeyModifiers::CONTROL), Some(Ev::CtrlD));
    assert_eq!(key_to_ev(KeyCode::Char('r'), KeyModifiers::CONTROL), Some(Ev::CtrlR));
}

#[test]
fn ctrl_c_is_quit_not_a_letter() {
    assert_eq!(key_to_ev(KeyCode::Char('c'), KeyModifiers::CONTROL), Some(Ev::CtrlC));
}

#[test]
fn other_control_and_alt_letters_do_nothing() {
    assert_eq!(key_to_ev(KeyCode::Char('q'), KeyModifiers::CONTROL), None, "ctrl+q must not quit");
    assert_eq!(key_to_ev(KeyCode::Char('x'), KeyModifiers::CONTROL), None);
    assert_eq!(key_to_ev(KeyCode::Char('q'), KeyModifiers::ALT), None, "alt+q must not quit");
}

#[test]
fn plain_and_shifted_letters_are_typed() {
    assert_eq!(key_to_ev(KeyCode::Char('q'), KeyModifiers::NONE), Some(Ev::Char('q')));
    assert_eq!(key_to_ev(KeyCode::Char('Q'), KeyModifiers::SHIFT), Some(Ev::Char('Q')));
}

#[test]
fn navigation_and_editing_keys_map() {
    assert_eq!(key_to_ev(KeyCode::Up, KeyModifiers::NONE), Some(Ev::Up));
    assert_eq!(key_to_ev(KeyCode::Down, KeyModifiers::NONE), Some(Ev::Down));
    assert_eq!(key_to_ev(KeyCode::PageUp, KeyModifiers::NONE), Some(Ev::PageUp));
    assert_eq!(key_to_ev(KeyCode::PageDown, KeyModifiers::NONE), Some(Ev::PageDown));
    assert_eq!(key_to_ev(KeyCode::Home, KeyModifiers::NONE), Some(Ev::Home));
    assert_eq!(key_to_ev(KeyCode::End, KeyModifiers::NONE), Some(Ev::End));
    assert_eq!(key_to_ev(KeyCode::Enter, KeyModifiers::NONE), Some(Ev::Enter));
    assert_eq!(key_to_ev(KeyCode::Esc, KeyModifiers::NONE), Some(Ev::Esc));
    assert_eq!(key_to_ev(KeyCode::Backspace, KeyModifiers::NONE), Some(Ev::Backspace));
    assert_eq!(key_to_ev(KeyCode::F(1), KeyModifiers::NONE), None);
}

/// An app with one session on disk, so rename has a row to act on.
fn empty_app() -> App {
    let d: PathBuf = std::env::temp_dir().join(format!("sesync-keymap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let dir = d.join("omp").join("sessions").join("-tmp");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(d.join("pi")).unwrap();
    std::fs::create_dir_all(d.join("claude")).unwrap();
    let sid = "aaaaaaaa-1111-2222-3333-444444444444";
    std::fs::write(dir.join(format!("2026-10-01T10-00-00-000Z_{sid}.jsonl")),
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{sid}\",\"cwd\":\"/tmp\",\"title\":\"one\"}}\n")).unwrap();
    let roots = Roots::under(&d, d.join("no-sesync-conv"));
    App::new(roots, State::default(), d.join("locks"), d.join("no-sesync_lock"))
}

#[test]
fn ctrl_c_copies_the_selected_session_path_and_says_to_use_q() {
    let mut a = empty_app();
    a.handle(Ev::CtrlC, Instant::now());
    assert!(!a.should_quit(), "ctrl+c must not quit in browse mode");
    assert!(a.status().contains("copied"), "ctrl+c copies: {}", a.status());
    assert!(a.status().contains("q to quit"), "ctrl+c says how to quit: {}", a.status());
    assert!(a.status().contains("aaaaaaaa-1111-2222-3333-444444444444"), "the path is named: {}", a.status());

    let mut a = empty_app();
    a.handle(Ev::Char('r'), Instant::now());
    a.handle(Ev::CtrlC, Instant::now());
    assert!(a.should_quit() == false, "ctrl+c must not quit in rename mode");
    assert!(a.status().contains("copied"), "{}", a.status());

    let mut a = empty_app();
    a.set_team_sessions(vec![("11111111-2222-3333-4444-555555555555".into(), b"x".to_vec())]);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('i'), Instant::now());
    a.handle(Ev::CtrlC, Instant::now());
    assert!(a.should_quit() == false, "ctrl+c must not quit in confirm mode");
}

#[test]
fn ctrl_c_copies_nothing_when_the_list_is_empty() {
    let sb = std::env::temp_dir().join(format!("hs-keymap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sb);
    std::fs::create_dir_all(sb.join("omp").join("sessions").join("-tmp")).unwrap();
    std::fs::create_dir_all(sb.join("pi")).unwrap();
    std::fs::create_dir_all(sb.join("claude")).unwrap();
    let roots = Roots::under(&sb, sb.join("no-sesync-conv"));
    let mut a = App::new(roots, State::default(), sb.join("locks"), sb.join("no-sesync_lock"));
    a.handle(Ev::CtrlC, Instant::now());
    assert!(!a.should_quit());
    assert!(a.status().contains("nothing selected"), "{}", a.status());
}
