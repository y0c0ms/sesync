//! The command palette, the Team view, and the screen that shows them. Enter and Ctrl+P open the
//! palette, which lists every action with the key that runs it and whether it can run here. Tab
//! switches between the local Sessions and the verified Team sessions.

use ratatui::{backend::TestBackend, Terminal};
use sesync::app::{App, Ev, Mode, View};
use sesync::manage::Roots;
use sesync::state::State;
use sesync::ui::draw;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const SID_A: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const SID_B: &str = "bbbbbbbb-1111-2222-3333-444444444444";
const TEAM_ID: &str = "11111111-2222-3333-4444-555555555555";
const TEAM_ID_2: &str = "22222222-2222-3333-4444-555555555555";

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-views-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn roots(sb: &Path) -> Roots {
    Roots {
        omp: sb.join("omp").join("sessions"),
        pi: sb.join("pi").join("sessions"),
        claude: sb.join("claude").join("projects"),
        gemini: sb.join("gemini").join("tmp"),
    }
}

fn write_session(sb: &Path, sid: &str, title: &str) {
    let dir = sb.join("omp").join("sessions").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let p = dir.join(format!("2026-10-01T10-00-00-000Z_{sid}.jsonl"));
    fs::write(&p, format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{sid}\",\"cwd\":\"/tmp\",\"title\":\"{title}\"}}\n")).unwrap();
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

fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines().find(|l| l.contains(needle)).unwrap_or("")
}

fn type_text(a: &mut App, s: &str) {
    for c in s.chars() {
        a.handle(Ev::Char(c), Instant::now());
    }
}

fn team_session(id: &str, title: &str) -> (String, Vec<u8>) {
    (id.to_string(), format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"cwd\":\"/tmp\",\"title\":\"{title}\"}}\n").into_bytes())
}

