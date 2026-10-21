//! Trust-list publish and fetch. The admin publishes newer signed lists; only an admin signature
//! over the pinned key and a strictly increasing version are stored. Members fetch the list to
//! learn the authors they should accept; the fetching client re-verifies the signature, so a
//! lying server cannot widen membership.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sesync_team::membership::{device_entry, Device, MemberList, SignedList};
use sesync_team::server::{self, AppState, Clock};
use sesync_team::service::{MemoryStore, Service};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const ADMIN_TOKEN: &str = "admin-token-for-tests-0123456789abcdef";
const T0: u64 = 1_000_020;

fn app() -> (axum::Router, Arc<AppState<MemoryStore>>) {
    let clock: Clock = Arc::new(|| T0);
    let state = Arc::new(AppState { service: Mutex::new(Service::new(MemoryStore::default())), clock,
        metrics: Default::default(), admin_token: Some(ADMIN_TOKEN.into()) });
    (server::router(state.clone()), state)
}

async fn post(app: &axum::Router, uri: &str, auth: Option<&str>, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let mut b = Request::builder().method("POST").uri(uri).header("content-type", "application/json");
    if let Some(a) = auth { b = b.header("authorization", a); }
    let resp = app.clone().oneshot(b.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let st = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

/// Create the team with a pinned admin key and a version-1 list over `devices`.
async fn create(app: &axum::Router, admin: &Device, members: &[&str], devices: &[&Device]) -> serde_json::Value {
    let entries: Vec<Vec<u8>> = devices.iter().map(|d| device_entry(d)).collect();
    let list = MemberList::new(1, entries.clone()).sign(admin);
    let auth = format!("Bearer {ADMIN_TOKEN}");
    post(app, "/admin/team", Some(&auth), serde_json::json!({
        "space": "personal", "members": members,
        "admin_public": hex(&admin.public_key()),
        "devices": entries.iter().map(|e| hex(e)).collect::<Vec<_>>(),
        "signed_list": hex(&list.to_bytes()),
    })).await.1
}

/// A member token, straight from the service, for the machine given. The device joins with its
/// key; a key the list names is bound to the machine, a new key waits for the admin.
fn token(state: &Arc<AppState<MemoryStore>>, member: &str, machine: &str, device: &Device) -> String {
    let mut svc = state.service.lock().unwrap();
    let code = sesync_team::totp::code_at(&svc.space_secret("personal").unwrap(), T0);
    svc.enrol("personal", member, machine, &code, Some(&device_entry(device)), T0)
        .map(|a| a.token().to_string()).unwrap()
}

#[tokio::test]
async fn an_admin_publishes_a_newer_list_and_a_member_fetches_it() {
    let (app, state) = app();
    let admin = Device::generate();
    let writer = Device::generate();
    let reader = Device::generate();
    create(&app, &admin, &["writer"], &[&admin, &writer, &reader]).await;
    let auth = format!("Bearer {}", token(&state, "writer", "box-w", &writer));

    // Before any publish, the member fetches exactly what was stored at creation.
    let (st, j) = post(&app, "/list", Some(&auth), serde_json::json!({"space":"personal","machine":"box-w"})).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    assert_eq!(j["version"], 1);
    let stored = SignedList::from_bytes(&unhex(j["signed_list"].as_str().unwrap())).expect("a signed list comes back");
    assert!(Device::verify(&admin.public_key(), &stored.list.encode(), &stored.signature), "the admin signed it");
    assert_eq!(stored.list.devices.len(), 3);

    // The admin publishes version 2, dropping the reader (revocation).
    let v2 = MemberList::new(2, vec![device_entry(&admin), device_entry(&writer)]).sign(&admin);
    let (st, j) = post(&app, "/admin/list", Some(&format!("Bearer {ADMIN_TOKEN}")),
        serde_json::json!({"space":"personal","signed_list": hex(&v2.to_bytes())})).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    assert_eq!(j["version"], 2);

    // The member now sees version 2, still verifiable against the admin key it would have pinned.
    let (st, j) = post(&app, "/list", Some(&auth), serde_json::json!({"space":"personal","machine":"box-w"})).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    assert_eq!(j["version"], 2, "a member fetches the newer list");
    let got = SignedList::from_bytes(&unhex(j["signed_list"].as_str().unwrap())).unwrap();
    assert_eq!(got.list.devices.len(), 2, "the revocation is visible");
}

#[tokio::test]
async fn an_older_or_equal_version_is_refused() {
    let (app, _state) = app();
    let admin = Device::generate();
    create(&app, &admin, &["writer"], &[&admin]).await;
    let admin_auth = format!("Bearer {ADMIN_TOKEN}");

    let v2 = MemberList::new(2, vec![device_entry(&admin)]).sign(&admin);
    let (st, _) = post(&app, "/admin/list", Some(&admin_auth), serde_json::json!({
        "space":"personal","signed_list": hex(&v2.to_bytes())})).await;
    assert_eq!(st, StatusCode::OK);

    // Equal version: refused, so a replayed list cannot swap entries under the same version.
    let (st, j) = post(&app, "/admin/list", Some(&admin_auth), serde_json::json!({
        "space":"personal","signed_list": hex(&v2.to_bytes())})).await;
    assert_eq!(st, StatusCode::CONFLICT, "{j}");

    // Older version: refused, so revocation cannot be rolled back.
    let v1 = MemberList::new(1, vec![device_entry(&admin)]).sign(&admin);
    let (st, j) = post(&app, "/admin/list", Some(&admin_auth), serde_json::json!({
        "space":"personal","signed_list": hex(&v1.to_bytes())})).await;
    assert_eq!(st, StatusCode::CONFLICT, "{j}");
}

#[tokio::test]
async fn a_list_signed_by_the_wrong_key_is_refused() {
    let (app, _state) = app();
    let admin = Device::generate();
    let impostor = Device::generate();
    create(&app, &admin, &["writer"], &[&admin]).await;
    let forged = MemberList::new(2, vec![device_entry(&impostor)]).sign(&impostor);
    let (st, j) = post(&app, "/admin/list", Some(&format!("Bearer {ADMIN_TOKEN}")), serde_json::json!({
        "space":"personal","signed_list": hex(&forged.to_bytes())})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "only the pinned admin key signs lists: {j}");
}

#[tokio::test]
async fn publishing_needs_the_admin_token_and_fetching_needs_a_member_token() {
    let (app, state) = app();
    let admin = Device::generate();
    let writer = Device::generate();
    create(&app, &admin, &["writer"], &[&admin, &writer]).await;
    let v2 = MemberList::new(2, vec![device_entry(&admin)]).sign(&admin);

    let body = serde_json::json!({"space":"personal","signed_list": hex(&v2.to_bytes())});
    let (st, _) = post(&app, "/admin/list", None, body).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "no admin token, no publish");

    let (st, _) = post(&app, "/list", None, serde_json::json!({"space":"personal","machine":"box-w"})).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "no member token, no fetch");

    let (st, _) = post(&app, "/list", Some("Bearer nonsense"), serde_json::json!({"space":"personal","machine":"box-w"})).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "a bad token is refused");

    // A stale token machine is refused too: the token is bound to its machine.
    let auth = format!("Bearer {}", token(&state, "writer", "box-w", &writer));
    let (st, _) = post(&app, "/list", Some(&auth), serde_json::json!({"space":"personal","machine":"other-box"})).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn fetching_a_list_a_team_never_had_is_not_found() {
    let (app, state) = app();
    let admin = Device::generate();
    let writer = Device::generate();
    let list = MemberList::new(1, vec![device_entry(&admin)]).sign(&admin);
    let admin_auth = format!("Bearer {ADMIN_TOKEN}");
    // Create without the admin key: no trust root is stored (the pre-trust-root create).
    let (st, _) = post(&app, "/admin/team", Some(&admin_auth), serde_json::json!({
        "space":"personal","members":["writer"]})).await;
    assert_eq!(st, StatusCode::OK);
    let auth = format!("Bearer {}", token(&state, "writer", "box-w", &writer));
    let (st, j) = post(&app, "/list", Some(&auth), serde_json::json!({"space":"personal","machine":"box-w"})).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{j}");
    let _ = list;
}

