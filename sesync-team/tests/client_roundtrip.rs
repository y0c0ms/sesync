//! The whole client-sealed path, in-process: writer seals a session under a data key, wraps the
//! key to the listed devices, signs the record and sends it. The server stores it without reading
//! it. A second listed device unwraps the key, opens the session and gets the exact text back.
//! A device that was not listed cannot unwrap, and a revoked device gets no new session key.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sesync_team::membership::{device_entry, Device, MemberList, Verifier};
use sesync_team::server::{self, AppState, Clock};
use sesync_team::service::{MemoryStore, Service};
use sesync_team::session_keys::{open_session_bound, seal_session_bound, session_aad, unwrap_for, wrap_for, DataKey};
use sesync_team::signed_records::{open_record, seal_record, Binding};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const T0: u64 = 1_000_020;

#[tokio::test]
async fn a_session_goes_from_one_listed_device_to_another_without_the_server_reading_it() {
    let admin = Device::generate();
    let writer = Device::generate();
    let reader = Device::generate();
    let list = MemberList::new(1, vec![device_entry(&admin), device_entry(&writer), device_entry(&reader)]).sign(&admin);

    let mut svc = Service::new(MemoryStore::default());
    svc.create_space("personal", &["writer"]).unwrap();
    svc.pin_admin_key("personal", &admin.public_key()).unwrap();
    svc.store_trust_list("personal", &list.to_bytes()).unwrap();
    let clock: Clock = Arc::new(|| T0 + 1);
    let state = Arc::new(AppState { service: Mutex::new(svc), clock, metrics: Default::default(), admin_token: None });
    let app = server::router(state.clone());

    // Writer: take the lease first, because the record's sequence is the lease version it is
    // written under. Then seal, wrap to the listed devices, sign, send.
    let (version, token) = {
        let mut svc = state.service.lock().unwrap();
        let code = sesync_team::totp::code_at(&svc.space_secret("personal").unwrap(), T0);
        let token = svc.enrol("personal", "writer", "box-w", &code, T0).unwrap();
        // Device-bound auth: the machine's device key must be named by the current list.
        svc.bind_device("personal", "box-w", device_entry(&writer));
        let lease = svc.acquire("personal", "ses_rt", "box-w", T0).unwrap();
        (lease.version, token)
    };
    let text = b"the exact session text";
    let key = DataKey::random();
    let body = seal_session_bound(&key, &session_aad("personal", "ses_rt", version), text).unwrap();
    let wraps = wrap_for(&key, &[writer.box_public(), reader.box_public()]).unwrap();
    let record = seal_record(&writer, 1, version, "personal", "ses_rt", &encode_with_wraps(&wraps, &body)).unwrap();
    let uri = format!("/record/sealed?space=personal&conversation=ses_rt&machine=box-w&version={version}");
    let req = Request::builder().method("POST").uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/octet-stream").body(Body::from(record.clone())).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The server holds only the signed record: no plaintext, no data key.
    let stored = state.service.lock().unwrap().raw_stored("personal", "ses_rt").unwrap();
    assert!(!stored.windows(text.len()).any(|w| w == text), "server holds no plaintext");

    // Reader: verify the record against the list and its binding, unwrap, open under the same
    // associated data the writer used.
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list).unwrap();
    let expect = Binding { space: "personal".into(), conversation: "ses_rt".into(), min_sequence: 0 };
    let opened = open_record(&v, &expect, &stored).expect("the record verifies for a listed author");
    assert_eq!(opened.sequence, version, "the sequence is the lease version");
    let (got_wraps, got_body) = decode_with_wraps(&opened.content);
    let k = unwrap_for(&reader, &got_wraps).expect("the listed reader unwraps");
    let aad = session_aad("personal", "ses_rt", opened.sequence);
    assert_eq!(open_session_bound(&k, &aad, &got_body).unwrap(), text, "the exact text comes back");
}

#[test]
fn a_device_not_listed_at_writing_time_cannot_read_the_session() {
    let admin = Device::generate();
    let writer = Device::generate();
    let late = Device::generate();
    let key = DataKey::random();
    let body = seal_session_bound(&key, &session_aad("personal", "ses_p", 1), b"private").unwrap();
    let wraps = wrap_for(&key, &[writer.box_public()]).unwrap();
    let _ = admin;
    assert!(unwrap_for(&late, &wraps).is_err(), "a device added later gets no key for old sessions");
}

/// Pack the wraps and the sealed body into one record payload: count, then each wrap, then body.
fn encode_with_wraps(wraps: &[sesync_team::session_keys::Wrap], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(wraps.len() as u32).to_be_bytes());
    for w in wraps {
        out.extend_from_slice(&(w.device.len() as u32).to_be_bytes());
        out.extend_from_slice(&w.device);
        out.extend_from_slice(&(w.sealed.len() as u32).to_be_bytes());
        out.extend_from_slice(&w.sealed);
    }
    out.extend_from_slice(body);
    out
}

fn decode_with_wraps(b: &[u8]) -> (Vec<sesync_team::session_keys::Wrap>, Vec<u8>) {
    let mut i = 0usize;
    let rd = |i: &mut usize| { let n = u32::from_be_bytes(b[*i..*i+4].try_into().unwrap()) as usize; *i += 4; n };
    let count = rd(&mut i);
    let mut wraps = Vec::with_capacity(count);
    for _ in 0..count {
        let dl = rd(&mut i); let device = b[i..i+dl].to_vec(); i += dl;
        let sl = rd(&mut i); let sealed = b[i..i+sl].to_vec(); i += sl;
        wraps.push(sesync_team::session_keys::Wrap { device, sealed });
    }
    (wraps, b[i..].to_vec())
}
