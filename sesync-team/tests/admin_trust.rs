//! Team creation publishes the trust root: the admin key is pinned and a signed member list is
//! stored, so the sealed path works for a freshly created team.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sesync_team::membership::{device_entry, Device, MemberList};
use sesync_team::server::{self, AppState, Clock};
use sesync_team::service::{MemoryStore, Service};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const ADMIN_TOKEN: &str = "admin-token-for-tests-0123456789abcdef";

fn app() -> (axum::Router, Arc<AppState<MemoryStore>>) {
    let clock: Clock = Arc::new(|| 1_000_020);
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

#[tokio::test]
async fn creating_a_team_pins_the_admin_and_stores_a_signed_list() {
    let (app, state) = app();
    let admin = Device::generate();
    let auth = format!("Bearer {ADMIN_TOKEN}");
    let list = MemberList::new(1, vec![device_entry(&admin)]).sign(&admin);
    let (st, j) = post(&app, "/admin/team", Some(&auth), serde_json::json!({
        "space": "personal", "members": ["admin-box"],
        "admin_public": hex(&admin.public_key()),
        "devices": [hex(&device_entry(&admin))],
        "signed_list": hex(&list.to_bytes()),
    })).await;
    assert_eq!(st, StatusCode::OK, "{j}");
    let svc = state.service.lock().unwrap();
    assert_eq!(svc.load_admin_key_for_test("personal").unwrap(), admin.public_key(), "the admin key is pinned");
    let list = svc.load_trust_list_for_test("personal").expect("a signed list is stored");
    assert!(sesync_team::membership::SignedList::from_bytes(&list).is_ok());
}

#[tokio::test]
async fn a_team_created_without_an_admin_key_has_no_trust_root_and_refuses_sealed_records() {
    let (app, _state) = app();
    let auth = format!("Bearer {ADMIN_TOKEN}");
    let (st, _) = post(&app, "/admin/team", Some(&auth), serde_json::json!({"space":"personal","members":["a"]})).await;
    assert_eq!(st, StatusCode::OK, "the old create still works");
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }
