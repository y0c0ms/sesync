//! The team members dialog and the invite dialog, driven through the app the way the TUI
//! drives them: keys in, a request out, the caller's answer back in. The caller (the TUI
//! binary) runs the network work on a thread; these tests play that caller.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use ratatui::{backend::TestBackend, Terminal};
use sesync::app::{App, Ev, Kind, Mode};
use sesync::team_actions::AdminDone;
use sesync::team_cli::{Command, DeviceRow, PendingRow, RosterView};
use sesync::ui::draw;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

const SERVER: &str = "http://team.test:30500";
const TOKEN_FILE: &str = "/tmp/admin-token";

fn app() -> App {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("sesync-members-{}-{}", std::process::id(), n));
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

/// An app on a machine that holds the admin key and has one team configured.
fn admin_app(team: &str) -> App {
    let mut a = app();
    a.set_teams(vec![team.to_string()]);
    a.set_team_configured(true);
    a.set_admin(true);
    a.set_team_defaults(SERVER, TOKEN_FILE);
    a
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

/// Run a command from the command list: Enter opens it, the words filter it, Enter runs it.
fn run_command(a: &mut App, words: &str) {
    a.handle(Ev::Enter, Instant::now());
    type_text(a, words);
    a.handle(Ev::Enter, Instant::now());
}

fn admin_device() -> DeviceRow {
    DeviceRow {
        member: Some("admin".into()),
        machine: Some("admin-box".into()),
        fingerprint: "AAAA-1111".into(),
        admin: true,
    }
}

fn bob_request() -> PendingRow {
    PendingRow {
        member: "bob".into(),
        machine: "bob-box".into(),
        fingerprint: "CCCC-3333".into(),
        age_secs: 90,
    }
}

fn roster(devices: Vec<DeviceRow>, pending: Vec<PendingRow>) -> RosterView {
    RosterView { name: "main".into(), version: 7, devices, pending }
}

/// Open the members dialog and answer its roster read with `view`.
fn open_members_with(view: RosterView) -> App {
    let mut a = admin_app("main");
    run_command(&mut a, "team members");
    assert_eq!(a.mode(), Mode::Members, "the dialog opens: {}", a.status());
    let job = a.take_admin_request().expect("the dialog asks the caller to read the list");
    match job.command {
        Command::Roster { name } => assert_eq!(name, "main"),
        other => panic!("expected a roster read, got {other:?}"),
    }
    assert_eq!(job.server, SERVER);
    assert_eq!(job.token_file, TOKEN_FILE);
    assert!(a.admin_busy(), "the read is running");
    a.finish_admin(Ok(AdminDone::Roster(view)));
    assert!(!a.admin_busy(), "the read is done");
    a
}

#[test]
fn the_members_dialog_lists_devices_and_waiting_requests() {
    let a = open_members_with(roster(vec![admin_device()], vec![bob_request()]));
    let text = screen(&a, 110, 30);
    assert!(text.contains("1 listed, 1 waiting"), "the header counts the rows:\n{text}");
    assert!(text.contains("admin key"), "the admin's own key is marked:\n{text}");
    assert!(text.contains("waiting") && text.contains("bob-box") && text.contains("CCCC-3333"),
        "the waiting request shows its machine and fingerprint:\n{text}");
    assert!(text.contains("asked") && text.contains("ago"), "the request shows its age:\n{text}");
}

#[test]
fn the_members_dialog_says_when_nobody_has_joined() {
    let a = open_members_with(roster(vec![], vec![]));
    let text = screen(&a, 110, 30);
    assert!(text.contains("nobody has joined this team yet"), "{text}");
}

#[test]
fn approving_a_request_takes_two_presses_on_the_same_row_and_sends_its_fingerprint() {
    let mut a = open_members_with(roster(vec![admin_device()], vec![bob_request()]));
    a.handle(Ev::Down, Instant::now()); // the waiting request is the second row

    a.handle(Ev::Char('a'), Instant::now());
    assert!(a.members().unwrap().prompt().is_some(), "the first press asks for confirmation");
    assert!(a.take_admin_request().is_none(), "nothing is signed on the first press");

    a.handle(Ev::Char('a'), Instant::now());
    let job = a.take_admin_request().expect("the second press sends the change");
    match job.command {
        Command::Approve { name, member, machine, fingerprint } => {
            assert_eq!(name, "main");
            assert_eq!(member, "bob");
            assert_eq!(machine, "bob-box");
            assert_eq!(fingerprint.as_deref(), Some("CCCC-3333"), "the approval checks the fingerprint");
        }
        other => panic!("expected an approval, got {other:?}"),
    }
    assert!(a.admin_busy(), "the change is running");
}

#[test]
fn a_different_key_or_a_new_row_cancels_a_pending_approval() {
    let mut a = open_members_with(roster(vec![admin_device()], vec![bob_request()]));
    a.handle(Ev::Down, Instant::now());
    a.handle(Ev::Char('a'), Instant::now());
    a.handle(Ev::Char('x'), Instant::now()); // not a change key: the pending change is dropped
    assert!(a.members().unwrap().prompt().is_none());
    a.handle(Ev::Char('a'), Instant::now()); // starts over, one press
    assert!(a.take_admin_request().is_none(), "one press after a cancel is not enough");

    a.handle(Ev::Up, Instant::now()); // moving to the admin row drops it as well
    a.handle(Ev::Down, Instant::now());
    assert!(a.members().unwrap().prompt().is_none(), "moving the cursor drops the pending change");
    assert!(a.take_admin_request().is_none());
}

#[test]
fn the_admin_key_cannot_be_revoked_and_a_request_cannot_be_revoked() {
    let mut a = open_members_with(roster(vec![admin_device()], vec![bob_request()]));
    a.handle(Ev::Char('r'), Instant::now()); // the admin's own key, on the first row
    assert!(a.status().contains("the admin's own key cannot be revoked"), "{}", a.status());
    a.handle(Ev::Down, Instant::now());
    a.handle(Ev::Char('r'), Instant::now()); // a waiting request is not on the list yet
    assert!(a.status().contains("it is not on the list yet"), "{}", a.status());
    assert!(a.take_admin_request().is_none(), "neither refusal sends anything");
}

#[test]
fn the_members_dialog_closes_on_escape_and_forgets_the_roster() {
    let mut a = open_members_with(roster(vec![admin_device()], vec![]));
    a.handle(Ev::Esc, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert!(a.members().is_none());
}

#[test]
fn the_members_and_invite_commands_say_why_they_are_not_available() {
    // A machine without the admin key.
    let mut a = app();
    a.set_teams(vec!["main".into()]);
    a.set_team_configured(true);
    run_command(&mut a, "team members");
    assert_eq!(a.mode(), Mode::Palette, "the list stays open when the command cannot run");
    assert!(a.status().contains("only the machine that started the team can do this"), "{}", a.status());
    assert!(a.take_admin_request().is_none());

    // Admin key, but no team started or joined here.
    let mut b = app();
    b.set_admin(true);
    run_command(&mut b, "invite a member");
    assert_eq!(b.mode(), Mode::Palette, "the list stays open when the command cannot run");
    assert!(b.status().contains("no team to manage here; start one first"), "{}", b.status());
}

#[test]
fn a_team_started_here_is_managed_without_joining_it_or_any_environment() {
    // The admin machine just started 'fresh': it joined nothing, and the TUI runs without
    // SESYNC_TEAM_URL or SESYNC_ADMIN_TOKEN_FILE. The team's own record supplies both.
    let mut a = app();
    a.set_admin(true);
    a.set_admin_teams(vec![sesync::team_cli::AdminTeam {
        team: "fresh".into(), server: "http://team.test:30777".into(), token_file: "/tmp/fresh-token".into(),
    }]);
    run_command(&mut a, "team members");
    assert_eq!(a.mode(), Mode::Members, "{}", a.status());
    let job = a.take_admin_request().expect("the dialog asks for the team's list");
    assert!(matches!(&job.command, Command::Roster { name } if name == "fresh"), "{:?}", job.command);
    assert_eq!(job.server, "http://team.test:30777");
    assert_eq!(job.token_file, "/tmp/fresh-token");
    // Once the list is in, the waiting note goes; the dialog says the rest.
    a.finish_admin(Ok(AdminDone::Roster(roster(vec![admin_device()], vec![bob_request()]))));
    assert_eq!(a.status(), "");
}

#[test]
fn the_invite_dialog_is_filled_from_the_team_defaults_and_sends_the_member() {
    let mut a = admin_app("main");
    run_command(&mut a, "invite a member");
    assert_eq!(a.mode(), Mode::Form);
    let text = screen(&a, 110, 26);
    assert!(text.contains("Invite a member"), "{text}");
    assert_eq!(a.form_value("server url"), SERVER);
    assert_eq!(a.form_value("team"), "main");
    assert_eq!(a.form_value("admin token file"), TOKEN_FILE);

    // Fields: server url, team, member, admin token file. Tab twice reaches member.
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Tab, Instant::now());
    type_text(&mut a, "pixel");
    a.handle(Ev::Enter, Instant::now());

    let req = a.take_setup_request().expect("the caller is asked to invite");
    assert_eq!(req.kind, Kind::Invite);
    assert_eq!(req.value_of("member"), "pixel");
    assert_eq!(req.value_of("team"), "main");
    assert_eq!(req.value_of("admin token file"), TOKEN_FILE);
}

#[test]
fn a_failed_invite_says_what_went_wrong() {
    let mut a = admin_app("main");
    a.finish_setup(sesync::team_actions::SetupDone {
        result: Err("not invited: the team server is not answering".into()),
        created: None,
        joined: false,
    });
    assert!(a.status().contains("not invited"), "{}", a.status());
}

/// A real-length authenticator URI: 32-character secret, team name, issuer.
fn real_uri() -> String {
    "otpauth://totp/sesync:personal?secret=JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP&issuer=sesync".to_string()
}

#[test]
fn the_authenticator_qr_fits_an_80x24_terminal_with_the_key_below_it() {
    let mut a = app();
    a.show_qr("personal", &real_uri());
    let text = screen(&a, 80, 24);
    assert!(text.contains("Add team 'personal' to your authenticator app"), "{text}");
    assert!(text.contains("JBSW Y3DP EHPK 3PXP JBSW Y3DP EHPK 3PXP"), "the key is typed out too, in fours:\n{text}");
    let blocks = text.matches('█').count() + text.matches('▀').count() + text.matches('▄').count();
    assert!(blocks > 100, "the QR is drawn in half-blocks ({blocks} cells):\n{text}");
}

#[test]
fn a_small_terminal_says_how_large_the_qr_needs_and_still_shows_the_key() {
    let mut a = app();
    a.show_qr("personal", &real_uri());
    let text = screen(&a, 60, 18);
    assert!(text.contains("the code needs"), "{text}");
    assert!(text.contains("JBSW Y3DP EHPK 3PXP JBSW Y3DP EHPK 3PXP"), "the key is still shown:\n{text}");
}
