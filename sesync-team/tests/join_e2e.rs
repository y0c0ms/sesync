//! The join and renew calls a member machine makes, against a real team-server process over
//! HTTP: enrol with the authenticator code, then renew the token. Runs only when TEAM_TEST_PG
//! names a throwaway database and TEAM_SERVER_BIN names the built binary; otherwise it skips.
#[path = "common/mod.rs"]
mod common;

use sesync_team::pg::PgStore;
use sesync_team::service::Service;
use sesync_team::totp::code_at;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}

/// Minimal HTTP POST over a raw socket, so the test needs no client dependency.
fn post(port: u16, path: &str, auth: Option<&str>, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let auth_line = auth.map(|a| format!("Authorization: {a}\r\n")).unwrap_or_default();
    let req = format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
                       Content-Length: {}\r\n{auth_line}Connection: close\r\n\r\n{body}", body.len());
    s.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status: u16 = resp[9..12].parse().unwrap();
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

#[test]
fn enrol_then_renew_over_a_real_server() {
    let Some((url, bin)) = common::live_env() else { return };
    let schema = "t_join_e2e";
    {
        let mut c = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        c.batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema};")).unwrap();
    }
    let scoped = format!("{url}?options=-c%20search_path%3D{schema}");
    let store = PgStore::connect(&scoped, [7u8; 32]).unwrap();
    store.migrate().unwrap();
    let mut svc = Service::new(store);
    svc.create_space("personal", &["member-box"]).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let code = code_at(&svc.space_secret("personal").unwrap(), now);
    drop(svc);

    let dir = std::env::temp_dir().join(format!("sesync-join-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("master.key"), [7u8; 32]).unwrap();
    let port = 18300 + (std::process::id() % 500) as u16;
    let child = Command::new(&bin)
        .env("TEAM_SERVER_BIND", format!("127.0.0.1:{port}"))
        .env("TEAM_SERVER_DATABASE", &scoped)
        .env("TEAM_MASTER_KEY_FILE", dir.join("master.key"))
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().expect("start team-server");
    let _server = Server(child);
    std::thread::sleep(std::time::Duration::from_secs(3));

    let (st, body) = post(port, "/enrol", None,
        &format!(r#"{{"space":"personal","member":"member-box","machine":"box-2","code":"{code}"}}"#));
    assert_eq!(st, 200, "enrol: {body}");
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"].as_str().unwrap().to_string();

    // Renew a second later: a token has a one-second resolution, so an immediate renewal would
    // legitimately be identical to the enrolment token.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (st, body) = post(port, "/renew", Some(&format!("Bearer {token}")),
        r#"{"space":"personal","machine":"box-2"}"#);
    assert_eq!(st, 200, "renew: {body}");
    let renewed = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"].as_str().unwrap().to_string();
    assert_ne!(renewed, token, "renew issues a new token");

    let (st, _) = post(port, "/renew", None, r#"{"space":"personal","machine":"box-2"}"#);
    assert_eq!(st, 401, "renew without a token is refused");
    let _ = std::fs::remove_dir_all(&dir);
}
