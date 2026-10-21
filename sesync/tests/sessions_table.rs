//! The sessions list shows mirrors, sync state, team state and updated time as columns, drops them
//! least useful first on narrow panes, and draws every title the same in every terminal. Drawn to
//! a test buffer; no terminal is used.
use ratatui::{backend::TestBackend, Terminal};
use sesync::app::{App, Ev};
use sesync::manage::Roots;
use sesync::state::State;
use sesync::ui::draw;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

static COUNTER: AtomicUsize = AtomicUsize::new(0);
const SID: &str = "aaaaaaaa-1111-2222-3333-444444444444";

fn app(state: State) -> App {
    let d = dir();
    app_in(&d, state)
}

/// A sandbox dir with one omp session in it. The path is the key the app reads state by.
fn dir() -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("sesync-table-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("omp").join("sessions").join("-tmp")).unwrap();
    std::fs::create_dir_all(d.join("pi")).unwrap();
    std::fs::create_dir_all(d.join("claude")).unwrap();
    std::fs::write(session_path(&d),
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{SID}\",\"cwd\":\"/tmp\",\"title\":\"Table session\"}}\n")).unwrap();
    d
}

fn session_path(d: &std::path::Path) -> std::path::PathBuf {
    d.join("omp").join("sessions").join("-tmp")
        .join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"))
}

fn app_in(d: &std::path::Path, state: State) -> App {
    App::new(
        Roots::under(&d, d.join("no-sesync-conv")),
        state,
        d.join("locks"),
        d.join("no-sesync_lock"),
    )
}

/// The highlighted session row. The details pane shares the screen row of the column header, so a
/// plain title search would find the header instead.
fn is_session_row(l: &str) -> bool {
    l.starts_with("> ") && l.contains("Table session")
}

/// The foreground colour of the first cell whose text starts `mark`, on the session row.
fn mark_colour(a: &App, w: u16, h: u16, mark: &str) -> ratatui::style::Color {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(f, a)).unwrap();
    let buf = term.backend().buffer();
    for y in 0..h {
        let line: String = (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect();
        if is_session_row(&line) {
            if let Some(x) = line.find(mark) {
                return buf[(x as u16, y)].style().fg.unwrap();
            }
            panic!("no {mark} on the session row: |{line}|");
        }
    }
    panic!("no session row");
}

fn screen(a: &App, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(f, a)).unwrap();
    let buf = term.backend().buffer().clone();
    (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect()
}

#[test]
fn a_wide_pane_shows_the_column_headers() {
    let a = app(State::default());
    let lines = screen(&a, 140, 20);
    let head = lines.iter().find(|l| l.contains("TITLE")).expect("a header row");
    for name in ["TEAM", "SYNCED", "MIRRORS", "UPDATED"] {
        assert!(head.contains(name), "header has {name}: {head}");
    }
}

#[test]
fn a_shared_never_pushed_session_reads_marked() {
    let mut a = app(State::default());
    a.set_teams(vec!["team-a".into()]);
    a.handle(Ev::Char('m'), Instant::now()); // share the selected session with the team
    let lines = screen(&a, 140, 20);
    let row = lines.iter().find(|l| is_session_row(l)).expect("the session row");
    assert!(row.contains("marked"), "team state: {row}");
}

#[test]
fn a_pushed_session_reads_shared_with_its_last_sync_and_mirror_targets() {
    let mut state = State::default();
    state.share(SID, "team-a");
    state.record_team_push("team-a", SID, "h", 1.0e12); // pushed far in the future: reads "just now"
    let a = app(state);
    let lines = screen(&a, 140, 20);
    let row = lines.iter().find(|l| is_session_row(l)).expect("the session row");
    assert!(row.contains("shared"), "team state: {row}");
    assert!(!row.contains("never"), "last sync is set: {row}");
}

#[test]
fn a_narrow_pane_drops_columns_least_useful_first_and_keeps_the_title() {
    let a = app(State::default());
    let lines = screen(&a, 50, 12);
    let head = lines.iter().find(|l| l.contains("TITLE")).expect("a header row");
    assert!(head.contains("SYNCED"), "the sync state is the last column to go: {head}");
    for gone in ["TEAM", "UPDATED", "MIRRORS"] {
        assert!(!head.contains(gone), "{gone} goes before SYNCED: {head}");
    }
    assert!(lines.iter().any(|l| l.contains("Table session")), "the title is still shown");
    let lines = screen(&a, 34, 12);
    assert!(lines.iter().all(|l| !l.contains("SYNCED")), "no column squeezes the title below its minimum");
    assert!(lines.iter().any(|l| l.contains("Table session")), "the title is still shown");
}

#[test]
fn an_emoji_title_keeps_every_column_in_place_and_no_emoji_reaches_the_terminal() {
    // The title that garbled the screen: U+2696 + U+FE0F, two cells in some terminals and one in
    // others, so every later column moved and stale characters stayed behind.
    let plain = app(State::default());
    let d = dir();
    std::fs::write(session_path(&d), format!(
        "{{\"type\":\"session\",\"version\":3,\"id\":\"{SID}\",\"cwd\":\"/tmp\",\"title\":\"\u{2696}\u{FE0F} Table session\"}}\n")).unwrap();
    let mut emoji = app_in(&d, State::default());
    // The list pane (60 percent of 140 cells) reads exactly as for the plain title.
    let list_row = |a: &App| -> String {
        screen(a, 140, 20).into_iter().find(|l| is_session_row(l)).expect("the session row").chars().take(84).collect()
    };
    assert_eq!(list_row(&emoji), list_row(&plain));
    // The status line and the delete dialog quote the stored title as it is; the frame still
    // carries no glyph whose width terminals disagree on.
    let frame_is_safe = |a: &App, what: &str| {
        let lines = screen(a, 140, 20);
        assert!(lines.iter().any(|l| l.contains("Table session")), "{what} shows the title");
        for l in &lines {
            assert!(!l.chars().any(sesync::session_cols::is_unsafe_glyph), "{what}: |{l}|");
        }
    };
    let t0 = Instant::now();
    emoji.handle(Ev::CtrlD, t0);
    frame_is_safe(&emoji, "the status line");
    emoji.handle(Ev::CtrlD, t0);
    assert_eq!(emoji.mode(), sesync::app::Mode::DeleteConfirm);
    frame_is_safe(&emoji, "the delete dialog");
}

#[test]
fn a_pending_mirror_mark_is_drawn_in_the_warning_colour() {
    let d = dir();
    let path = session_path(&d);
    let mut state = State::default();
    let mut src = sesync::state::Source::new("omp", path.to_str().unwrap());
    src.chosen_targets.insert("claude".to_string());
    state.sources.insert(path.to_string_lossy().to_string(), src);
    let a = app_in(&d, state);
    assert_eq!(mark_colour(&a, 140, 20, "claude?"), ratatui::style::Color::Rgb(0xEC, 0xBE, 0x77), "pending is warning");
}

#[test]
fn a_failing_mirror_mark_is_drawn_in_the_error_colour() {
    let d = dir();
    let path = session_path(&d);
    let mut state = State::default();
    let mut src = sesync::state::Source::new("omp", path.to_str().unwrap());
    src.chosen_targets.insert("claude".to_string());
    src.failures = 1;
    state.sources.insert(path.to_string_lossy().to_string(), src);
    let a = app_in(&d, state);
    assert_eq!(mark_colour(&a, 140, 20, "claude!"), ratatui::style::Color::Rgb(0xE1, 0x57, 0x5A), "failing is error");
}
