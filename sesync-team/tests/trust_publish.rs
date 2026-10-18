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

/// A member token, straight from the service, for the machine given, with its device bound.
fn token(state: &Arc<AppState<MemoryStore>>, member: &str, machine: &str, device: &Device) -> String {
    let mut svc = state.service.lock().unwrap();
    let code = sesync_team::totp::code_at(&svc.space_secret("personal").unwrap(), T0);
    let token = svc.enrol("personal", member, machine, &code, T0).unwrap();
    svc.bind_device("personal", machine, device_entry(device));
    token
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
async fn the_admin_reads_enrolled_devices_and_forgets_one() {
    let (app, state) = app();
    let admin = Device::generate();
    let writer = Device::generate();
    let reader = Device::generate();
    create(&app, &admin, &["writer", "reader"], &[&admin]).await;
    let w_token = token(&state, "writer", "box-w", &writer);
    let r_token = token(&state, "reader", "box-r", &reader);

    // Admin only: the device list names every machine in the team.
    let (st, _) = post(&app, "/admin/devices", None, serde_json::json!({"space":"personal"})).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "members do not enumerate the team's machines");
    let admin_auth = format!("Bearer {ADMIN_TOKEN}");
    let (st, j) = post(&app, "/admin/devices", Some(&admin_auth), serde_json::json!({"space":"personal"})).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    assert_eq!(j["version"], 1, "the current list version comes along");
    let names: Vec<&str> = j["devices"].as_array().unwrap().iter()
        .map(|d| d["machine"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["box-r", "box-w"], "sorted by machine");
    assert_eq!(unhex(j["devices"][1]["entry"].as_str().unwrap()), device_entry(&writer));

    // Publish v2 naming both devices: that is what makes the bound devices members.
    let v2 = MemberList::new(2, vec![device_entry(&admin), device_entry(&writer), device_entry(&reader)]).sign(&admin);
    let (st, _) = post(&app, "/admin/list", Some(&admin_auth), serde_json::json!({
        "space":"personal","signed_list": hex(&v2.to_bytes())})).await;
    assert_eq!(st, StatusCode::OK);
    let mut svc = state.service.lock().unwrap();
    assert!(svc.authenticate("personal", &w_token, "box-w", T0).is_ok(), "the published writer is a member");
    assert!(svc.authenticate("personal", &r_token, "box-r", T0).is_ok());
    drop(svc);

    // Forgetting a machine kills its token at once and removes it from the next list.
    let (st, j) = post(&app, "/admin/forget", Some(&admin_auth), serde_json::json!({"space":"personal","machine":"box-w"})).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    let mut svc = state.service.lock().unwrap();
    assert!(svc.authenticate("personal", &w_token, "box-w", T0).is_err(), "the writer token is dead at once");
    assert!(svc.authenticate("personal", &r_token, "box-r", T0).is_ok(), "the reader is untouched");
    drop(svc);
    let (_, j) = post(&app, "/admin/devices", Some(&admin_auth), serde_json::json!({"space":"personal"})).await;
    let names: Vec<&str> = j["devices"].as_array().unwrap().iter()
        .map(|d| d["machine"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["box-r"], "the forgotten machine is gone from the admin view");
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}
