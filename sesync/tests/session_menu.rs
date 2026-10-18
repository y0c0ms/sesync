//! The per-session menu and the mirror-target picker, driven by key events in a sandbox. Enter on a
//! local session opens its options; the picker only offers installed harnesses other than the
//! session's own format; nothing is saved until the picker is confirmed. No real store is used.
use sesync::app::{App, Ev, Mode};
use sesync::manage::Roots;
use sesync::state::State;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

const SID: &str = "aaaaaaaa-1111-2222-3333-444444444444";

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-menu-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn fixture(sb: &PathBuf) -> Roots {
    let roots = Roots {
        omp: sb.join("omp").join("sessions"),
        pi: sb.join("pi").join("sessions"),
        claude: sb.join("claude").join("projects"),
    };
    let dir = roots.omp.join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(&roots.pi).unwrap();
    fs::create_dir_all(&roots.claude).unwrap();
    fs::write(
        dir.join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl")),
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{SID}\",\"cwd\":\"/tmp\",\"title\":\"Old\"}}\n"),
    ).unwrap();
    roots
}

fn app(sb: &PathBuf, roots: Roots) -> App {
    App::new(roots, State::default(), sb.join("locks"), sb.join("no-sesync_lock"))
}

fn press(a: &mut App, ev: Ev) {
    a.handle(ev, Instant::now());
}

#[test]
fn enter_on_a_session_opens_its_menu_not_the_palette() {
    let sb = sandbox("enter");
    let mut a = app(&sb, fixture(&sb));
    press(&mut a, Ev::Enter);
    assert_eq!(a.mode(), Mode::SessionMenu, "Enter on a session opens its options");
}

#[test]
fn ctrl_p_still_opens_the_global_palette() {
    let sb = sandbox("ctrlp");
    let mut a = app(&sb, fixture(&sb));
    press(&mut a, Ev::CtrlP);
    assert_eq!(a.mode(), Mode::Palette, "ctrl+p keeps the command palette");
}

#[test]
fn escape_closes_the_menu_without_running_anything() {
    let sb = sandbox("esc");
    let mut a = app(&sb, fixture(&sb));
    press(&mut a, Ev::Enter);
    press(&mut a, Ev::Esc);
    assert_eq!(a.mode(), Mode::Browse);
}

#[test]
fn the_menu_lists_the_session_actions() {
    let sb = sandbox("rows");
    let a = app(&sb, fixture(&sb));
    let titles: Vec<&str> = a.session_menu_rows().iter().map(|(t, _)| *t).collect();
    assert_eq!(titles, vec!["Rename", "Mark / unmark for team", "Mirror to harness", "Delete"]);
}

#[test]
fn rename_from_the_menu_enters_rename_mode() {
    let sb = sandbox("rename");
    let mut a = app(&sb, fixture(&sb));
    press(&mut a, Ev::Enter);
    press(&mut a, Ev::Enter); // row 0: Rename
    assert_eq!(a.mode(), Mode::Rename, "Rename from the menu runs the same path as the r key");
}

#[test]
fn mark_from_the_menu_marks_the_session_for_the_team() {
    let sb = sandbox("mark");
    let mut a = app(&sb, fixture(&sb));
    press(&mut a, Ev::Enter);
    press(&mut a, Ev::Down);
    press(&mut a, Ev::Enter); // row 1: Mark
    assert_eq!(a.mode(), Mode::Browse);
    assert!(a.status().contains("marked"), "status: {}", a.status());
}

#[test]
fn mirror_picker_offers_only_installed_harnesses_other_than_the_source() {
    let sb = sandbox("picker");
    let mut a = app(&sb, fixture(&sb));
    a.set_installed_targets(vec!["omp".into(), "pi".into(), "claude".into()]);
    press(&mut a, Ev::Enter);
    press(&mut a, Ev::Down);
    press(&mut a, Ev::Down);
    press(&mut a, Ev::Enter); // row 2: Mirror to harness
    assert_eq!(a.mode(), Mode::MirrorPick);
    let targets = a.picker_targets();
    assert!(!targets.contains(&"omp".to_string()), "the source's own format is never offered");
    assert_eq!(targets, vec!["pi".to_string(), "claude".to_string()]);
}

#[test]
fn opening_the_picker_writes_nothing_until_confirmed() {
    let sb = sandbox("noop");
    let mut a = app(&sb, fixture(&sb));
    a.set_installed_targets(vec!["pi".into()]);
    press(&mut a, Ev::Enter);
    press(&mut a, Ev::Down);
    press(&mut a, Ev::Down);
    press(&mut a, Ev::Enter);
    press(&mut a, Ev::Esc);
    assert_eq!(a.mode(), Mode::Browse);
    assert!(a.status().contains("cancelled"), "status: {}", a.status());
}

#[test]
fn confirming_the_picker_saves_the_chosen_targets_to_state() {
    let sb = sandbox("confirm");
    let mut a = app(&sb, fixture(&sb));
    a.set_installed_targets(vec!["pi".into(), "claude".into()]);
    press(&mut a, Ev::Enter);
    press(&mut a, Ev::Down);
    press(&mut a, Ev::Down);
    press(&mut a, Ev::Enter); // open picker; both targets ticked by default
    press(&mut a, Ev::Enter); // confirm
    assert_eq!(a.mode(), Mode::Browse);
    let key = a.rows()[0].key.clone();
    let chosen: Vec<String> = a.state().sources[&key].chosen_targets.iter().cloned().collect();
    assert_eq!(chosen, vec!["claude".to_string(), "pi".to_string()]);
}

#[test]
fn unticking_everything_clears_the_mirror_intent() {
    let sb = sandbox("clear");
    let mut a = app(&sb, fixture(&sb));
    a.set_installed_targets(vec!["pi".into()]);
    press(&mut a, Ev::Enter);
    press(&mut a, Ev::Down);
    press(&mut a, Ev::Down);
    press(&mut a, Ev::Enter); // picker, pi ticked
    press(&mut a, Ev::Char(' ')); // untick pi
    press(&mut a, Ev::Enter);
    let key = a.rows()[0].key.clone();
    assert!(a.state().sources.get(&key).map_or(true, |s| s.chosen_targets.is_empty()),
        "an empty choice means nothing is mirrored");
}
