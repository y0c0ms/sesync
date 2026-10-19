//! POST /sessions carries a "sequences" object next to "conversations". Each conversation whose
//! stored record is a signed record maps to the sequence that record carries. "conversations" is
//! unchanged: every id, sorted. Only a member with a token may ask. These run the real routes
//! in-process against the in-memory store.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sesync_team::membership::{device_entry, Device, MemberList};
use sesync_team::server::{self, AppState, Clock};
use sesync_team::service::{MemoryStore, Service};
use sesync_team::session_keys::{seal_session_bound, session_aad, DataKey};
use sesync_team::signed_records::seal_record;
use sesync_team::totp::code_at;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const ADMIN_TOKEN: &str = "admin-token-for-tests-0123456789abcdef";
const T0: u64 = 1_000_020;
const SPACE: &str = "personal";

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

fn app() -> (axum::Router, Arc<AppState<MemoryStore>>) {
    let clock: Clock = Arc::new(|| T0);
    let state = Arc::new(AppState { service: Mutex::new(Service::new(MemoryStore::default())), clock,
        metrics: Default::default(), admin_token: Some(ADMIN_TOKEN.into()) });
    (server::router(state.clone()), state)
}

async fn send(app: &axum::Router, uri: &str, auth: Option<&str>, body: Vec<u8>) -> (StatusCode, serde_json::Value) {
    let mut b = Request::builder().method("POST").uri(uri).header("content-type", "application/json");
    if let Some(a) = auth { b = b.header("authorization", a); }
    let resp = app.clone().oneshot(b.body(Body::from(body)).unwrap()).await.unwrap();
    let st = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

async fn post_json(app: &axum::Router, uri: &str, auth: Option<&str>, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    send(app, uri, auth, body.to_string().into_bytes()).await
}

/// A team with a pinned admin key, the admin's list over the admin and one writer device, and the
/// writer enrolled on `box-w` with its device key. Returns the writer's bearer header.
async fn team_with_enrolled_writer(app: &axum::Router, state: &Arc<AppState<MemoryStore>>, admin: &Device, writer: &Device) -> String {
    let entries = vec![device_entry(admin), device_entry(writer)];
    let list = MemberList::new(1, entries.clone()).sign(admin);
    let (st, j) = post_json(app, "/admin/team", Some(&format!("Bearer {ADMIN_TOKEN}")), serde_json::json!({
        "space": SPACE, "members": ["writer"],
        "admin_public": hex(&admin.public_key()),
        "devices": entries.iter().map(|e| hex(e)).collect::<Vec<_>>(),
        "signed_list": hex(&list.to_bytes()),
    })).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    let code = {
        let svc = state.service.lock().unwrap();
        code_at(&svc.space_secret(SPACE).unwrap(), T0)
    };
    let (st, j) = post_json(app, "/enrol", None, serde_json::json!({
        "space": SPACE, "member": "writer", "machine": "box-w", "code": code,
        "device": hex(&device_entry(writer))})).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    format!("Bearer {}", j["token"].as_str().unwrap())
}

async fn acquire(app: &axum::Router, auth: &str, conv: &str) -> u64 {
    let (st, j) = post_json(app, "/lease/acquire", Some(auth),
        serde_json::json!({"space": SPACE, "conversation": conv, "machine": "box-w"})).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    j["version"].as_u64().unwrap()
}

async fn release(app: &axum::Router, auth: &str, conv: &str) {
    let (st, j) = post_json(app, "/lease/release", Some(auth),
        serde_json::json!({"space": SPACE, "conversation": conv, "machine": "box-w"})).await;
    assert_eq!(st, StatusCode::OK, "{j}");
}

/// A signed record for `conv` at `sequence`, written by `writer`.
fn signed_record(writer: &Device, conv: &str, sequence: u64) -> Vec<u8> {
    let body = seal_session_bound(&DataKey::random(), &session_aad(SPACE, conv, sequence), b"session").unwrap();
    seal_record(writer, 1, sequence, SPACE, conv, &body).unwrap()
}

/// Store a signed record under the writer's live lease, as the sealed write route does.
async fn put_signed(app: &axum::Router, auth: &str, writer: &Device, conv: &str) -> u64 {
    let version = acquire(app, auth, conv).await;
    let uri = format!("/record/sealed?space={SPACE}&conversation={conv}&machine=box-w&version={version}");
    let (st, j) = send(app, &uri, Some(auth), signed_record(writer, conv, version)).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    release(app, auth, conv).await;
    version
}

async fn list(app: &axum::Router, auth: Option<&str>) -> (StatusCode, serde_json::Value) {
    post_json(app, "/sessions", auth, serde_json::json!({"space": SPACE, "machine": "box-w"})).await
}

#[tokio::test]
async fn the_listing_carries_each_stored_record_sequence_and_still_every_id() {
    let (app, state) = app();
    let admin = Device::generate();
    let writer = Device::generate();
    let auth = team_with_enrolled_writer(&app, &state, &admin, &writer).await;

    // ses_a is written twice, so its stored record is the second one, at the second lease version.
    put_signed(&app, &auth, &writer, "ses_a").await;
    let a_latest = put_signed(&app, &auth, &writer, "ses_a").await;
    let b_only = put_signed(&app, &auth, &writer, "ses_b").await;
    assert!(a_latest > 1, "the second write is at a higher version");

    let (st, j) = list(&app, Some(&auth)).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    assert_eq!(j["conversations"], serde_json::json!(["ses_a", "ses_b"]), "every id, sorted, as before");
    assert_eq!(j["sequences"]["ses_a"], serde_json::json!(a_latest), "the stored record's sequence");
    assert_eq!(j["sequences"]["ses_b"], serde_json::json!(b_only));
}

#[tokio::test]
async fn a_conversation_whose_stored_bytes_carry_no_sequence_is_listed_without_one() {
    // A plain (non-signed) write stores bytes with no sequence. The id stays in "conversations";
    // it is left out of "sequences", so a client fetches it as it did before.
    let (app, state) = app();
    let admin = Device::generate();
    let writer = Device::generate();
    let auth = team_with_enrolled_writer(&app, &state, &admin, &writer).await;

    put_signed(&app, &auth, &writer, "ses_signed").await;
    let version = acquire(&app, &auth, "ses_plain").await;
    let uri = format!("/record?space={SPACE}&conversation=ses_plain&machine=box-w&version={version}");
    let (st, j) = send(&app, &uri, Some(&auth), b"plain session".to_vec()).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    release(&app, &auth, "ses_plain").await;

    let (st, j) = list(&app, Some(&auth)).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    assert_eq!(j["conversations"], serde_json::json!(["ses_plain", "ses_signed"]));
    assert!(j["sequences"].get("ses_plain").is_none(), "no sequence is advertised for it: {j}");
    assert!(j["sequences"].get("ses_signed").is_some(), "the signed one is advertised: {j}");
}

#[tokio::test]
async fn the_listing_needs_a_member_token() {
    let (app, state) = app();
    let admin = Device::generate();
    let writer = Device::generate();
    let auth = team_with_enrolled_writer(&app, &state, &admin, &writer).await;
    put_signed(&app, &auth, &writer, "ses_a").await;

    let (st, j) = list(&app, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "no token, no listing");
    assert!(j.get("sequences").is_none() && j.get("conversations").is_none(), "{j}");
}
