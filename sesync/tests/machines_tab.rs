//! The Machines tab's chosen view is remembered across runs. The tab cycle itself is in `tui_views.rs`.

use sesync::app::{prefs, App, View};
use sesync::manage::Roots;
use sesync::state::State;
use std::fs;
use std::path::{Path, PathBuf};

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-machines-tab-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn app(sb: &Path) -> App {
    App::new(Roots::under(sb, sb.join("no-sesync-conv")), State::default(), sb.join("locks"), sb.join("no-sesync_lock"))
}

#[test]
fn the_machines_tab_is_remembered_across_runs() {
    let sb = sandbox("prefs");
    prefs::save(&sb, "", View::Machines).unwrap();
    let saved = prefs::load(&sb);
    assert_eq!(saved.view, Some(View::Machines));
    let mut a = app(&sb);
    a.restore_prefs(saved.theme, saved.view);
    assert_eq!(a.view(), View::Machines);
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn tab_moves_through_every_view_and_never_closes_the_popup() {
    let sb = sandbox("tab-cycle");
    let mut a = app(&sb);
    a.restore_prefs(None, Some(View::Machines));
    a.handle(sesync::app::Ev::Tab, std::time::Instant::now());
    assert_eq!(a.view(), View::Sessions, "Machines goes on to Sessions");
    assert!(!a.should_quit(), "a Tab from Machines does not end the popup");
    a.handle(sesync::app::Ev::Tab, std::time::Instant::now());
    assert_eq!(a.view(), View::Team);
    a.handle(sesync::app::Ev::Tab, std::time::Instant::now());
    assert_eq!(a.view(), View::Machines);
    a.handle(sesync::app::Ev::Tab, std::time::Instant::now());
    assert_eq!(a.view(), View::Sessions, "the cycle comes back to Sessions");
    assert!(!a.should_quit(), "a full cycle keeps the popup open");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn moving_between_sessions_and_team_keeps_the_popup_open() {
    let sb = sandbox("keep-open");
    let mut a = app(&sb);
    a.handle(sesync::app::Ev::Tab, std::time::Instant::now());
    assert_eq!(a.view(), View::Team);
    assert!(!a.should_quit(), "Sessions to Team does not end the popup");
    let _ = fs::remove_dir_all(&sb);
}
