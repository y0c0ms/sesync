//! The push end to end against a real team-server process: a marked session is written under a
//! lease and the server holds it sealed; a second machine is refused while the first holds the
//! lease. Runs only when TEAM_TEST_PG and TEAM_SERVER_BIN are set; otherwise it skips.
#[path = "common/mod.rs"]
mod common;

use sesync_team::pg::PgStore;
use sesync_team::service::Service;
use sesync_team::totp::code_at;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

struct Server(Child);
impl Drop for Server { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

fn http(port: u16, method: &str, path: &str, auth: Option<&str>, ctype: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let auth_line = auth.map(|a| format!("Authorization: {a}\r\n")).unwrap_or_default();
    let head = format!("{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{auth_line}Connection: close\r\n\r\n", body.len());
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let status: u16 = std::str::from_utf8(&resp[9..12]).unwrap().parse().unwrap();
    let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (status, resp[split + 4..].to_vec())
}

#[test]
fn a_marked_session_is_stored_and_a_second_machine_is_held_out() {
    let Some((url, bin)) = common::live_env() else { return };
    let schema = "t_push_e2e";
    {
        let mut c = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        c.batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema};")).unwrap();
    }
    let scoped = format!("{url}?options=-c%20search_path%3D{schema}");
    let store = PgStore::connect(&scoped, [9u8; 32]).unwrap();
    store.migrate().unwrap();
    let mut svc = Service::new(store);
    svc.create_space("personal", &["box-a", "box-b"]).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let secret = svc.space_secret("personal").unwrap();
    let dir = std::env::temp_dir().join(format!("sesync-push-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("master.key"), [9u8; 32]).unwrap();
    let port = 18400 + (std::process::id() % 400) as u16;
    let child = Command::new(&bin)
        .env("TEAM_SERVER_BIND", format!("127.0.0.1:{port}"))
        .env("TEAM_SERVER_DATABASE", &scoped)
        .env("TEAM_MASTER_KEY_FILE", dir.join("master.key"))
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let _server = Server(child);
    std::thread::sleep(std::time::Duration::from_secs(3));

    let enrol = |machine: &str, code: &str| -> String {
        let body = format!(r#"{{"space":"personal","member":"{}","machine":"{machine}","code":"{code}"}}"#,
                           if machine == "box-a" { "box-a" } else { "box-b" });
        let (st, b) = http(port, "POST", "/enrol", None, "application/json", body.as_bytes());
        assert_eq!(st, 200, "{}", String::from_utf8_lossy(&b));
        let t = serde_json::from_slice::<serde_json::Value>(&b).unwrap()["token"].as_str().unwrap().to_string();
        format!("Bearer {t}")
    };
    let code = code_at(&secret, now);
    let token_a = enrol("box-a", &code);
    let token_b = enrol("box-b", &code);

    // Machine A takes the lease and writes a marked session under it.
    let lease = format!(r#"{{"space":"personal","conversation":"ses_push","machine":"box-a"}}"#);
    let (st, b) = http(port, "POST", "/lease/acquire", Some(&token_a), "application/json", lease.as_bytes());
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&b));
    let version = serde_json::from_slice::<serde_json::Value>(&b).unwrap()["version"].as_u64().unwrap();
    let payload = b"{\"type\":\"user\",\"text\":\"pushed session\"}\n";
    let (st, _) = http(port, "POST",
        &format!("/record?space=personal&conversation=ses_push&machine=box-a&version={version}"),
        Some(&token_a), "application/octet-stream", payload);
    assert_eq!(st, 200, "the lease holder writes");

    // Machine B is held out while A holds the lease.
    let lease_b = r#"{"space":"personal","conversation":"ses_push","machine":"box-b"}"#;
    let (st, _) = http(port, "POST", "/lease/acquire", Some(&token_b), "application/json", lease_b.as_bytes());
    assert_eq!(st, 409, "the other machine is held out");

    // Reading it back as a member returns the exact bytes.
    let read = r#"{"space":"personal","conversation":"ses_push","machine":"box-a"}"#;
    let (st, back) = http(port, "POST", "/record/read", Some(&token_a), "application/json", read.as_bytes());
    assert_eq!(st, 200);
    assert_eq!(back, payload, "the pushed session reads back unchanged");
    let _ = std::fs::remove_dir_all(&dir);
}
