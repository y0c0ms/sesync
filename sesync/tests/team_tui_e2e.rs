//! The TUI's team actions end to end, against the in-process team server: the dialogs' answers
//! go through `team_actions::setup` and `team_actions::admin` exactly as the TUI calls them.
//! Start a team, invite a member, the member asks to join, the members dialog shows the request,
//! a wrong fingerprint is refused, the right one approves, and the member picks up the approval.

#[path = "support/team_server.rs"]
mod support;

use std::path::Path;

use sesync::app::{Kind, SetupRequest};
use sesync::team_actions::{self, AdminDone, AdminJob};
use sesync::team_cli::Command;
use sesync::team_join::{self, Approval};
use support::{sandbox, TeamServer, ADMIN_TOKEN};

const T0: u64 = 1_800_000_000;

fn request(kind: Kind, values: &[(&str, &str)]) -> SetupRequest {
    SetupRequest {
        kind,
        values: values.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    }
}

/// The admin token file the dialogs read, written the way an operator would keep it: owner-only,
/// since the CLI and the dialogs refuse a token file others can read.
fn token_file(dir: &Path) -> String {
    let path = dir.join("admin-token");
    std::fs::write(&path, ADMIN_TOKEN).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path.display().to_string()
}

fn job(srv: &TeamServer, token: &str, command: Command) -> AdminJob {
    AdminJob { command, server: srv.base.clone(), token_file: token.to_string() }
}

fn roster(srv: &TeamServer, token: &str, admin: &Path, name: &str) -> sesync::team_cli::RosterView {
    match team_actions::admin(&job(srv, token, Command::Roster { name: name.into() }), admin) {
        Ok(AdminDone::Roster(view)) => view,
        other => panic!("expected the roster, got {other:?}"),
    }
}

#[test]
fn a_member_invited_from_the_dialogs_is_approved_from_the_members_dialog() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("tui-admin");
    let bob = sandbox("tui-bob");
    let token = token_file(&admin);
    let base = srv.base.clone();

    // Start a team from the register dialog.
    let done = team_actions::setup(&request(Kind::Register, &[
        ("team", "personal"),
        ("members", "alice"),
        ("admin token file", token.as_str()),
        ("server url", base.as_str()),
    ]), &admin);
    assert!(done.result.is_ok(), "{:?}", done.result);
    let (team, uri) = done.created.expect("the QR dialog gets the one-time URI");
    assert_eq!(team, "personal");
    assert!(uri.starts_with("otpauth://"), "{uri}");
    // The admin dialogs find the team later (another run, no environment) with what was typed.
    assert_eq!(sesync::team_cli::administered(&admin), vec![sesync::team_cli::AdminTeam {
        team: "personal".into(), server: base.clone(), token_file: token.clone(),
    }]);

    // Invite bob from the invite dialog.
    let done = team_actions::setup(&request(Kind::Invite, &[
        ("server url", base.as_str()),
        ("team", "personal"),
        ("member", "bob"),
        ("admin token file", token.as_str()),
    ]), &admin);
    assert!(done.result.is_ok(), "{:?}", done.result);

    // Bob's machine joins with the code from his authenticator.
    let code = srv.code("personal");
    let done = team_actions::setup(&request(Kind::Enroll, &[
        ("server url", base.as_str()),
        ("team", "personal"),
        ("member", "bob"),
        ("machine", "bob-box"),
        ("code", code.as_str()),
    ]), &bob);
    let msg = done.result.expect("the join goes through");
    assert!(msg.contains("waiting for the admin to approve"), "{msg}");
    assert!(done.joined, "the machine holds a pending credential");

    // The members dialog reads the list: bob's request is waiting, nothing is listed for him yet.
    let view = roster(&srv, &token, &admin, "personal");
    assert_eq!(view.pending.len(), 1, "{view:?}");
    let asked = view.pending[0].clone();
    assert_eq!((asked.member.as_str(), asked.machine.as_str()), ("bob", "bob-box"), "{asked:?}");
    assert!(view.devices.iter().any(|d| d.admin), "the admin's own key is listed: {view:?}");

    // A fingerprint that does not match the request is refused and nothing is signed.
    let before = view.version;
    let wrong = job(&srv, &token, Command::Approve {
        name: "personal".into(),
        member: "bob".into(),
        machine: "bob-box".into(),
        fingerprint: Some("0000-0000-0000-0000".into()),
    });
    let e = team_actions::admin(&wrong, &admin).unwrap_err();
    assert!(e.contains("nothing was signed"), "{e}");
    assert_eq!(roster(&srv, &token, &admin, "personal").version, before, "no list was published");

    // The fingerprint the dialog showed approves it.
    let right = job(&srv, &token, Command::Approve {
        name: "personal".into(),
        member: "bob".into(),
        machine: "bob-box".into(),
        fingerprint: Some(asked.fingerprint.clone()),
    });
    let ok = team_actions::admin(&right, &admin).expect("the approval is signed");
    assert!(matches!(ok, AdminDone::Message(_)), "{ok:?}");

    let after = roster(&srv, &token, &admin, "personal");
    assert!(after.version > before, "a new list was published");
    assert!(after.pending.is_empty(), "the request left the waiting rows: {after:?}");
    assert!(after.devices.iter().any(|d| d.member.as_deref() == Some("bob") && d.machine.as_deref() == Some("bob-box")),
        "bob's device is listed: {after:?}");

    // Bob's machine picks up the signed approval, as the daemon does.
    let picked = team_join::check_approval(None, &bob, "personal").expect("the approval is readable");
    assert!(matches!(picked, Some(Approval::Approved { .. })), "bob is approved: {picked:?}");
}

#[test]
fn the_members_dialog_reports_a_missing_admin_token_file() {
    let srv = TeamServer::start(T0);
    let admin = sandbox("tui-missing-token");
    let gone = admin.join("no-such-token").display().to_string();
    let e = team_actions::admin(&job(&srv, &gone, Command::Roster { name: "personal".into() }), &admin).unwrap_err();
    assert!(e.contains("cannot read the admin token file"), "{e}");
}
