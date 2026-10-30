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
