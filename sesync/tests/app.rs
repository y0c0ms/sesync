//! TUI application, driven by key events against real files in a sandbox.
//! Covers browse, the two-press delete, and rename. The real stores and the real
//! lock are never used; the lock is the sesync_lock binary with a private directory.

use sesync::app::{App, Ev, Mode, View};
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
    let roots = Roots::under(&sb, sb.join("no-sesync-conv"));
    let dir = roots.locations.sessions_dir("omp").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(&roots.locations.sessions_dir("pi")).unwrap();
    fs::create_dir_all(&roots.locations.sessions_dir("claude")).unwrap();
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
    a.handle(Ev::CtrlD, t + Duration::from_secs(1)); // opens the confirmation
    assert!(path.exists(), "the confirmation dialog must not delete on its own");
    a.handle(Ev::CtrlD, t + Duration::from_secs(2)); // confirm
    assert!(!path.exists(), "a confirmed delete removes the source");
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

/// The TUI runs as a herdr popup (ctrl+alt+s); Esc must close it like any other popup.
#[test]
fn esc_quits_from_either_list() {
    for to_team in [false, true] {
        let sb = sandbox(if to_team { "esc-team" } else { "esc-sessions" });
        let (roots, _) = fixture(&sb);
        let mut a = app(&sb, roots);
        if to_team {
            a.handle(Ev::Tab, Instant::now());
            assert_eq!(a.view(), View::Team);
        }
        a.handle(Ev::Esc, Instant::now());
        assert!(a.should_quit(), "esc must quit (team list: {to_team})");
    }
}

#[test]
fn esc_clears_a_kept_filter_before_it_quits() {
    let sb = sandbox("esc-filter");
    let (roots, _, _) = two_sessions(&sb);
    let mut a = app(&sb, roots);
    a.handle(Ev::Char('/'), Instant::now());
    for c in "second".chars() {
        a.handle(Ev::Char(c), Instant::now());
    }
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.visible_rows().len(), 1, "the filter is kept after enter");
    a.handle(Ev::Esc, Instant::now());
    assert!(!a.should_quit(), "the first esc only clears the kept filter");
    assert_eq!(a.visible_rows().len(), 2);
    a.handle(Ev::Esc, Instant::now());
    assert!(a.should_quit());
}

#[test]
fn esc_on_an_armed_delete_cancels_it_and_keeps_the_tui_open() {
    let sb = sandbox("esc-armed");
    let (roots, path) = fixture(&sb);
    let mut a = app(&sb, roots);
    let t = Instant::now();
    a.handle(Ev::CtrlD, t);
    a.handle(Ev::Esc, t);
    assert!(!a.should_quit(), "backing out of a delete must not close the TUI");
    assert_eq!(a.status(), "delete cancelled");
    a.handle(Ev::CtrlD, t + Duration::from_secs(1));
    assert!(path.exists(), "the cancelled arm cannot fire: this ctrl+d only re-arms");
}

