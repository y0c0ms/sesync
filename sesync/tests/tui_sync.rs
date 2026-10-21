//! Per-target sync records: they survive a save, the sessions list marks orphans, the details pane
//! explains a failed write, and a click maps to the row it lands on. Drawn to a test buffer.
use ratatui::{backend::TestBackend, layout::Rect, Terminal};
use sesync::app::App;
use sesync::manage::Roots;
use sesync::state::{Source, State};
use sesync::ui::{draw, session_row_at};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);
const SID: &str = "bbbbbbbb-1111-2222-3333-444444444444";

fn dir() -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("sesync-tuisync-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("omp").join("sessions").join("-tmp")).unwrap();
    std::fs::create_dir_all(d.join("pi")).unwrap();
    std::fs::create_dir_all(d.join("claude")).unwrap();
    std::fs::write(session_path(&d),
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{SID}\",\"cwd\":\"/tmp\",\"title\":\"Sync session\"}}\n")).unwrap();
    d
}

fn session_path(d: &std::path::Path) -> std::path::PathBuf {
    d.join("omp").join("sessions").join("-tmp").join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"))
}

fn app_in(d: &std::path::Path, state: State) -> App {
    App::new(
        Roots::under(&d, d.join("no-sesync-conv")),
        state,
        d.join("locks"),
        d.join("no-sesync_lock"),
    )
}

fn screen(a: &App, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(f, a)).unwrap();
    let buf = term.backend().buffer();
    (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect()
}

#[test]
fn sync_records_survive_a_save_and_a_source_without_them_loads_empty() {
    let d = dir();
    let file = d.join("state.json");
    let key = session_path(&d).display().to_string();
    let mut state = State::default();
    let mut src = Source::new("omp", &key);
    src.record_sync("pi", Ok(()), 100.0);
    src.record_sync("claude", Err("disk full"), 200.0);
    state.sources.insert(key.clone(), src);
    state.save(&file).unwrap();

    let back = State::load(&file).unwrap();
    let s = &back.sources[&key];
    assert_eq!(s.sync["pi"].ok_at, 100.0);
    assert_eq!(s.sync["pi"].problem, "");
    assert_eq!(s.sync["claude"].problem, "disk full");
    assert_eq!(s.sync["claude"].problem_at, 200.0);

    let mut bare = State::default();
    bare.sources.insert("/x.jsonl".into(), Source::new("omp", "/x.jsonl"));
    bare.save(&file).unwrap();
    assert!(State::load(&file).unwrap().sources["/x.jsonl"].sync.is_empty());
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_session_whose_file_is_gone_is_listed_as_missing() {
    let d = dir();
    let mut state = State::default();
    let mut gone = Source::new("pi", "/nowhere/gone.jsonl");
    gone.title = "Gone session".into();
    state.sources.insert("/nowhere/gone.jsonl".into(), gone);
    let a = app_in(&d, state);
    let lines = screen(&a, 140, 20);
    assert!(lines.iter().any(|l| l.contains("Gone session") && l.contains("(missing)")),
        "orphan row says missing: {lines:#?}");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn details_pane_explains_a_failed_write_to_a_target() {
    let d = dir();
    let key = session_path(&d).display().to_string();
    let mut state = State::default();
    let mut src = Source::new("omp", &key);
    // The user chose claude and the daemon's write there failed.
    src.chosen_targets.insert("claude".into());
    src.record_sync("claude", Err("disk full"), 200.0);
    state.sources.insert(key, src);
    let a = app_in(&d, state);
    let lines = screen(&a, 140, 24);
    assert!(lines.iter().any(|l| l.contains("failed") && l.contains("disk full")),
        "details name the failed target's problem: {lines:#?}");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_click_on_the_session_row_selects_it_and_a_click_on_the_header_does_nothing() {
    let d = dir();
    let a = app_in(&d, State::default());
    let lines = screen(&a, 140, 20);
    // The highlighted row starts with "> "; the details pane shares the header's screen row.
    let row = lines.iter().position(|l| l.starts_with("> ") && l.contains("Sync session")).expect("session row") as u16;
    let head = lines.iter().position(|l| l.contains("TITLE")).expect("header row") as u16;
    let screen_rect = Rect::new(0, 0, 140, 20);
    assert_eq!(session_row_at(&a, screen_rect, 3, row), Some(0));
    assert_eq!(session_row_at(&a, screen_rect, 3, head), None, "the header is not a session");
    std::fs::remove_dir_all(&d).unwrap();
}
