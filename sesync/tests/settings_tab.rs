//! The Settings tab: the folder files from other machines and the phone arrive in. The bridge reads the same
//! `transfer.json` on every upload, so what the tab saves is what the bridge uses next.

use sesync::app::{prefs, App, Ev, Mode, View};
use sesync::bridge::transfer;
use sesync::manage::Roots;
use sesync::state::State;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-settings-tab-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// An app whose config directory is `<sb>/config`, already on the Settings tab.
fn on_settings(sb: &Path) -> App {
    let mut a = App::new(Roots::under(sb, sb.join("no-sesync-conv")), State::default(), sb.join("locks"), sb.join("no-sesync_lock"));
    a.set_config_dir(sb.join("config"));
    a.restore_prefs(None, Some(View::Settings));
    assert_eq!(a.view(), View::Settings);
    a
}

fn type_text(a: &mut App, text: &str) {
    for c in text.chars() {
        a.handle(Ev::Char(c), Instant::now());
    }
}

/// What the bridge would use for the next upload.
fn bridge_inbox(sb: &Path) -> PathBuf {
    transfer::inbox_dir(&sb.join("config").join("bridge"))
}

#[test]
fn enter_types_a_folder_and_the_bridge_uses_it_from_the_next_upload() {
    let sb = sandbox("save");
    let mut a = on_settings(&sb);
    assert!(a.inbox_is_default(), "nothing is chosen yet");
    let chosen = sb.join("received files");
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Settings);
    // The buffer starts at the current folder; clear it and type the new one.
    while !a.input().is_empty() {
        a.handle(Ev::Backspace, Instant::now());
    }
    type_text(&mut a, chosen.to_str().unwrap());
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert_eq!(bridge_inbox(&sb), chosen);
    assert!(chosen.is_dir(), "a folder that did not exist is created");
    assert_eq!(a.inbox_folder(), chosen.to_string_lossy());
    assert!(!a.inbox_is_default());
}

#[test]
fn a_folder_that_cannot_be_used_is_refused_and_the_old_one_stays() {
    let sb = sandbox("refuse");
    let mut a = on_settings(&sb);
    let before = bridge_inbox(&sb);
    a.handle(Ev::Enter, Instant::now());
    while !a.input().is_empty() {
        a.handle(Ev::Backspace, Instant::now());
    }
    type_text(&mut a, "relative/not/absolute");
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Settings, "the input stays open so the folder can be fixed");
    assert!(a.status().contains("refused"), "status was {:?}", a.status());
    assert_eq!(bridge_inbox(&sb), before, "nothing was saved");
    assert!(!sb.join("config").join(transfer::SETTINGS_FILE).exists());
}

#[test]
fn esc_leaves_the_folder_as_it_was() {
    let sb = sandbox("cancel");
    let mut a = on_settings(&sb);
    let before = bridge_inbox(&sb);
    a.handle(Ev::Enter, Instant::now());
    type_text(&mut a, "x");
    a.handle(Ev::Esc, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert_eq!(bridge_inbox(&sb), before);
    assert!(a.input().is_empty(), "the typed text does not leak into the next edit");
}

#[test]
fn the_settings_tab_is_remembered_across_runs() {
    let sb = sandbox("prefs");
    let config = sb.join("config");
    prefs::save(&config, "", View::Settings).unwrap();
    assert_eq!(prefs::load(&config).view, Some(View::Settings));
}
