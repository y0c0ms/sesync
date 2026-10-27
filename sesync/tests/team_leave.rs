//! A device the team no longer serves, and the way out. A pull the server refuses reports it
//! instead of reading as an empty team; "Leave a team" removes a revoked membership or a join the
//! admin declined (its credential, its shares, the rows its pulls brought over) and forgets a team
//! managed here; the share picker can untie a team this machine has left. A real team server runs
//! in-process (support/team_server.rs); the app is driven through its own events.
#[path = "support/team_server.rs"]
mod support;

use sesync::app::{App, Ev, LeaveRequest, Mode};
use sesync::manage::Roots;
use sesync::state::State;
use sesync::team_cli::{AdminTeam, Command};
use sesync::team_join::{self, credential_path};
use sesync::team_watch::{teams_from_env, TeamCopy, TeamWatch};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;
use support::{sandbox, TeamServer, ADMIN_TOKEN};

/// A fixed server time, mid-step, so a code never straddles a 30-second boundary.
const T0: u64 = 1_000_020;

fn run_admin(srv: &TeamServer, admin: &Path, cmd: Command) -> Result<String, String> {
    sesync::team_cli::run(&cmd, &srv.base, ADMIN_TOKEN, admin)
}

fn app(root: &Path, state: State) -> App {
    App::new(Roots::under(root, root.join("no-sesync-conv")), state, root.join("locks"), root.join("hs.lock"))
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// An omp session the Sessions list shows, with a user turn.
fn omp_session(root: &Path, id: &str) -> PathBuf {
    let path = root.join("omp").join("sessions").join("-proj").join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!(
        "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"/home/u/proj\",\"timestamp\":\"2026-10-01T10:00:00.000Z\"}}\n\
         {{\"type\":\"message\",\"id\":\"m1\",\"message\":{{\"role\":\"user\",\"content\":\"{}\"}}}}\n", "hello ".repeat(100))).unwrap();
    path
}

#[test]
fn a_pull_the_server_refuses_is_reported_not_shown_as_an_empty_team() {
    let srv = TeamServer::start(T0);
    let (admin, cfg) = (sandbox("lv-pull-admin"), sandbox("lv-pull-member"));
    srv.create(&admin, "personal", &["alice"]);
    srv.admit(&admin, &cfg, "personal", "alice", "laptop");
    let floors = State::default().floors_all();
    let served = TeamWatch::pull_each(&teams_from_env(&cfg), &floors);
    assert_eq!(served.len(), 1);
    assert_eq!(served[0].out.error, None, "a listed device is served");

    run_admin(&srv, &admin, Command::Revoke { name: "personal".into(), member: "alice".into(), machine: "laptop".into() }).unwrap();
    let refused = TeamWatch::pull_each(&teams_from_env(&cfg), &floors);
    assert_eq!(refused.len(), 1, "the credential is still there, so the team is still pulled");
    assert!(refused[0].out.error.is_some(), "the refusal is reported: {:?}", refused[0].out);
    assert!(refused[0].out.shown.is_empty());
}

/// A verified copy as a pull returns it, titled so the Team list shows which team's copy it is.
fn copy(space: &str, conversation: &str, seq: u64, title: &str) -> TeamCopy {
    TeamCopy {
        space: space.into(),
        conversation: conversation.into(),
        seq,
        text: format!("{{\"type\":\"session\",\"id\":\"{conversation}\",\"title\":\"{title}\"}}\n").into_bytes(),
        mine: false,
    }
}

#[test]
fn leaving_a_revoked_team_removes_its_credential_its_shares_and_only_its_pulled_rows() {
    let srv = TeamServer::start(T0);
    let (admin, cfg, root) = (sandbox("lv-rv-admin"), sandbox("lv-rv-member"), sandbox("lv-rv-app"));
    srv.create(&admin, "personal", &["alice"]);
    srv.admit(&admin, &cfg, "personal", "alice", "laptop");
    run_admin(&srv, &admin, Command::Revoke { name: "personal".into(), member: "alice".into(), machine: "laptop".into() }).unwrap();

    let mut state = State::default();
    state.share("conv-1", "personal");
    state.share("conv-1", "other");
    state.share("conv-2", "personal");
    let mut a = app(&root, state);
    a.set_teams(vec!["personal".into(), "other".into()]);
    a.set_team_cards(team_join::team_cards(&cfg));
    let (both, only_left, only_other) = ("11111111-0000-4000-8000-000000000001", "22222222-0000-4000-8000-000000000002", "33333333-0000-4000-8000-000000000003");
    a.set_pull_result(vec![
        copy("personal", both, 5, "both teams, the newer copy"),
        copy("other", both, 3, "both teams, as the other team holds it"),
        copy("personal", only_left, 1, "only the team left"),
        copy("other", only_other, 2, "only the other team"),
    ], 0);
    assert_eq!(a.team_rows().len(), 3, "one row per conversation");
    let now = Instant::now();

    a.handle(Ev::OpenLeave, now);
    assert_eq!(a.mode(), Mode::LeavePick);
    assert_eq!(a.leave_rows().len(), 1, "one membership, no team managed here");
    a.handle(Ev::Enter, now);
    assert!(a.take_leave_request().is_none(), "the first Enter only arms the row");
    a.handle(Ev::Enter, now);
    let req = a.take_leave_request().expect("the second Enter leaves");
    assert_eq!(req, LeaveRequest { team: "personal".into(), membership: true, admin: false });
    assert_eq!(a.mode(), Mode::Browse);

    let outcome = sesync::team_actions::leave(&req, &cfg);
    assert!(outcome.is_ok(), "{outcome:?}");
    a.left_team(&req, outcome);
    assert!(!credential_path(&cfg, "personal").exists(), "the credential is gone");
    assert!(teams_from_env(&cfg).is_empty() && team_join::team_cards(&cfg).is_empty(), "nothing pulls or lists it");
    assert_eq!(a.state().shares_of("conv-1"), set(&["other"]), "only the team left is unshared");
    assert!(a.state().shares_of("conv-2").is_empty());
    assert!(a.take_user_dirty(), "the shares are saved");
    let rows = a.team_rows();
    assert_eq!(rows.len(), 2, "only the conversation no other team holds leaves the list: {rows:?}");
    assert!(rows.iter().any(|r| r.title == "both teams, as the other team holds it"),
        "a conversation the other team holds stays, as that team's copy: {rows:?}");
    assert!(rows.iter().any(|r| r.title == "only the other team"), "{rows:?}");
}

