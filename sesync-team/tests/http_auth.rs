//! The HTTP layer's auth rules, tested on the handler functions without a socket.
//! Every route except /enrol needs a bearer token bound to the requesting machine.

#[path = "common/mod.rs"]
mod common;

use sesync_team::http::{self, EnrolBody, LeaseBody};
use sesync_team::service::{MemoryStore, Service};
use sesync_team::totp::code_at;

const T0: u64 = 1_000_020;

fn setup() -> (Service<MemoryStore>, String) {
    let mut s = Service::new(MemoryStore::default());
    s.create_space("team-a", &["alice"]).unwrap();
    let code = code_at(&s.space_secret("team-a").unwrap(), T0);
    let token = s.enrol("team-a", "alice", "box-1", &code, T0).unwrap();
    (s, token)
}

fn lease(machine: &str) -> LeaseBody {
    LeaseBody { space: "team-a".into(), conversation: "c1".into(), machine: machine.into() }
}

#[test]
fn enrol_with_the_right_code_returns_a_token_and_lifetime() {
    let mut s = Service::new(MemoryStore::default());
    s.create_space("team-a", &["alice"]).unwrap();
    let code = code_at(&s.space_secret("team-a").unwrap(), T0);
    let body = EnrolBody { space: "team-a".into(), member: "alice".into(), machine: "box-1".into(), code, device: None };
    let (status, json) = http::enrol(&mut s, &body, T0);
    assert_eq!(status, 200);
    assert!(json["token"].as_str().unwrap().len() > 10);
    assert_eq!(json["expires_in"], 86400);
}

#[test]
fn enrol_with_a_wrong_code_is_401_and_issues_nothing() {
    let mut s = Service::new(MemoryStore::default());
    s.create_space("team-a", &["alice"]).unwrap();
    let body = EnrolBody { space: "team-a".into(), member: "alice".into(), machine: "box-1".into(), code: "000000".into(), device: None };
    let (status, json) = http::enrol(&mut s, &body, T0);
    assert_eq!(status, 401);
    assert!(json.get("token").is_none());
}

#[test]
fn a_lease_without_a_token_is_401() {
    let (mut s, _) = setup();
    let (status, json) = http::acquire(&mut s, None, &lease("box-1"), T0);
    assert_eq!(status, 401);
    assert_eq!(json["error"], "missing bearer token");
}

#[test]
fn a_lease_with_a_token_for_another_machine_is_401() {
    let (mut s, token) = setup();
    let header = format!("Bearer {token}");
    let (status, _) = http::acquire(&mut s, Some(&header), &lease("box-2"), T0);
    assert_eq!(status, 401, "a token is bound to the machine that enrolled");
}

#[test]
fn a_lease_with_the_right_token_succeeds() {
    let (mut s, token) = setup();
    let header = format!("Bearer {token}");
    let (status, _) = http::acquire(&mut s, Some(&header), &lease("box-1"), T0 + 1);
    assert_eq!(status, 200);
}

#[test]
fn a_non_bearer_scheme_is_refused() {
    let (mut s, token) = setup();
    let (status, _) = http::acquire(&mut s, Some(&format!("Basic {token}")), &lease("box-1"), T0);
    assert_eq!(status, 401);
}

#[test]
fn release_also_needs_a_token() {
    let (mut s, _) = setup();
    let (status, _) = http::release(&mut s, None, &lease("box-1"), T0);
    assert_eq!(status, 401);
}

#[test]
fn an_error_response_never_echoes_secret_material() {
    let (mut s, token) = setup();
    let header = format!("Bearer {token}");
    let (_, json) = http::acquire(&mut s, Some(&header), &lease("box-2"), T0);
    let text = json.to_string();
    assert!(!text.contains(&token), "the response must not echo the token");
}

#[test]
fn listing_sessions_needs_a_token_and_returns_only_ids() {
    use sesync_team::http::list_sessions;
    let (mut s, token) = setup();
    // Two conversations; the list must carry their ids only.
    let lease = s.acquire("team-a", "ses_b", "box-1", T0).unwrap();
    s.put_record("team-a", "ses_b", "box-1", lease.version, b"private body".to_vec(), T0).unwrap();
    let lease = s.acquire("team-a", "ses_a", "box-1", T0).unwrap();
    s.put_record("team-a", "ses_a", "box-1", lease.version, b"private body".to_vec(), T0).unwrap();

    let (status, _) = list_sessions(&s, None, "team-a", "box-1", T0);
    assert_eq!(status, 401, "no token, no listing");

    let header = format!("Bearer {token}");
    let (status, json) = list_sessions(&s, Some(&header), "team-a", "box-1", T0 + 1);
    assert_eq!(status, 200);
    assert_eq!(json["conversations"], serde_json::json!(["ses_a", "ses_b"]), "sorted ids");
    assert!(!json.to_string().contains("private body"), "the listing never carries contents");
}

#[test]
fn a_token_for_another_machine_cannot_list_a_space() {
    use sesync_team::http::list_sessions;
    let (s, token) = setup();
    let header = format!("Bearer {token}");
    let (status, _) = list_sessions(&s, Some(&header), "team-a", "box-2", T0);
    assert_eq!(status, 401);
}

#[test]
fn health_is_200_when_the_store_answers() {
    use sesync_team::http::health;
    let s = Service::new(MemoryStore::default());
    let (status, json) = health(&s);
    assert_eq!(status, 200);
    assert_eq!(json["status"], "ok");
}

#[test]
fn health_is_503_when_the_store_does_not_answer() {
    use sesync_team::http::health;
    use sesync_team::service::Service;
    let s = Service::new(common::TestStore::database_down());
    let (status, json) = health(&s);
    assert_eq!(status, 503, "a dead database must fail the health check");
    assert!(!json.to_string().contains("password"), "the health reply carries no connection detail");
}