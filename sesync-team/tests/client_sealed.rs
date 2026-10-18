//! Client-sealed records through the server. The server stores exactly the bytes a device sends
//! and returns them unchanged; it cannot read them, and it refuses a record that a listed device
//! did not sign. These run in-process against the in-memory store.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sesync_team::membership::{device_entry, Device, MemberList, Verifier};
use sesync_team::server::{self, AppState, Clock};
use sesync_team::service::{MemoryStore, Service};
use sesync_team::signed_records::seal_record;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const T0: u64 = 1_000_020;

fn app() -> (axum::Router, Arc<AppState<MemoryStore>>) {
    let mut svc = Service::new(MemoryStore::default());
    svc.create_space("personal", &["writer"]).unwrap();
    let clock: Clock = Arc::new(|| T0 + 1);
    let state = Arc::new(AppState { service: Mutex::new(svc), clock, metrics: Default::default(), admin_token: None });
    (server::router(state.clone()), state)
}


/// A valid token and lease for `machine`, so a request reaches the check its test is about.
fn credentials(state: &Arc<AppState<MemoryStore>>, machine: &str, conv: &str, device: &Device) -> (String, u64) {
    let mut svc = state.service.lock().unwrap();
    let code = sesync_team::totp::code_at(&svc.space_secret("personal").unwrap(), T0);
    let token = svc.enrol("personal", "writer", machine, &code, T0).unwrap();
    // Device-bound auth: the machine's device key must be named by the trust list.
    svc.bind_device("personal", machine, device_entry(device));
    let lease = svc.acquire("personal", conv, machine, T0).unwrap();
    (format!("Bearer {token}"), lease.version)
}

async fn call(app: &axum::Router, uri: &str, auth: Option<&str>, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let mut b = Request::builder().method("POST").uri(uri).header("content-type", "application/octet-stream");
    if let Some(a) = auth { b = b.header("authorization", a); }
    let resp = app.clone().oneshot(b.body(Body::from(body)).unwrap()).await.unwrap();
    let st = resp.status();
    (st, resp.into_body().collect().await.unwrap().to_bytes().to_vec())
}

#[tokio::test]
async fn the_server_stores_and_returns_the_client_sealed_bytes_unchanged() {
    let (app, state) = app();
    let writer = Device::generate();
    let admin = Device::generate();
    // The admin's signed list names the writer; the server stores this list as the space's trust root.
    let list = MemberList::new(1, vec![device_entry(&admin), device_entry(&writer)]).sign(&admin);
    state.service.lock().unwrap().pin_admin_key("personal", &admin.public_key()).unwrap();
    state.service.lock().unwrap().store_trust_list("personal", &list.to_bytes()).unwrap();

    // The writer encrypts the session under its data key first, then signs the ciphertext.
    // The record's sequence is the lease version it is written under: the first lease is 1.
    let key = sesync_team::session_keys::DataKey::random();
    let aad = sesync_team::session_keys::session_aad("personal", "ses_c", 1);
    let body = sesync_team::session_keys::seal_session_bound(&key, &aad, b"session content").unwrap();
    let record = seal_record(&writer, 1, 1, "personal", "ses_c", &body).unwrap();
    let (auth, v) = credentials(&state, "box-w", "ses_c", &writer);
    let uri = format!("/record/sealed?space=personal&conversation=ses_c&machine=box-w&version={v}");
    let (st, body) = call(&app, &uri, Some(&auth), record.clone()).await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    let stored = state.service.lock().unwrap().raw_stored("personal", "ses_c").unwrap();
    assert_eq!(stored, record, "the server holds exactly the bytes the device sent");
    assert!(!stored.windows(15).any(|w| w == b"session content"), "the server never holds the plaintext");
}

#[tokio::test]
async fn a_record_from_an_unlisted_device_is_refused() {
    let (app, state) = app();
    let writer = Device::generate();
    let admin = Device::generate();
    let outsider = Device::generate();
    let list = MemberList::new(1, vec![device_entry(&admin), device_entry(&writer)]).sign(&admin);
    state.service.lock().unwrap().pin_admin_key("personal", &admin.public_key()).unwrap();
    state.service.lock().unwrap().store_trust_list("personal", &list.to_bytes()).unwrap();
    let forged = seal_record(&outsider, 1, 1, "personal", "ses_c", b"x").unwrap();
    let (auth, v) = credentials(&state, "box-w", "ses_c", &writer);
    let uri = format!("/record/sealed?space=personal&conversation=ses_c&machine=box-w&version={v}");
    let (st, _) = call(&app, &uri, Some(&auth), forged).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "an unlisted author is refused");
}

#[tokio::test]
async fn a_record_is_refused_when_no_trust_list_is_set() {
    let (app, state) = app();
    let writer = Device::generate();
    let record = seal_record(&writer, 1, 1, "personal", "ses_c", b"x").unwrap();
    let (auth, v) = credentials(&state, "box-w", "ses_c", &writer);
    let uri = format!("/record/sealed?space=personal&conversation=ses_c&machine=box-w&version={v}");
    let (st, _) = call(&app, &uri, Some(&auth), record).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "no trust root, no accepted record");
}