#[test]
fn enter_opens_the_palette_listing_every_action() {
    let sb = sandbox("enter");
    write_session(&sb, SID_A, "one");
    let mut a = app(&sb);
    a.handle(Ev::CtrlP, Instant::now());
    assert_eq!(a.mode(), Mode::Palette, "enter opens the command list");
    let text = screen(&a, 120, 30);
    for name in ["Rename", "Delete", "Import", "Share", "Stop sharing", "Refresh team", "Switch", "Quit"] {
        assert!(text.contains(name), "the palette lists {name}:\n{text}");
    }
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn ctrl_p_opens_the_palette_too() {
    let sb = sandbox("ctrl-p");
    write_session(&sb, SID_A, "one");
    let mut a = app(&sb);
    a.handle(Ev::CtrlP, Instant::now());
    assert_eq!(a.mode(), Mode::Palette);
    a.handle(Ev::CtrlP, Instant::now());
    assert_eq!(a.mode(), Mode::Browse, "ctrl+p again closes it");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn typing_filters_the_palette() {
    let sb = sandbox("filter");
    write_session(&sb, SID_A, "one");
    let mut a = app(&sb);
    a.handle(Ev::CtrlP, Instant::now());
    type_text(&mut a, "share");
    let titles = a.palette_titles();
    assert_eq!(titles.first().copied(), Some("Share with teams"), "{titles:?}");
    assert!(!titles.iter().any(|t| t.starts_with("Quit")), "non-matching actions are hidden: {titles:?}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn enter_in_the_palette_runs_the_highlighted_action() {
    let sb = sandbox("run");
    write_session(&sb, SID_A, "one");
    let mut a = app(&sb);
    a.set_teams(vec!["team-a".into(), "team-b".into()]);
    a.set_team_configured(true);
    a.handle(Ev::CtrlP, Instant::now());
    type_text(&mut a, "share");
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::SharePick, "the share picker opens");
    a.handle(Ev::Char('a'), Instant::now()); // tick every team
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert!(a.is_selected_marked(), "the action shared the selected session");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_disabled_action_does_not_run_and_says_why() {
    let sb = sandbox("disabled");
    write_session(&sb, SID_A, "one");
    let mut a = app(&sb);
    a.handle(Ev::CtrlP, Instant::now());
    type_text(&mut a, "stop");
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Palette, "the list stays open when the action cannot run");
    assert!(a.status().contains("not shared"), "the reason is shown: {}", a.status());
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn esc_closes_the_palette() {
    let sb = sandbox("esc");
    write_session(&sb, SID_A, "one");
    let mut a = app(&sb);
    a.handle(Ev::CtrlP, Instant::now());
    a.handle(Ev::Esc, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn tab_switches_between_sessions_and_team() {
    let sb = sandbox("tab");
    let mut a = app(&sb);
    assert_eq!(a.view(), View::Sessions);
    a.handle(Ev::Tab, Instant::now());
    assert_eq!(a.view(), View::Team);
    a.handle(Ev::Tab, Instant::now());
    assert_eq!(a.view(), View::Sessions);
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn i_in_the_sessions_view_points_to_the_team_tab() {
    let sb = sandbox("i-sessions");
    let mut a = app(&sb);
    a.set_team_sessions(vec![team_session(TEAM_ID, "Deploy runbook")]);
    a.handle(Ev::Char('i'), Instant::now());
    assert_eq!(a.mode(), Mode::Browse, "import is not offered from the Sessions view");
    assert!(a.status().contains("Team tab"), "{}", a.status());    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn in_the_team_view_i_imports_the_selected_team_row() {
    let sb = sandbox("i-team");
    let mut a = app(&sb);
    a.set_team_sessions(vec![team_session(TEAM_ID, "first"), team_session(TEAM_ID_2, "second")]);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Down, Instant::now());
    assert_eq!(a.team_index(), 1);
    a.handle(Ev::Char('i'), Instant::now());
    assert!(a.status().contains(TEAM_ID_2), "the question names the selected row: {}", a.status());
    a.handle(Ev::Char('y'), Instant::now());
    assert_eq!(a.imported(), vec![TEAM_ID_2.to_string()], "the selected row is the one imported");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn rename_and_delete_act_only_on_local_sessions() {
    let sb = sandbox("local-only");
    write_session(&sb, SID_A, "one");
    let mut a = app(&sb);
    a.set_team_sessions(vec![team_session(TEAM_ID, "x")]);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('r'), Instant::now());
    assert_eq!(a.mode(), Mode::Browse, "rename is not offered on a team row");
    assert!(a.status().contains("local sessions"), "{}", a.status());
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_team_cursor_is_separate_from_the_session_cursor() {
    let sb = sandbox("two-cursors");
    write_session(&sb, SID_A, "one");
    write_session(&sb, SID_B, "two");
    let mut a = app(&sb);
    a.set_team_sessions(vec![team_session(TEAM_ID, "x"), team_session(TEAM_ID_2, "y")]);
    a.handle(Ev::Down, Instant::now());
    assert_eq!(a.selected_index(), 1);
    a.handle(Ev::Tab, Instant::now());
    assert_eq!(a.team_index(), 0, "the team cursor starts at its own top");
    a.handle(Ev::Down, Instant::now());
    assert_eq!(a.team_index(), 1);
    a.handle(Ev::Tab, Instant::now());
    assert_eq!(a.selected_index(), 1, "the session cursor did not move");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_screen_shows_both_tabs_and_points_at_the_palette() {
    let sb = sandbox("tabs");
    write_session(&sb, SID_A, "one");
    let a = app(&sb);
    let text = screen(&a, 100, 10);
    let tabs = line_with(&text, "Sessions");
    assert!(tabs.contains("Team"), "the tab bar shows Team:\n{text}");
    assert!(text.contains("ctrl+p"), "the footer points at the palette:\n{text}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_team_tab_says_why_it_is_empty_when_no_server_is_configured() {
    let sb = sandbox("empty-team");
    let mut a = app(&sb);
    a.handle(Ev::Tab, Instant::now());
    let text = screen(&a, 120, 10);
    assert!(text.contains("No team server configured"), "{text}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_team_tab_lists_pulled_sessions_by_title() {
    let sb = sandbox("team-list");
    let mut a = app(&sb);
    a.set_team_sessions(vec![team_session(TEAM_ID, "Deploy runbook")]);
    a.handle(Ev::Tab, Instant::now());
    let text = screen(&a, 120, 10);
    assert!(text.contains("Deploy runbook"), "{text}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_palette_shows_a_reason_for_each_disabled_action() {
    let sb = sandbox("reasons");
    write_session(&sb, SID_A, "one");
    let mut a = app(&sb);
    a.handle(Ev::CtrlP, Instant::now());
    let text = screen(&a, 140, 30);
    assert!(text.contains("no team configured") || text.contains("not shared"), "{text}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn refresh_is_enabled_only_when_a_team_server_is_configured() {
    let sb = sandbox("refresh-enabled");
    let mut a = app(&sb);
    a.handle(Ev::CtrlP, Instant::now());
    type_text(&mut a, "refresh");
    let off = screen(&a, 140, 30);
    assert!(line_with(&off, "Refresh team").contains("off:"), "{off}");
    a.handle(Ev::Esc, Instant::now());
    a.set_team_configured(true);
    a.handle(Ev::Enter, Instant::now());
    type_text(&mut a, "refresh");
    let on = screen(&a, 140, 30);
    assert!(line_with(&on, "Refresh team").contains("enabled"), "{on}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_pull_result_shows_how_many_were_refused() {
    let sb = sandbox("pull-result");
    let mut a = app(&sb);
    a.set_pull_result(vec![], 2);
    let header = a.team_header(Instant::now());
    assert!(header.contains("2 refused"), "{header}");
    assert!(header.contains("pulled 0s ago"), "the header says when the pull happened: {header}");
    assert!(a.status().contains("2 refused"), "{}", a.status());
    assert!(a.team_rows().is_empty(), "nothing unverified is listed");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_team_header_says_when_nothing_was_pulled_yet() {
    let sb = sandbox("never-pulled");
    let a = app(&sb);
    assert!(a.team_header(Instant::now()).contains("not pulled yet"), "an untouched list says so");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn team_rows_show_a_short_id_next_to_the_title() {
    let sb = sandbox("short-id");
    let mut a = app(&sb);
    a.set_team_sessions(vec![team_session(TEAM_ID, "Deploy runbook")]);
    let rows = a.team_rows();
    assert_eq!(rows, vec!["Deploy runbook · 11111111…".to_string()],
        "the row carries the full title and just enough id to tell sessions apart");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn question_mark_opens_the_help_for_the_current_tab() {
    let sb = sandbox("help-sessions");
    write_session(&sb, SID_A, "one");
    let mut a = app(&sb);
    a.handle(Ev::Char('?'), Instant::now());
    assert_eq!(a.mode(), Mode::Help, "? opens the help overlay");
    let text = screen(&a, 140, 30);
    assert!(text.contains("Keys — sessions"), "{text}");
    assert!(text.contains("re-list"), "the local keys are listed:\n{text}");
    a.handle(Ev::Esc, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_help_overlay_names_the_team_keys_on_the_team_tab() {
    let sb = sandbox("help-team");
    let mut a = app(&sb);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Char('?'), Instant::now());
    assert_eq!(a.mode(), Mode::Help, "? opens the help overlay on the team tab too");
    let text = screen(&a, 140, 30);
    assert!(text.contains("Keys — team"), "{text}");
    assert!(text.contains("pull the team"), "the team keys are listed:\n{text}");
    let _ = fs::remove_dir_all(&sb);
}
