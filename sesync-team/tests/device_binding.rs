//! Device-bound auth. Once a team has a trust root, a machine must enrol with its device key,
//! and every later request is checked against the current admin-signed list: revoking the device
//! stops it at once, however long its token would have run, even for reads and renewals.
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
const SPACE: &str = "personal";

/// Create the team with a pinned admin key and the version-1 list over the given devices.
async fn create_with_root(app: &axum::Router, admin: &Device, members: &[&str], devices: &[&Device]) {
    let entries: Vec<Vec<u8>> = devices.iter().map(|d| device_entry(d)).collect();
    let list = MemberList::new(1, entries.clone()).sign(admin);
    let (st, j) = post(app, "/admin/team", Some(&format!("Bearer {ADMIN_TOKEN}")), serde_json::json!({
        "space": SPACE, "members": members,
        "admin_public": hex(&admin.public_key()),
        "devices": entries.iter().map(|e| hex(e)).collect::<Vec<_>>(),
        "signed_list": hex(&list.to_bytes()),
    })).await;
    assert_eq!(st, StatusCode::OK, "{j}");
}

fn code(state: &Arc<AppState<MemoryStore>>) -> String {
    let svc = state.service.lock().unwrap();
    sesync_team::totp::code_at(&svc.space_secret(SPACE).unwrap(), T0)
}

#[tokio::test]
async fn a_team_with_a_trust_root_requires_the_device_key_at_enrolment() {
    let (app, state) = app();
    let admin = Device::generate();
    create_with_root(&app, &admin, &["writer"], &[&admin]).await;

    let (st, j) = post(&app, "/enrol", None, serde_json::json!({
        "space": SPACE, "member": "writer", "machine": "box-w", "code": code(&state)})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "no device key, no token: {j}");
    assert!(j["error"].as_str().unwrap().contains("device key"), "{j}");

    let (st, j) = post(&app, "/enrol", None, serde_json::json!({
        "space": SPACE, "member": "writer", "machine": "box-w", "code": code(&state),
        "device": "not-hex"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "a malformed entry is refused: {j}");
}

#[tokio::test]
async fn the_enrol_reply_carries_the_trust_root_for_the_joining_device_to_pin() {
    let (app, state) = app();
    let admin = Device::generate();
    let writer = Device::generate();
    create_with_root(&app, &admin, &["writer"], &[&admin, &writer]).await;

    let (st, j) = post(&app, "/enrol", None, serde_json::json!({
        "space": SPACE, "member": "writer", "machine": "box-w", "code": code(&state),
        "device": hex(&device_entry(&writer))})).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    assert!(j["token"].as_str().is_some());
    assert_eq!(j["list_version"], 1);
    let admin_public = (0..j["admin_public"].as_str().unwrap().len()).step_by(2)
        .map(|i| u8::from_str_radix(&j["admin_public"].as_str().unwrap()[i..i + 2], 16).unwrap())
        .collect::<Vec<u8>>();
    assert_eq!(admin_public, admin.public_key(), "the reply names the key the admin actually holds");
    let list = SignedList::from_bytes(&(0..j["signed_list"].as_str().unwrap().len()).step_by(2)
        .map(|i| u8::from_str_radix(&j["signed_list"].as_str().unwrap()[i..i + 2], 16).unwrap())
        .collect::<Vec<u8>>()).expect("a signed list comes back");
    assert!(Device::verify(&admin.public_key(), &list.list.encode(), &list.signature));
}

#[tokio::test]
async fn a_revoked_device_is_stopped_at_once_despite_a_live_token() {
    let (app, state) = app();
    let admin = Device::generate();
    let writer = Device::generate();
    create_with_root(&app, &admin, &["writer"], &[&admin, &writer]).await;

    let (st, j) = post(&app, "/enrol", None, serde_json::json!({
        "space": SPACE, "member": "writer", "machine": "box-w", "code": code(&state),
        "device": hex(&device_entry(&writer))})).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    let auth = format!("Bearer {}", j["token"].as_str().unwrap());

    // The token works while the device is listed.
    let (st, _) = post(&app, "/list", Some(&auth), serde_json::json!({"space": SPACE, "machine": "box-w"})).await;
    assert_eq!(st, StatusCode::OK);

    // Revoke: the admin publishes version 2 without the writer.
    let v2 = MemberList::new(2, vec![device_entry(&admin)]).sign(&admin);
    let (st, _) = post(&app, "/admin/list", Some(&format!("Bearer {ADMIN_TOKEN}")),
        serde_json::json!({"space": SPACE, "signed_list": hex(&v2.to_bytes())})).await;
    assert_eq!(st, StatusCode::OK);

    // The same, unexpired token is refused now: reads, leases and renewals all go through here.
    let (st, j) = post(&app, "/list", Some(&auth), serde_json::json!({"space": SPACE, "machine": "box-w"})).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "revoked at once: {j}");
    let (st, j) = post(&app, "/renew", Some(&auth), serde_json::json!({"space": SPACE, "machine": "box-w"})).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "renewal cannot outrun revocation: {j}");
    let (st, j) = post(&app, "/lease/acquire", Some(&auth), serde_json::json!({
        "space": SPACE, "conversation": "ses_c", "machine": "box-w"})).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "no leases either: {j}");
}

#[tokio::test]
async fn a_team_without_a_trust_root_keeps_the_old_enrolment() {
    let (app, state) = app();
    // No admin key at creation: no trust root, legacy behaviour.
    let (st, _) = post(&app, "/admin/team", Some(&format!("Bearer {ADMIN_TOKEN}")),
        serde_json::json!({"space": SPACE, "members": ["writer"]})).await;
    assert_eq!(st, StatusCode::OK);

    let (st, j) = post(&app, "/enrol", None, serde_json::json!({
        "space": SPACE, "member": "writer", "machine": "box-w", "code": code(&state)})).await;
    assert_eq!(st, StatusCode::OK, "no device needed without a trust root: {j}");
    let auth = format!("Bearer {}", j["token"].as_str().unwrap());
    let (st, _) = post(&app, "/sessions", Some(&auth), serde_json::json!({"space": SPACE, "machine": "box-w"})).await;
    assert_eq!(st, StatusCode::OK, "the legacy token still lists");
}
