//! The colour scheme picker in the TUI. It is a floating dialog like the old one: up/down
//! previews live, Enter applies, Esc keeps the theme that was active when the picker opened,
//! and "(active)" marks the scheme in use. Ctrl+T opens it, and so do the commands in the list.

use std::sync::atomic::AtomicUsize;
static COUNTER: AtomicUsize = AtomicUsize::new(0);

use ratatui::{backend::TestBackend, Terminal};
use sesync::app::{App, Ev, Mode};
use sesync::ui::draw;
use std::time::Instant;

fn app() -> App {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("sesync-theme-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("omp").join("sessions").join("-tmp")).unwrap();
    std::fs::create_dir_all(d.join("pi")).unwrap();
    std::fs::create_dir_all(d.join("claude")).unwrap();
    App::new(
        sesync::manage::Roots::under(&d, d.join("no-sesync-conv")),
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

#[test]
fn ctrl_t_opens_the_colour_scheme_picker() {
    let mut a = app();
    a.handle(Ev::Theme, Instant::now());
    assert_eq!(a.mode(), Mode::Theme);
    let text = screen(&a, 100, 24);
    assert!(text.contains("Colour scheme"), "the dialog has a title:\n{text}");
    assert!(text.contains("(active)"), "the scheme in use is marked:\n{text}");
}

#[test]
fn browsing_previews_and_esc_restores_the_original() {
    let mut a = app();
    let original = a.theme_name().to_string();
    a.handle(Ev::Theme, Instant::now());
    a.handle(Ev::Down, Instant::now());
    assert_ne!(a.theme_name(), original, "browsing previews the scheme at the cursor");
    a.handle(Ev::Esc, Instant::now());
    assert_eq!(a.theme_name(), original, "esc keeps the original scheme");
    assert_eq!(a.mode(), Mode::Browse);
}

#[test]
fn enter_applies_the_highlighted_scheme() {
    let mut a = app();
    a.handle(Ev::Theme, Instant::now());
    a.handle(Ev::Down, Instant::now());
    let highlighted = a.theme_highlighted().0.to_string();
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert_eq!(a.theme_name(), highlighted);
    assert!(a.status().contains(&highlighted), "the status names the applied scheme: {}", a.status());
}

#[test]
fn the_picker_is_one_of_the_commands_in_the_list() {
    let mut a = app();
    a.handle(Ev::Enter, Instant::now()); // open the command list
    for c in "colour".chars() {
        a.handle(Ev::Char(c), Instant::now());
    }
    assert_eq!(a.palette_titles()[0], "Colour scheme");
    a.handle(Ev::Enter, Instant::now()); // run it
    assert_eq!(a.mode(), Mode::Theme, "the command opens the picker");
}

#[test]
fn every_theme_paints_its_own_page_background() {
    let mut a = app();
    a.handle(Ev::Theme, Instant::now());
    // Walk the whole list with the cursor; at every stop the painted page background must be
    // the previewed theme's bg, everywhere no widget drew its own colour.
    let names = sesync::theme::names_sorted();
    assert!(names.len() >= 10, "the picker lists the full set, got {}: {:?}", names.len(), names);
    for _ in names.iter() {
        a.handle(Ev::Down, Instant::now());
        let th = a.theme();
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|f| draw(f, &a)).unwrap();
        let buf = term.backend().buffer().clone();
        // Mid-list, right edge: no widget writes here.
        assert_eq!(buf[(95, 10)].clone().bg, th.bg, "page background follows the previewed theme '{}'", th.name);
    }
    a.handle(Ev::Esc, Instant::now());
}

#[test]
fn a_dialog_restamps_the_rows_it_covers_with_the_page_background() {
    let mut a = app();
    a.handle(Ev::Theme, Instant::now()); // a dialog is open now
    let th = a.theme();
    let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
    term.draw(|f| draw(f, &a)).unwrap();
    let buf = term.backend().buffer().clone();
    // Inside the dialog box (centered, 74 wide in a 100-wide pane), right of any text: the
    // page background, not a leak of what the list drew under it.
    assert_eq!(buf[(84, 10)].clone().bg, th.bg, "no list text or colour shows through a dialog");
    let _ = screen(&a, 100, 20);
}