#[test]
fn a_join_the_admin_declined_can_be_left() {
    let srv = TeamServer::start(T0);
    let (admin, cfg) = (sandbox("lv-dec-admin"), sandbox("lv-dec-member"));
    srv.create(&admin, "personal", &["alice"]);
    srv.join(&cfg, "personal", "alice", "laptop");
    run_admin(&srv, &admin, Command::Decline { name: "personal".into(), member: "alice".into(), machine: "laptop".into() }).unwrap();
    // The server answers a declined key as it answers a pending one, so the join keeps waiting.
    assert_eq!(team_join::waiting_teams(&cfg), vec!["personal".to_string()]);

    let req = LeaveRequest { team: "personal".into(), membership: true, admin: false };
    sesync::team_actions::leave(&req, &cfg).unwrap();
    assert!(team_join::waiting_teams(&cfg).is_empty(), "nothing waits any more");
    assert!(team_join::team_cards(&cfg).is_empty(), "the Team tab no longer lists it");
    assert!(sesync::team_actions::leave(&req, &cfg).is_err(), "a second leave finds nothing to remove");
}

#[test]
fn stopping_managing_a_team_forgets_only_that_admin_record() {
    let srv = TeamServer::start(T0);
    let (admin, root) = (sandbox("lv-adm-admin"), sandbox("lv-adm-app"));
    srv.create(&admin, "personal", &["alice"]);
    srv.admit(&admin, &admin, "personal", "alice", "admin-box");
    for team in ["personal", "work"] {
        sesync::team_cli::remember_administered(&admin, &AdminTeam {
            team: team.into(), server: srv.base.clone(), token_file: "token".into(),
        }).unwrap();
    }
    let mut a = app(&root, State::default());
    a.set_team_cards(team_join::team_cards(&admin));
    a.set_admin_teams(sesync::team_cli::administered(&admin));
    let rows = a.leave_rows();
    let at = rows.iter().position(|(_, r)| r.admin && r.team == "personal").expect("a row to stop managing it");
    let now = Instant::now();
    a.handle(Ev::OpenLeave, now);
    for _ in 0..at {
        a.handle(Ev::Down, now);
    }
    a.handle(Ev::Enter, now);
    a.handle(Ev::Enter, now);
    let req = a.take_leave_request().expect("the second Enter leaves");
    assert_eq!(req, LeaveRequest { team: "personal".into(), membership: false, admin: true });
    sesync::team_actions::leave(&req, &admin).unwrap();
    let left: Vec<String> = sesync::team_cli::administered(&admin).into_iter().map(|t| t.team).collect();
    assert_eq!(left, vec!["work".to_string()], "the other team is still managed here");
    assert!(credential_path(&admin, "personal").exists(), "this machine's own membership stays");
}

#[test]
fn the_share_picker_can_untie_a_team_this_machine_has_left() {
    let root = sandbox("lv-pick");
    let id = "aaaaaaaa-1111-2222-3333-444444444444";
    let key = omp_session(&root, id).display().to_string();
    let conv = sesync::manage::session_id_of(&key);
    let mut state = State::default();
    state.share(&conv, "personal");
    state.share(&conv, "gone");
    let mut a = app(&root, state);
    a.set_teams(vec!["personal".into()]);
    let now = Instant::now();
    a.handle(Ev::Char('m'), now);
    assert_eq!(a.mode(), Mode::SharePick,
        "with a stale share to show, m opens the picker instead of toggling the one team");
    assert_eq!(a.share_picker_rows(), vec!["personal".to_string(), "gone".to_string()]);
    a.handle(Ev::Down, now);
    a.handle(Ev::Char(' '), now);
    a.handle(Ev::Enter, now);
    assert_eq!(a.state().shares_of(&conv), set(&["personal"]), "the stale team is untied, the live one kept");
}
