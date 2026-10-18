//! The team setup dialogs: join a team and start a team (admin create). Same
//! floating-dialog style as the command list, with input fields; the caller performs the
//! network step handed over as a request. `Start a team` leads into the authenticator QR
//! when the server returns its one-time otpauth:// URI.

use std::sync::atomic::AtomicUsize;
static COUNTER: AtomicUsize = AtomicUsize::new(0);

use ratatui::{backend::TestBackend, Terminal};
use sesync::app::{App, Ev, Kind, Mode};
use sesync::ui::draw;
use std::time::Instant;

fn app() -> App {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("sesync-form-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("omp").join("sessions").join("-tmp")).unwrap();
    std::fs::create_dir_all(d.join("pi")).unwrap();
    std::fs::create_dir_all(d.join("claude")).unwrap();
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

fn type_text(a: &mut App, s: &str) {
    for c in s.chars() {
        a.handle(Ev::Char(c), Instant::now());
    }
}

fn open_join() -> App {
    let mut a = app();
    a.handle(Ev::Enter, Instant::now());
    type_text(&mut a, "join");
    a.handle(Ev::Enter, Instant::now()); // run the command
    assert_eq!(a.mode(), Mode::Form);
    a
}

fn open_register() -> App {
    let mut a = app();
    a.handle(Ev::Enter, Instant::now());
    type_text(&mut a, "start a team");
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Form);
    a
}

#[test]
fn the_join_dialog_has_the_join_fields() {
    let mut a = open_join();
    let text = screen(&a, 110, 26);
    for field in ["server url", "team", "member", "machine", "code"] {
        assert!(text.contains(field), "the dialog asks for '{field}':\n{text}");
    }
    assert!(text.contains("tab next field · enter submit · esc cancel"), "hint row:\n{text}");
    let _ = a;
}

#[test]
fn typing_edits_the_highlighted_field_and_tab_moves_it() {
    let mut a = open_join();
    type_text(&mut a, "http://10.10.20.6:30500");
    a.handle(Ev::Tab, Instant::now()); // -> team
    type_text(&mut a, "main");
    a.handle(Ev::Tab, Instant::now()); // -> member
    a.handle(Ev::Backspace, Instant::now()); // no-op on empty, harmless
    assert_eq!(a.form_value("server url"), "http://10.10.20.6:30500");
    assert_eq!(a.form_value("team"), "main");
    assert_eq!(a.form_value("member"), "");
}

#[test]
fn submitting_a_complete_dialog_hands_the_request_to_the_caller() {
    let mut a = open_join();
    for (i, v) in ["http://127.0.0.1:30500", "main", "laptop", "laptop-1", "123456"].iter().enumerate() {
        if i > 0 {
            a.handle(Ev::Tab, Instant::now());
        }
        type_text(&mut a, v);
    }
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Browse, "the dialog closes and the work begins");
    let req = a.take_setup_request().expect("the caller is asked to enroll");
    assert_eq!(req.kind, Kind::Enroll);
    assert_eq!(req.value_of("team"), "main");
    assert_eq!(req.value_of("code"), "123456");
    assert!(a.take_setup_request().is_none(), "the request is taken once");
    assert_eq!(a.form_value("team"), "", "the dialog is cleared");
}

#[test]
fn an_incomplete_dialog_refuses_to_submit() {
    let mut a = open_join();
    for (i, v) in ["http://127.0.0.1:30500", "main", "laptop", "laptop-1"].iter().enumerate() {
        // leave the code empty
        if i > 0 {
            a.handle(Ev::Tab, Instant::now());
        }
        type_text(&mut a, v);
    }
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Form, "stay until it is complete");
    assert!(a.take_setup_request().is_none(), "no half-filled request goes out");
    assert!(a.status().to_lowercase().contains("fill"), "the status says what is wrong: {}", a.status());
}

#[test]
fn the_code_must_be_six_digits() {
    let mut a = open_join();
    for (i, v) in ["http://s:1", "main", "laptop", "laptop-1", "12ab56"].iter().enumerate() {
        if i > 0 {
            a.handle(Ev::Tab, Instant::now());
        }
        type_text(&mut a, v);
    }
    a.handle(Ev::Enter, Instant::now());
    assert!(a.take_setup_request().is_none(), "not a 6-digit code");
    assert!(a.status().contains("6 digits"), "{}", a.status());
}

#[test]
fn esc_closes_the_dialog_without_a_request() {
    let mut a = open_join();
    type_text(&mut a, "http://x");
    a.handle(Ev::Esc, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert!(a.take_setup_request().is_none());
    assert_eq!(a.form_value("server url"), "", "the dialog is cleared");
}

#[test]
fn the_register_dialog_asks_for_the_admin_fields() {
    let a = open_register();
    let text = screen(&a, 110, 26);
    for field in ["team", "members", "admin token file"] {
        assert!(text.contains(field), "the dialog asks for '{field}':\n{text}");
    }
    assert!(text.contains("shown once and cannot be read back"), "the QR warning is shown:\n{text}");
}

#[test]
fn the_dialog_can_be_prefilled_by_the_caller() {
    let mut a = app();
    a.open_form(Kind::Register, &[("admin token file", "/home/yocoms/.sesync/admin-token")]);
    assert_eq!(a.form_value("admin token file"), "/home/yocoms/.sesync/admin-token");
    assert_eq!(a.form_value("team"), "");
}

#[test]
fn a_completed_register_hands_the_request_to_the_caller() {
    let mut a = open_register();
    for (i, v) in ["main", "laptop, pixel", "/tmp/admin-token"].iter().enumerate() {
        if i > 0 {
            a.handle(Ev::Tab, Instant::now());
        }
        type_text(&mut a, v);
    }
    a.handle(Ev::Enter, Instant::now());
    let req = a.take_setup_request().expect("the caller is asked to create the team");
    assert_eq!(req.kind, Kind::Register);
    assert_eq!(req.value_of("team"), "main");
    assert_eq!(req.value_of("members"), "laptop, pixel");
}

#[test]
fn the_qr_dialog_draws_the_authenticator_code() {
    let mut a = app();
    a.show_qr("main", "otpauth://totp/sesync:main?secret=JBSWY3DPEHPK3PXP&issuer=sesync");
    assert_eq!(a.mode(), Mode::Qr);
    let text = screen(&a, 110, 34);
    assert!(text.contains("Add team"), "{text}");
    assert!(text.contains("main"), "{text}");
    assert!(text.matches('█').count() + text.matches('▀').count() + text.matches('▄').count() > 50,
        "the dialog draws the QR: {text}");
    a.handle(Ev::Esc, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
}
