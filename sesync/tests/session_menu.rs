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
        gemini: sb.join("gemini").join("tmp"),
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

/// Enter on the only session, then two rows down, then Enter: the mirror-target picker.
fn open_picker(a: &mut App) {
    press(a, Ev::Enter);
    press(a, Ev::Down);
    press(a, Ev::Down);
    press(a, Ev::Enter);
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
fn only_a_key_that_changes_a_choice_marks_state_for_saving() {
    // The TUI saves state only after a key that changed something user-owned: every other
    // keypress must not rewrite the state file.
    let sb = sandbox("dirty");
    let mut a = app(&sb, fixture(&sb));
    assert!(!a.take_user_dirty(), "a fresh app has nothing to save");
    press(&mut a, Ev::Down);
    assert!(!a.take_user_dirty(), "moving the cursor changes nothing to save");
    press(&mut a, Ev::Char('m'));
    assert!(a.take_user_dirty(), "a mark must be saved");
    assert!(!a.take_user_dirty(), "taking the flag clears it");
}

#[test]
fn a_chosen_mirror_with_no_file_yet_is_marked_pending_or_failed() {
    // The MIRRORS column must show a chosen target the daemon has not written, and must say
    // when the write has been failing — an invisible failure is how a mirror gets forgotten.
    let sb = sandbox("mirror-marks");
    let roots = fixture(&sb);
    let key = roots.omp.join("-tmp")
        .join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"))
        .display().to_string();
    let group = sesync::manage::Group::file("omp", key.clone(), "Old");

    let chosen = |failures: u32| {
        let mut src = sesync::state::Source::new("omp", &key);
        src.chosen_targets.insert("pi".into());
        src.failures = failures;
        let mut st = State::default();
        st.sources.insert(key.clone(), src);
        st
    };
    let a = App::new(fixture(&sb), chosen(0), sb.join("locks"), sb.join("no-sesync_lock"));
    assert_eq!(a.session_cols(&group).mirrors, "pi?", "pending shows a question mark");
    let a = App::new(fixture(&sb), chosen(3), sb.join("locks"), sb.join("no-sesync_lock"));
    assert_eq!(a.session_cols(&group).mirrors, "pi!", "a failing write shows a bang");
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
    assert_eq!(titles, vec!["Rename", "Mark / unmark for team", "Mirror to harness", "Show transcript", "Copy its path", "Delete"]);
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
fn mirror_picker_offers_gemini_once_installed() {
    let sb = sandbox("picker-gemini");
    let mut a = app(&sb, fixture(&sb));
    a.set_installed_targets(vec!["omp".into(), "gemini".into(), "pi".into()]);
    press(&mut a, Ev::Enter);
    press(&mut a, Ev::Down);
    press(&mut a, Ev::Down);
    press(&mut a, Ev::Enter); // row 2: Mirror to harness
    assert_eq!(a.mode(), Mode::MirrorPick);
    assert_eq!(a.picker_targets(), vec!["gemini".to_string(), "pi".to_string()],
        "gemini is offered, the source's own format is never offered");
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
fn a_fresh_picker_starts_unticked_and_a_then_n_toggle_everything() {
    // The old picker pre-ticked every installed harness: one stray Enter mirrored a session
    // everywhere. A fresh session now starts with nothing ticked.
    let sb = sandbox("fresh");
    let mut a = app(&sb, fixture(&sb));
    a.set_installed_targets(vec!["pi".into(), "claude".into()]);
    open_picker(&mut a);
    assert!(!a.pick_is_chosen("pi") && !a.pick_is_chosen("claude"), "a fresh picker ticks nothing");
    press(&mut a, Ev::Char('a'));
    assert!(a.pick_is_chosen("pi") && a.pick_is_chosen("claude"), "'a' ticks every offered harness");
    press(&mut a, Ev::Char('n'));
    assert!(!a.pick_is_chosen("pi") && !a.pick_is_chosen("claude"), "'n' clears every tick");
}

#[test]
fn the_picker_starts_from_the_sessions_saved_choice() {
    let sb = sandbox("existing");
    let roots = fixture(&sb);
    let key = roots.omp.join("-tmp").join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"));
    let mut state = State::default();
    let mut src = sesync::state::Source::new("omp", key.to_str().unwrap());
    src.chosen_targets.insert("claude".to_string());
    state.sources.insert(key.to_string_lossy().to_string(), src);
    let mut a = App::new(roots, state, sb.join("locks"), sb.join("no-sesync_lock"));
    a.set_installed_targets(vec!["pi".into(), "claude".into()]);
    open_picker(&mut a);
    assert!(a.pick_is_chosen("claude"), "the saved choice comes back ticked");
    assert!(!a.pick_is_chosen("pi"), "an unchosen harness stays unticked");
    assert_eq!(a.pick_row_note("claude"), Some("chosen"));
    assert_eq!(a.pick_row_note("pi"), None);
}

#[test]
fn confirming_the_picker_saves_the_chosen_targets_to_state() {
    let sb = sandbox("confirm");
    let mut a = app(&sb, fixture(&sb));
    a.set_installed_targets(vec!["pi".into(), "claude".into()]);
    open_picker(&mut a);
    press(&mut a, Ev::Char('a')); // tick both
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
    open_picker(&mut a);
    press(&mut a, Ev::Char('a')); // tick pi
    press(&mut a, Ev::Char(' ')); // untick pi
    press(&mut a, Ev::Enter);
    let key = a.rows()[0].key.clone();
    assert!(a.state().sources.get(&key).map_or(true, |s| s.chosen_targets.is_empty()),
        "an empty choice means nothing is mirrored");
}

#[test]
fn show_transcript_opens_the_file_in_the_tui() {
    let sb = sandbox("transcript");
    let mut a = app(&sb, fixture(&sb));
    open_menu_row(&mut a, 3); // Show transcript
    assert_eq!(a.mode(), Mode::Transcript, "the menu opens the transcript view");
    let t = a.transcript_view().expect("the transcript is loaded");
    assert!(t.lines.iter().any(|l| l.contains("\"type\":\"session\"")), "the file's lines are there");
    assert_eq!(t.scroll, 0, "it starts at the top");
    press(&mut a, Ev::Esc);
    assert_eq!(a.mode(), Mode::Browse, "esc closes it");
}

#[test]
fn huge_transcripts_are_cut_to_their_tail() {
    let sb = sandbox("transcript-tail");
    let roots = fixture(&sb);
    let path = roots.omp.join("-tmp").join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"));
    let mut body = String::new();
    for i in 0..(sesync::app::TRANSCRIPT_MAX_LINES + 50) {
        body.push_str(&format!("line-{i}\n"));
    }
    std::fs::write(&path, body).unwrap();
    let mut a = App::new(roots, State::default(), sb.join("locks"), sb.join("no-sesync_lock"));
    open_menu_row(&mut a, 3);
    let t = a.transcript_view().expect("loaded");
    assert_eq!(t.lines.len(), sesync::app::TRANSCRIPT_MAX_LINES + 1,
        "one marker line plus the kept tail");
    assert!(t.lines[0].contains("earlier line(s) not shown"), "the head is marked as cut: {}", t.lines[0]);
    assert_eq!(t.lines[1], "line-50", "the kept tail starts where the cut happened");
    press(&mut a, Ev::Char('j'));
    assert_eq!(a.transcript_view().unwrap().scroll, 1, "j scrolls the kept tail");
}

#[test]
fn copy_path_from_the_menu_copies_the_session_file() {
    let sb = sandbox("copy-path");
    let mut a = app(&sb, fixture(&sb));
    open_menu_row(&mut a, 4); // Copy its path
    assert!(a.status().contains("copied"), "status: {}", a.status());
    assert!(a.status().contains("2026-10-01T10-00-00-000Z"), "the copied thing is the file path: {}", a.status());
}

/// Enter on the only session, then `row` downs, then Enter: run that menu row.
fn open_menu_row(a: &mut App, row: usize) {
    press(a, Ev::Enter);
    for _ in 0..row {
        press(a, Ev::Down);
    }
    press(a, Ev::Enter);
}