#[tokio::test]
async fn the_roster_names_listed_and_pending_devices_and_only_the_admin_reads_it() {
    let (app, state) = app();
    let admin = Device::generate();
    let writer = Device::generate();
    let reader = Device::generate();
    create(&app, &admin, &["writer", "reader"], &[&admin, &writer]).await;
    let w_token = token(&state, "writer", "box-w", &writer);
    let _ = token(&state, "reader", "box-r", &reader);

    // Admin only: names, machines and requests stay inside the team's administration.
    let (st, _) = post(&app, "/admin/roster", None, serde_json::json!({"space":"personal"})).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "no token, no roster");
    let (st, _) = post(&app, "/admin/roster", Some(&format!("Bearer {w_token}")), serde_json::json!({"space":"personal"})).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a listed member does not read the roster");
    let admin_auth = format!("Bearer {ADMIN_TOKEN}");
    let (st, j) = post(&app, "/admin/roster", Some(&admin_auth), serde_json::json!({"space":"personal"})).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    assert_eq!(j["version"], 1, "the current list version comes along");
    let signed = SignedList::from_bytes(&unhex(j["signed_list"].as_str().unwrap())).unwrap();
    assert!(Device::verify(&admin.public_key(), &signed.list.encode(), &signed.signature));
    let listed = j["listed"].as_array().unwrap();
    assert_eq!(listed.len(), 2, "{j}");
    let w = listed.iter().find(|d| d["machine"] == "box-w").expect("the writer's machine is named");
    assert_eq!(w["member"], "writer");
    assert_eq!(w["fingerprint"], sesync_team::membership::fingerprint(&device_entry(&writer)));
    assert!(listed.iter().any(|d| d["machine"].is_null() && unhex(d["entry"].as_str().unwrap()) == device_entry(&admin)),
        "the admin key is listed with no machine: {j}");
    let pending = j["pending"].as_array().unwrap();
    assert_eq!(pending.len(), 1, "{j}");
    assert_eq!((pending[0]["member"].as_str(), pending[0]["machine"].as_str()), (Some("reader"), Some("box-r")));
    assert_eq!(pending[0]["fingerprint"], sesync_team::membership::fingerprint(&device_entry(&reader)));
    assert_eq!(unhex(pending[0]["entry"].as_str().unwrap()), device_entry(&reader));

    // The removed routes are gone: revocation is a list change, nothing else.
    for gone in ["/admin/devices", "/admin/forget"] {
        let (st, _) = post(&app, gone, Some(&admin_auth), serde_json::json!({"space":"personal","machine":"box-w"})).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{gone} no longer exists");
    }
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}
