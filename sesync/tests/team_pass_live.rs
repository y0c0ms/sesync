//! The daemon's team pass, end to end against a real team server and Postgres. The writer marks a
//! session; `team_pass::run` (the same call the daemon loop makes) seals it and pushes it; the reader
//! pulls and must get the same bytes back. The database must hold no plaintext. A second pass with
//! the same content must not upload again. Runs only when TEAM_TEST_PG and TEAM_SERVER_BIN are set;
//! with TEAM_REQUIRE_PG a skip is a failure.
use sesync::device_keys::load_or_create;
use sesync::state::State;
use sesync::team_join;
use sesync::team_pass;
use sesync::team_watch::TeamWatch;
use sesync::team_cli::Command;
use sesync_team::pg::PgStore;
use sesync_team::service::Service;
use sesync_team::totp::code_at;
use std::process::{Child, Command as Proc, Stdio};
use std::time::Duration;

const MASTER: [u8; 32] = [7u8; 32];
const ADMIN_TOKEN: &str = "team-pass-live-admin-token-000000000000";

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn live_env() -> Option<(String, String)> {
    match (std::env::var("TEAM_TEST_PG"), std::env::var("TEAM_SERVER_BIN")) {
        (Ok(pg), Ok(bin)) => Some((pg, bin)),
        _ => {
            if std::env::var("TEAM_REQUIRE_PG").is_ok() {
                panic!("TEAM_REQUIRE_PG is set but this test needs TEAM_TEST_PG and TEAM_SERVER_BIN");
            }
            eprintln!("skipped: set TEAM_TEST_PG and TEAM_SERVER_BIN");
            None
        }
    }
}

fn wait_health(port: u16) {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(2)).build();
    for _ in 0..75 {
        if agent.get(&format!("http://127.0.0.1:{port}/healthz")).call().is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("the team server did not come up");
}

#[test]
fn the_daemon_team_pass_delivers_a_marked_session_to_the_reader() {
    let Some((pg, bin)) = live_env() else { return };
    let schema = "t_team_pass";
    let mut client = postgres::Client::connect(&pg, postgres::NoTls).unwrap();
    client
        .batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema};"))
        .unwrap();
    let scoped = format!("{pg}?options=-c%20search_path%3D{schema}");
    PgStore::connect(&scoped, MASTER).unwrap().migrate().unwrap();

    let dir = std::env::temp_dir().join(format!("sesync-team-pass-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("master.key"), MASTER).unwrap();
    std::fs::write(dir.join("admin.token"), ADMIN_TOKEN).unwrap();

    let port = 18200 + (std::process::id() % 400) as u16;
    let log = std::fs::File::create(dir.join("server.log")).unwrap();
    let child = Proc::new(&bin)
        .env("TEAM_SERVER_BIND", format!("127.0.0.1:{port}"))
        .env("TEAM_SERVER_DATABASE", &scoped)
        .env("TEAM_MASTER_KEY_FILE", dir.join("master.key"))
        .env("TEAM_ADMIN_TOKEN_FILE", dir.join("admin.token"))
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .expect("start team-server");
    let _server = Server(child);
    wait_health(port);
    let base = format!("http://127.0.0.1:{port}");

    // The admin creates the team, and the two members join through their own device keys.
    let create = Command::Create { name: "personal".into(), members: vec!["writer".into(), "reader".into()] };
    sesync::team_cli::run_detailed(&create, &base, ADMIN_TOKEN, &dir).expect("create the team");
    let svc = Service::new(PgStore::connect(&scoped, MASTER).unwrap());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let code = code_at(&svc.space_secret("personal").unwrap(), now);
    let writer_path = team_join::join(&base, "personal", "writer", "writer-box", &code, &dir.join("keys/writer")).unwrap();
    let reader_path = team_join::join(&base, "personal", "reader", "reader-box", &code, &dir.join("keys/reader")).unwrap();
    let writer_cred = team_join::load(&writer_path).unwrap();
    let reader_cred = team_join::load(&reader_path).unwrap();
    let publish = Command::Publish { name: "personal".into() };
    sesync::team_cli::run_detailed(&publish, &base, ADMIN_TOKEN, &dir).expect("publish the member list");

    // The writer's daemon pass: the session is marked and quiet, so it is sealed and pushed.
    std::env::set_var("SESYNC_TEAM_URL", &base);
    let journal = b"{\"type\":\"session\"}\nthe team pass session\n".to_vec();
    let mut state = State::default();
    state.share("ses_pass", "personal");
    let content = |id: &str| -> Result<Vec<u8>, String> {
        assert_eq!(id, "ses_pass", "only the marked session is offered");
        Ok(journal.clone())
    };
    let passes = team_pass::run(&dir.join("keys/writer"), &mut state, now as f64 + 3600.0,
        content, |_| Some(now as f64)).expect("the writer has a team configured");
    assert_eq!(passes.len(), 1, "the machine belongs to one team here");
    let pass = &passes[0];
    assert_eq!(pass.pushed, vec!["ses_pass".to_string()], "the shared, quiet session is pushed: {pass:?}");
    assert!(state.team_hash("team-a", "ses_pass").is_some(), "the push is recorded in state");
    let _ = writer_cred;

    // The database holds the session only as ciphertext.
    let row = client
        .query_one(
            &format!("SELECT position(convert_to($1, 'UTF8') IN sealed) FROM {schema}.records WHERE conversation = 'ses_pass'"),
            &[&"the team pass session".to_string()],
        )
        .unwrap();
    let position: i32 = row.get(0);
    assert_eq!(position, 0, "the server must hold no plaintext copy of a team-pass session");

    // The reader pulls the same bytes back.
    let watch = TeamWatch {
        base: base.clone(),
        token: reader_cred.token.clone(),
        machine: "reader-box".into(),
        space: "personal".into(),
        device: load_or_create(&dir.join("keys/reader")).unwrap(),
        trust: team_join::trust_of(&reader_cred),
    };
    let out = watch.pull(&std::collections::BTreeMap::new());
    assert_eq!(out.dropped, 0, "the writer is listed, so nothing is dropped");
    assert_eq!(out.shown.len(), 1, "the pushed session is shown");
    assert_eq!(out.shown[0].conversation, "ses_pass");
    assert_eq!(out.shown[0].text, journal, "the reader sees exactly the journal the writer's daemon sent");

    // A second pass with the same content uploads nothing new.
    let again = team_pass::run(&dir.join("keys/writer"), &mut state, now as f64 + 7200.0,
        |_| Ok(journal.clone()), |_| Some(now as f64)).unwrap();
    assert!(again.iter().all(|p| p.pushed.is_empty()), "an unchanged session is not re-uploaded: {again:?}");
    assert_eq!(again[0].skipped_unchanged, vec!["ses_pass".to_string()]);

    let _ = std::fs::remove_dir_all(&dir);
}
