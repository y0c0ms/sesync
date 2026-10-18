//! The sealed read returns the stored signed record to a member, and only to a member. The server
//! does not open it; the client verifies and decrypts.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sesync_team::membership::{device_entry, Device, MemberList};
use sesync_team::server::{self, AppState, Clock};
use sesync_team::service::{MemoryStore, Service};
use sesync_team::signed_records::seal_record;
use sesync_team::session_keys::{seal_session_bound, session_aad, DataKey};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const T0: u64 = 1_000_020;

#[tokio::test]
async fn a_member_reads_back_the_exact_record_bytes() {
    let admin = Device::generate();
    let writer = Device::generate();
    let mut svc = Service::new(MemoryStore::default());
    svc.create_space("personal", &["writer"]).unwrap();
    svc.pin_admin_key("personal", &admin.public_key()).unwrap();
    svc.store_trust_list("personal", &MemberList::new(1, vec![device_entry(&admin), device_entry(&writer)]).sign(&admin).to_bytes()).unwrap();
    let code = sesync_team::totp::code_at(&svc.space_secret("personal").unwrap(), T0);
    let token = svc.enrol("personal", "writer", "box-w", &code, T0).unwrap();
    svc.bind_device("personal", "box-w", device_entry(&writer));
    let lease = svc.acquire("personal", "ses_r", "box-w", T0).unwrap();
    let key = DataKey::random();
    let body = seal_session_bound(&key, &session_aad("personal", "ses_r", lease.version), b"x").unwrap();
    let record = seal_record(&writer, 1, lease.version, "personal", "ses_r", &body).unwrap();
    svc.put_sealed_leased("personal", "ses_r", "box-w", lease.version, record.clone()).unwrap();

    let clock: Clock = Arc::new(|| T0 + 1);
    let state = Arc::new(AppState { service: Mutex::new(svc), clock, metrics: Default::default(), admin_token: None });
    let app = server::router(state);
    let req = Request::builder().method("POST").uri("/record/sealed/read")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"space":"personal","conversation":"ses_r","machine":"box-w"}"#)).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
    assert_eq!(bytes, record, "the stored record comes back byte for byte");
}

#[tokio::test]
async fn a_read_without_a_token_is_refused() {
    let svc = Service::new(MemoryStore::default());
    let clock: Clock = Arc::new(|| T0 + 1);
    let state = Arc::new(AppState { service: Mutex::new(svc), clock, metrics: Default::default(), admin_token: None });
    let app = server::router(state);
    let req = Request::builder().method("POST").uri("/record/sealed/read")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"space":"personal","conversation":"ses_r","machine":"box-w"}"#)).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
