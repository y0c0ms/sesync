//! The sessions list shows team state, last sync, next, mirrors and updated time as columns, and
//! hides them on narrow panes. Drawn to a test buffer; no terminal is used.
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
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("sesync-table-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("omp").join("sessions").join("-tmp")).unwrap();
    std::fs::create_dir_all(d.join("pi")).unwrap();
    std::fs::create_dir_all(d.join("claude")).unwrap();
    std::fs::write(d.join("omp").join("sessions").join("-tmp")
        .join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl")),
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{SID}\",\"cwd\":\"/tmp\",\"title\":\"Table session\"}}\n")).unwrap();
    App::new(
        Roots { omp: d.join("omp").join("sessions"), pi: d.join("pi"), claude: d.join("claude") },
        state,
        d.join("locks"),
        d.join("no-sesync_lock"),
    )
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
    for name in ["TEAM", "LAST SYNC", "NEXT", "MIRRORS", "UPDATED"] {
        assert!(head.contains(name), "header has {name}: {head}");
    }
}

#[test]
fn a_marked_never_pushed_session_reads_marked_and_never() {
    let mut a = app(State::default());
    a.handle(Ev::Char('m'), Instant::now()); // mark the selected session for the team
    let lines = screen(&a, 140, 20);
    let row = lines.iter().find(|l| l.contains("Table session")).expect("the session row");
    assert!(row.contains("marked"), "team state: {row}");
    assert!(row.contains("never"), "last sync: {row}");
    assert!(row.contains("next tick") || row.contains("in "), "next sync: {row}");
}

#[test]
fn a_pushed_session_reads_shared_with_its_last_sync_and_mirror_targets() {
    let mut state = State::default();
    state.mark(SID);
    state.record_team_push(SID, "h", 1.0e12); // pushed far in the future: reads "just now"
    let a = app(state);
    let lines = screen(&a, 140, 20);
    let row = lines.iter().find(|l| l.contains("Table session")).expect("the session row");
    assert!(row.contains("shared"), "team state: {row}");
    assert!(!row.contains("never"), "last sync is set: {row}");
}

#[test]
fn a_narrow_pane_hides_the_columns_and_keeps_the_title() {
    let a = app(State::default());
    let lines = screen(&a, 50, 12);
    assert!(lines.iter().all(|l| !l.contains("LAST SYNC")), "no columns on a narrow pane");
    assert!(lines.iter().any(|l| l.contains("Table session")), "the title is still shown");
}
