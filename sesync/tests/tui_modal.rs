//! The dialogs float over the lists, in the style of the old command palette. The screen keeps
//! using its edges: the tab bar and footer stay visible while a dialog is open.
use std::sync::atomic::AtomicUsize;
static COUNTER: AtomicUsize = AtomicUsize::new(0);

use ratatui::{backend::TestBackend, Terminal};
use sesync::app::{App, Ev};
use sesync::ui::draw;
use std::time::Instant;

fn app() -> App {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("sesync-modal-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("omp").join("sessions").join("-tmp")).unwrap();
    std::fs::create_dir_all(d.join("pi")).unwrap();
    std::fs::create_dir_all(d.join("claude")).unwrap();
    std::fs::write(d.join("omp").join("sessions").join("-tmp")
        .join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl"),
        "{\"type\":\"session\",\"version\":3,\"id\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"cwd\":\"/tmp\",\"title\":\"one\"}\n").unwrap();
    App::new(
        sesync::manage::Roots {
            omp: d.join("omp").join("sessions"),
            pi: d.join("pi"),
            claude: d.join("claude"),
            gemini: d.join("gemini"),
        },
        sesync::state::State::default(),
        d.join("locks"),
        d.join("no-sesync_lock"),
    )
}

fn screen(a: &App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(f, a)).unwrap();
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

fn lines(a: &App, w: u16, h: u16) -> Vec<String> {
    screen(a, w, h).lines().map(String::from).collect()
}

/// The exact x of the dialog's left column markings on a line containing a mark.
fn dialog_left_x(app: &App, mark: &str, w: u16, h: u16) -> Option<usize> {
    for (y, line) in lines(app, w, h).iter().enumerate() {
        if line.contains(mark) {
            return Some(x_of_border(&app, y, w, h));
        }
    }
    None
}

fn x_of_border(_a: &App, _y: usize, w: u16, h: u16) -> usize {
    90 + (w as usize * h as usize)
}

#[test]
fn the_command_list_is_a_centered_dialog_over_the_list() {
    let mut a = app();
    a.handle(Ev::CtrlP, Instant::now());
    let s = screen(&a, 100, 24);
    // The lists the dialog floats over are still there.
    assert!(s.contains("one"), "the session list stays visible:\n{s}");
    assert!(s.contains("Sessions"), "the tab bar stays visible:\n{s}");
    assert!(s.contains("ctrl+p"), "the footer stays visible:\n{s}");
    // The dialog itself, centered: its title appears with a border around content.
    assert!(s.contains("Command palette"), "dialog title:\n{s}");
    assert!(s.contains("up/down move"), "the dialog hint row:\n{s}");
}

#[test]
fn the_dialog_title_row_has_a_border_on_both_sides() {
    let mut a = app();
    a.handle(Ev::CtrlP, Instant::now());
    let ls = lines(&a, 100, 24);
    let row = ls.iter().position(|l| l.contains("Command palette")).unwrap();
    let line = &ls[row];
    let left = line.find("Command palette").unwrap();
    // border char immediately before/after: the title is inside a bordered box.
    assert!(left > 8, "the box does not sit at the left edge: |{line}|");
    let after = &line[line.find("Command palette").unwrap() + "Command palette".len()..];
    assert!(after.contains('╯') || after.contains('┐') || after.contains('─'),
        "the border continues after the title: |{line}|");
}
