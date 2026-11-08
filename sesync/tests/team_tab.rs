//! The Team tab: one list of the conversations this machine shares and the team holds. Each row's
//! state comes from the completed turns the comparison read, and is drawn as one of five markers.

#[path = "support/team_list.rs"]
mod team_list;

use sesync::app::{App, Ev, Mode, RowState, TeamRow, View};
use sesync::manage::Roots;
use sesync::session_cols::is_unsafe_glyph;
use sesync::state::{Source, State};
use sesync::team_actions::AdminDone;
use sesync::team_cli::{DeviceRow, RosterView};
use std::fs;
use std::time::Instant;
use team_list::*;

const LOCAL_ID: &str = "d1d1d1d1-1111-2222-3333-444444444444";

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()
}

fn row(a: &App, conv: &str) -> TeamRow {
    a.team_rows().into_iter().find(|r| r.conversation == conv)
        .unwrap_or_else(|| panic!("no Team row for {conv}"))
}

#[test]
fn the_list_shows_each_state_with_its_own_marker() {
    let sb = sandbox("states");
    let a = five_states(&sb);

    let same = row(&a, SAME);
    assert_eq!((same.state, same.state.marker(), same.note.as_str()), (RowState::InStep, '=', "in step"));

    let ahead = row(&a, AHEAD);
    assert_eq!(ahead.state.marker(), '↑');
    assert!(ahead.note.starts_with("waiting to push"), "{}", ahead.note);

    let behind = row(&a, BEHIND);
    assert_eq!(behind.state.marker(), '↓');
    assert!(behind.note.starts_with("behind"), "{}", behind.note);
    assert!(behind.here, "this machine holds the behind copy");

    let diverged = row(&a, DIVERGED);
    assert_eq!(diverged.state.marker(), '≠');
    assert!(diverged.note.starts_with("diverged after 1 common turn"), "{}", diverged.note);

    let shared = row(&a, SHARED);
    assert_eq!(shared.state.marker(), '•');
    assert!(shared.shared && shared.here);
    assert!(shared.note.contains("no completed turn yet"), "{}", shared.note);

    let only = row(&a, TEAM_ONLY);
    assert_eq!((only.state, only.here), (RowState::Behind, false));
    assert!(only.note.starts_with("not here"), "{}", only.note);

    let mut markers: Vec<char> = a.team_rows().iter().map(|r| r.state.marker()).collect();
    markers.sort_unstable();
    markers.dedup();
    assert_eq!(markers.len(), 5, "five states, five markers: {markers:?}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_markers_are_outside_the_glyph_ranges_the_frame_scrubs() {
    // Invariant 21: a glyph the frame scrubs would vanish from the screen. Each marker must survive.
    for c in ['=', '↑', '↓', '≠', '•'] {
        assert!(!is_unsafe_glyph(c), "{c} (U+{:04X}) is scrubbed from the frame", c as u32);
    }
}

#[test]
fn latest_by_is_this_machine_the_teammates_fingerprint_or_the_admins_member_at_machine() {
    let sb = sandbox("latest-by");
    let mut a = five_states(&sb);
    assert_eq!(row(&a, AHEAD).from, "this machine", "a copy this device wrote");
    assert_eq!(row(&a, SAME).from, "fp-same", "a teammate's copy shows the list's device fingerprint");
    assert_eq!(row(&a, SHARED).from, "-", "no team copy, so no writer");

    // The admin sees member@machine when the roster maps the fingerprint.
    a.set_admin(true);
    a.finish_admin(Ok(AdminDone::Roster(RosterView {
        name: "team".into(),
        version: 1,
        devices: vec![DeviceRow { member: Some("alice".into()), machine: Some("laptop".into()), fingerprint: "fp-same".into(), admin: false }],
        pending: Vec::new(),
    })));
    assert_eq!(row(&a, SAME).from, "alice@laptop");
    assert_eq!(row(&a, BEHIND).from, "fp-behind", "a fingerprint the roster does not map stays a fingerprint");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_shared_session_is_listed_before_its_comparison_arrives_and_a_push_asks_for_a_pull() {
    let sb = sandbox("pending");
    let key = write_local(&sb, LOCAL_ID, "Local one", &[("q", "a")]);
    let mut st = State::default();
    st.share(LOCAL_ID, "team");
    let mut a = app_for(&sb, st);
    let pending = a.team_rows();
    assert_eq!(pending.len(), 1);
    assert_eq!((pending[0].state, pending[0].note.as_str()), (RowState::Shared, "checking…"));
    assert!(!a.take_refresh_request(), "nothing pushed yet, so no pull");

    // The daemon has pushed it: the list pulls without a ctrl+r.
    let mut disk = State::default();
    disk.share(LOCAL_ID, "team");
    let mut src = Source::new("omp", &key);
    src.last_seen_change = now_secs() - 600.0;
    disk.sources.insert(key.clone(), src);
    disk.record_team_push("team", LOCAL_ID, "hash", now_secs() - 60.0);
    a.absorb_daemon_state(&disk);
    assert!(a.take_refresh_request(), "a push the daemon made asks for a pull");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn opening_the_team_tab_pulls_only_when_the_list_is_stale() {
    let sb = sandbox("open-tab");
    let mut a = app_for(&sb, State::default());
    a.handle(Ev::Tab, Instant::now());
    assert_eq!(a.view(), View::Team);
    assert!(a.take_refresh_request(), "never pulled this run: opening the tab pulls");
    a.set_pull_result(vec![], 0);
    a.handle(Ev::Tab, Instant::now()); // machines
    a.handle(Ev::Tab, Instant::now()); // sessions
    a.handle(Ev::Tab, Instant::now()); // team again, pulled a moment ago
    assert!(!a.take_refresh_request(), "pulled a moment ago: no new pull");

    let mut unconfigured = App::new(Roots::under(&sb, sb.join("no-sesync-conv")), State::default(),
        sb.join("locks"), sb.join("no-sesync_lock"));
    unconfigured.handle(Ev::Tab, Instant::now());
    assert!(!unconfigured.take_refresh_request(), "no team, nothing to pull");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_team_footer_names_i_import_and_enter_resolve() {
    let sb = sandbox("footer");
    let mut a = app_for(&sb, State::default());
    a.handle(Ev::Tab, Instant::now());
    let footer = a.footer();
    assert!(footer.starts_with("i import · enter resolve"), "{footer}");
    assert!(!footer.contains("enter import"), "{footer}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_command_list_offers_resolve_only_for_a_behind_or_diverged_row() {
    let sb = sandbox("palette");
    let mut a = five_states(&sb);
    let title = "Resolve this conversation";
    let ok_for = |a: &App| a.palette_rows().into_iter().find(|r| r.title == title).map(|r| (r.ok, r.why));

    select_team_row(&mut a, SAME);
    assert_eq!(ok_for(&a), Some((false, "the selected conversation is not behind or diverged".to_string())));

    select_team_row(&mut a, DIVERGED);
    assert_eq!(ok_for(&a), Some((true, String::new())));

    select_team_row(&mut a, BEHIND);
    assert_eq!(ok_for(&a), Some((true, String::new())));

    select_team_row(&mut a, TEAM_ONLY);
    assert_eq!(ok_for(&a).map(|(ok, _)| ok), Some(false), "not held here, nothing to compare");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn enter_opens_the_divergence_dialog_on_a_diverged_row_and_the_command_list_elsewhere() {
    let sb = sandbox("enter");
    let mut a = five_states(&sb);
    select_team_row(&mut a, DIVERGED);
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Resolve);

    a.handle(Ev::Esc, Instant::now());
    select_team_row(&mut a, SAME);
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Palette, "a row in step has nothing to resolve: enter opens the list");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_list_is_drawn_with_each_state_and_its_columns() {
    let sb = sandbox("draw");
    let mut a = five_states(&sb);
    a.handle(Ev::Tab, Instant::now());
    let text = screen_text(&a, 120, 30);
    for marker in ['=', '↑', '↓', '≠', '•'] {
        assert!(text.contains(marker), "the screen shows {marker}:\n{text}");
    }
    assert!(text.contains("LATEST BY"), "the list has its column header:\n{text}");
    assert!(text.contains("this machine") && text.contains("fp-behind"), "LATEST BY is drawn:\n{text}");
    let _ = fs::remove_dir_all(&sb);
}