#[test]
fn esc_in_an_overlay_closes_only_the_overlay() {
    let sb = sandbox("esc-help");
    let (roots, _) = fixture(&sb);
    let mut a = app(&sb, roots);
    a.handle(Ev::Char('?'), Instant::now());
    assert_eq!(a.mode(), Mode::Help);
    a.handle(Ev::Esc, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert!(!a.should_quit());
}

/// A second session in the same folder, so the list has two rows.
fn two_sessions(sb: &PathBuf) -> (Roots, PathBuf, PathBuf) {
    let (roots, first) = fixture(sb);
    let id2 = "bbbbbbbb-1111-2222-3333-444444444444";
    let second = roots.locations.sessions_dir("omp").join("-tmp").join(format!("2026-10-02T10-00-00-000Z_{id2}.jsonl"));
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

const CLAUDE_SID: &str = "dddddddd-1111-2222-3333-444444444444";

fn claude_fixture(sb: &PathBuf) -> (Roots, PathBuf) {
    let roots = Roots::under(&sb, sb.join("no-sesync-conv"));
    let dir = roots.locations.sessions_dir("claude").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{CLAUDE_SID}.jsonl"));
    fs::write(&path, format!(
        "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"the original prompt\"}}}}\n{{\"type\":\"custom-title\",\"customTitle\":\"Old Claude Title\",\"sessionId\":\"{CLAUDE_SID}\"}}\n")).unwrap();
    (roots, path)
}

#[test]
fn ctrl_r_on_the_local_tab_reloads_a_title_renamed_in_claude_code() {
    let sb = sandbox("ctrl-r-local");
    let (roots, path) = claude_fixture(&sb);
    let mut a = app(&sb, roots);
    assert_eq!(a.rows()[0].title, "Old Claude Title");
    // Claude Code's /rename appends a custom-title record; the later one wins.
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str(&format!(
        "{{\"type\":\"custom-title\",\"customTitle\":\"Renamed In Claude\",\"sessionId\":\"{CLAUDE_SID}\"}}\n"));
    fs::write(&path, text).unwrap();
    a.handle(Ev::CtrlR, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert_eq!(a.rows()[0].title, "Renamed In Claude", "ctrl+r must re-read the file");
    assert!(!a.take_refresh_request(), "a local refresh must not ask for a team pull");
}

#[test]
fn a_mirror_the_daemon_wrote_shows_after_the_state_is_absorbed() {
    use sesync::state::{Mirror, Source, State as DState};
    let sb = sandbox("absorb-mirror");
    let (roots, src) = fixture(&sb);
    let mut a = app(&sb, roots);
    let key = src.display().to_string();
    let mirror_path = sb.join("pi").join("sessions").join("-tmp").join("m.jsonl");
    fs::create_dir_all(mirror_path.parent().unwrap()).unwrap();
    fs::write(&mirror_path, "x").unwrap();

    let mut disk = DState::default();
    let mut d = Source::new("omp", &key);
    d.mirrors.insert("pi".into(), Mirror {
        target: "pi".into(), path: mirror_path.display().to_string(), ..Default::default()
    });
    disk.sources.insert(key.clone(), d);
    disk.mirror_keys.insert(mirror_path.display().to_string());
    a.absorb_daemon_state(&disk);
    a.refresh_local();

    let row = a.rows().iter().find(|g| g.key == key).unwrap();
    assert!(row.mirrors.iter().any(|(t, p)| t == "pi" && p == &mirror_path.display().to_string()),
        "the daemon-written mirror must be visible in the row: {:?}", row.mirrors);
}

#[test]
fn news_fades_but_failures_stay_at_the_status_line() {
    let sb = sandbox("fade");
    let (roots, _) = fixture(&sb);
    let mut a = app(&sb, roots);
    let now = Instant::now();
    a.set_status("some news");
    assert!(a.status_line(now).is_some(), "fresh news is shown");
    let later = now + sesync::app::STATUS_TTL + Duration::from_secs(1);
    assert_eq!(a.status_line(later), None, "informational messages fade on their own");
    a.set_error("something refused");
    assert_eq!(a.status_line(later), Some(("something refused", true)),
        "a failure is drawn and does not fade; the view colours it");
    let fresh = a.status_line(now);
    assert_eq!(fresh, Some(("something refused", true)), "a replaced message starts fresh");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_last_tab_and_theme_come_back_on_the_next_run() {
    let sb = sandbox("prefs");
    let (roots, _) = fixture(&sb);
    let config = sb.join("config");
    let mut a = app(&sb, roots);
    assert!(!a.take_prefs_dirty(), "a fresh app changed no taste");
    a.handle(Ev::Tab, Instant::now());
    assert!(a.take_prefs_dirty(), "switching the tab is a taste change");
    assert!(sesync::app::prefs::save(&config, "textual (dark)", a.view()).is_ok(),
        "the caller persists the taste");
    a.handle(Ev::Tab, Instant::now()); // back to sessions
    assert_eq!(a.view(), View::Sessions);
    a.restore_prefs(None, Some(View::Team));
    assert_eq!(a.view(), View::Team, "restore brings the last tab back");
    // Load with the same content: theme and tab come back through the file too.
    let p = sesync::app::prefs::load(&config);
    assert_eq!(p.theme.as_deref(), Some("textual (dark)"));
    assert_eq!(p.view, Some(View::Team));
    // A file with junk or missing keys yields defaults, never a failure.
    std::fs::write(config.join("tui.json"), b"not json at all").unwrap();
    assert_eq!(sesync::app::prefs::load(&config), sesync::app::prefs::Prefs::default(),
        "a broken preferences file is ignored");
    // An unknown theme name is ignored on restore, leaving the default in place.
    a.restore_prefs(Some("no such scheme".to_string()), None);
    assert_eq!(a.theme_name(), sesync::theme::DEFAULT, "an unknown theme never applies");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_second_ctrl_d_asks_and_any_other_key_cancels() {
    let sb = sandbox("confirm-delete");
    let (roots, path) = fixture(&sb);
    let mut a = app(&sb, roots);
    let now = Instant::now();
    a.handle(Ev::CtrlD, now); // arm
    a.handle(Ev::CtrlD, now); // the dialog
    assert_eq!(a.mode(), Mode::DeleteConfirm, "the second press asks, it does not delete");
    let (title, src) = a.delete_preview().expect("the dialog names the session");
    assert_eq!(title, "Old");
    assert!(src.ends_with(".jsonl"), "the source path is shown: {src}");
    a.handle(Ev::Char('x'), now); // anything else cancels
    assert_eq!(a.mode(), Mode::Browse);
    assert!(path.exists(), "a cancelled delete removes nothing");
    a.handle(Ev::CtrlD, now); // arm again
    a.handle(Ev::CtrlD, now); // dialog
    a.handle(Ev::Enter, now); // confirm with enter
    assert!(!path.exists(), "enter confirms the delete");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_pull_is_looked_at_one_at_a_time() {
    let sb = sandbox("pulling");
    let (roots, _) = fixture(&sb);
    let mut a = app(&sb, roots);
    let now = Instant::now();
    assert!(!a.pulling(), "no pull before the first one");
    assert!(a.begin_pull(), "the first pull starts");
    assert!(a.pulling(), "it is in flight");
    assert!(a.status().contains("pulling the team"), "{}", a.status());
    assert!(!a.begin_pull(), "a second pull waits for the one running");
    assert!(a.status().contains("already running"), "{}", a.status());
    a.set_pull_result(vec![], 0);
    assert!(!a.pulling(), "the finished pull is not in flight");
    assert!(a.status().contains("team pull"), "{}", a.status());
    assert!(a.begin_pull(), "another pull can start after it finished");
    a.pull_failed();
    assert!(!a.pulling(), "a failed pull is over too");
    assert!(a.status().contains("did not finish"), "{}", a.status());
    assert!(a.team_header(now).contains("pulled "), "the earlier successful pull still shows");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_tui_adopts_daemon_deletions_and_keeps_only_its_own_choices() {
    let sb = sandbox("prune");
    let (roots, _) = fixture(&sb);
    // A source only in this app's memory, adopted from an older disk state.
    let mut adopted = sesync::state::Source::new("opencode", "ses_resurrected");
    adopted.title = "gone elsewhere".into();
    let key_adopted = "ses_resurrected".to_string();
    // A choice the TUI itself recorded for a session the daemon has not scanned yet.
    let mut mine = sesync::state::Source::new("omp", "/new/session.jsonl");
    mine.chosen_targets.insert("pi".to_string());
    mine.tui_created = true;
    let key_mine = "/new/session.jsonl".to_string();
    let mut st = State::default();
    st.sources.insert(key_adopted.clone(), adopted);
    st.sources.insert(key_mine.clone(), mine);
    let mut a = App::new(roots, st, sb.join("locks"), sb.join("no-sesync_lock"));
    // The daemon's disk no longer knows either session.
    a.absorb_daemon_state(&State::default());
    assert!(a.state().sources.get(&key_adopted).is_none(),
        "a stale adopted copy cannot resurrect a session the daemon removed");
    assert!(a.state().sources.contains_key(&key_mine),
        "a choice the TUI itself recorded is not pruned");
    let _ = fs::remove_dir_all(&sb);
}
