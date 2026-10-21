//! The real `sesync --watch` loop against a real team server and Postgres. The binary must scan a
//! marked session, seal it, push it, then pull it back on a later tick and raise its sequence floor.
//! This is the wiring the module-level tests cannot see: config discovery, the team interval, the
//! state save between passes. Runs only when TEAM_TEST_PG and TEAM_SERVER_BIN are set; with
//! TEAM_REQUIRE_PG a skip is a failure.
use sesync::team_cli::Command;
use sesync::team_join;
use sesync_team::pg::PgStore;
use sesync_team::service::Service;
use sesync_team::totp::code_at;
use std::process::{Child, Command as Proc, Stdio};
use std::time::{Duration, SystemTime};

const MASTER: [u8; 32] = [7u8; 32];
const ADMIN_TOKEN: &str = "daemon-watch-admin-token-00000000000000";
const SID: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const PLAINTEXT: &str = "daemon watch plaintext marker";

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

fn wait_child(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let end = std::time::Instant::now() + limit;
    while std::time::Instant::now() < end {
        if let Ok(Some(s)) = child.try_wait() {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    None
}

#[test]
fn the_watch_loop_pushes_a_marked_session_and_raises_the_floor_on_the_next_pull() {
    let Some((pg, bin)) = live_env() else { return };
    let schema = "t_daemon_watch";
    let mut client = postgres::Client::connect(&pg, postgres::NoTls).unwrap();
    client
        .batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema};"))
        .unwrap();
    let scoped = format!("{pg}?options=-c%20search_path%3D{schema}");
    PgStore::connect(&scoped, MASTER).unwrap().migrate().unwrap();

    let dir = std::env::temp_dir().join(format!("sesync-daemon-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("master.key"), MASTER).unwrap();
    std::fs::write(dir.join("admin.token"), ADMIN_TOKEN).unwrap();

    let port = 19000 + (std::process::id() % 400) as u16;
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

    let config = dir.join("config");
    let create = Command::Create { name: "personal".into(), members: vec!["writer".into()] };
    sesync::team_cli::run_detailed(&create, &base, ADMIN_TOKEN, &config).expect("create the team");
    let svc = Service::new(PgStore::connect(&scoped, MASTER).unwrap());
    let now = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let code = code_at(&svc.space_secret("personal").unwrap(), now);
    let joined = team_join::join(&base, "personal", "writer", "writer-box", &code, &config)
        .expect("join the writer");
    let approve = Command::Approve { name: "personal".into(), member: "writer".into(), machine: "writer-box".into(),
                                     fingerprint: Some(joined.fingerprint.clone()) };
    sesync::team_cli::run_detailed(&approve, &base, ADMIN_TOKEN, &config).expect("approve the writer");
    team_join::check_approval(None, &config, "personal").expect("the writer picks up its approval");

    // Source layout: one omp journal, old enough that the team quiet rule (0 here) is satisfied.
    let omp = dir.join("omp");
    let sessions = omp.join("sessions").join("-tmp");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(dir.join("pi").join("sessions")).unwrap();
    std::fs::create_dir_all(dir.join("claude").join("projects")).unwrap();
    let journal = sessions.join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"));
    std::fs::write(&journal, format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{SID}\"}}\n{PLAINTEXT}\n")).unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&journal).unwrap();
    file.set_times(std::fs::FileTimes::new()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))).unwrap();

    // The daemon must already know the session is marked; everything else it discovers itself.
    let state_dir = dir.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("state.json"),
        format!("{{\"sources\":{{}},\"marked\":[\"{SID}\"],\"mirror_keys\":[]}}")).unwrap();

    let daemon_log = dir.join("daemon.log");
    let log = std::fs::File::create(&daemon_log).unwrap();
    let mut daemon = Proc::new(env!("CARGO_BIN_EXE_sesync"))
        .arg("--watch")
        .env("HOME", &dir)
        .env("SESYNC_DIR", &state_dir)
        .env("SESYNC_CONFIG_DIR", &config)
        .env("OMP_AGENT_DIR", &omp)
        .env("PI_AGENT_DIR", dir.join("pi"))
        .env("CLAUDE_CONFIG_DIR", dir.join("claude"))
        .env("OPENCODE_DB", dir.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", "/bin/false")
        .env("SESYNC_LOCK", "/bin/true")
        .env("SESYNC_CONV", "/bin/false")
        .env("SESYNC_TEAM_URL", &base)
        .env("SESYNC_TEAM_QUIET", "0")
        .env("SESYNC_POLL", "0.2")
        .env("SESYNC_PASSES", "3")
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawn sesync --watch");
    let status = wait_child(&mut daemon, Duration::from_secs(90));
    let log = std::fs::read_to_string(&daemon_log).unwrap_or_default();
    assert!(status.is_some(), "the watch loop must exit after SESYNC_PASSES; log:\n{log}");
    assert!(status.unwrap().success(), "the watch loop must exit cleanly; log:\n{log}");
    assert!(log.contains("1 pushed"), "the pass must report the push; log:\n{log}");

    // The server holds the session sealed, with no plaintext copy.
    let row = client.query_one(
        &format!("SELECT position(convert_to($1, 'UTF8') IN sealed) FROM {schema}.records WHERE conversation = '{SID}'"),
        &[&PLAINTEXT.to_string()],
    ).unwrap();
    assert_eq!(row.get::<_, i32>(0), 0, "the database must hold no plaintext");

    // The next tick pulled the pushed record and raised this device's floor for it.
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state_dir.join("state.json")).unwrap()).unwrap();
    assert!(state.get("team").and_then(|t| t.get(SID)).and_then(|t| t.get("hash")).is_some(),
        "the push must be recorded in state: {state}");
    assert!(state.get("team_seen").and_then(|t| t.get(SID)).and_then(|v| v.as_u64()).unwrap_or(0) >= 1,
        "the pull must raise the sequence floor: {state}");

    let _ = std::fs::remove_dir_all(&dir);
}
