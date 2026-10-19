//! The team service: the rules the HTTP layer must enforce, tested without a network or a
//! database. A store trait is implemented in memory here; Postgres implements the same trait.
//! Each rule below comes from the core modules and must hold at the service boundary.

use sesync_team::service::{Service, ServiceError, MemoryStore};
use sesync_team::totp::code_at;

const T0: u64 = 1_000_020; // a step boundary

/// The space's own authenticator secret, as the operator would enrol it.
fn secret_of(s: &Service<MemoryStore>, space: &str) -> Vec<u8> {
    s.space_secret(space).expect("space has a secret")
}

fn svc() -> Service<MemoryStore> {
    Service::new(MemoryStore::default())
}

#[test]
fn a_machine_joins_with_a_valid_code_and_not_otherwise() {
    let mut s = svc();
    s.create_space("team-alpha", &["alice"]).unwrap();
    assert!(matches!(s.join("team-alpha", "alice", "laptop-a", "000000", T0), Err(ServiceError::Auth(_))));
    s.join("team-alpha", "alice", "laptop-a", &code_at(&secret_of(&s, "team-alpha"), T0), T0).expect("valid code joins");
}

#[test]
fn an_unknown_space_is_refused() {
    let mut s = svc();
    assert!(matches!(s.join("nope", "alice", "laptop-a", "000000", T0), Err(ServiceError::NoSpace(_))));
}

#[test]
fn a_non_member_cannot_join_a_space() {
    let mut s = svc();
    s.create_space("team-alpha", &["alice"]).unwrap();
    assert!(matches!(s.join("team-alpha", "mallory", "laptop-m", &code_at(&secret_of(&s, "team-alpha"), T0), T0),
        Err(ServiceError::NotMember(_))));
}

#[test]
fn a_turn_on_one_machine_blocks_another_through_the_service() {
    let mut s = svc();
    s.create_space("team-alpha", &["alice"]).unwrap();
    s.join("team-alpha", "alice", "laptop-a", &code_at(&secret_of(&s, "team-alpha"), T0), T0).unwrap();
    s.join("team-alpha", "alice", "laptop-b", &code_at(&secret_of(&s, "team-alpha"), T0 + 30), T0 + 30).unwrap();
    s.acquire("team-alpha", "ses_1", "laptop-a", T0).unwrap();
    assert!(matches!(s.acquire("team-alpha", "ses_1", "laptop-b", T0 + 1),
        Err(ServiceError::Lease(_))));
}

#[test]
fn a_write_needs_the_live_lease() {
    let mut s = svc();
    s.create_space("team-alpha", &["alice"]).unwrap();
    s.join("team-alpha", "alice", "laptop-a", &code_at(&secret_of(&s, "team-alpha"), T0), T0).unwrap();
    let lease = s.acquire("team-alpha", "ses_1", "laptop-a", T0).unwrap();
    s.put_record("team-alpha", "ses_1", "laptop-a", lease.version, b"sealed bytes".to_vec(), T0).unwrap();
    s.release("team-alpha", "ses_1", "laptop-a").unwrap();
    assert!(matches!(s.put_record("team-alpha", "ses_1", "laptop-a", lease.version, b"x".to_vec(), T0 + 1),
        Err(ServiceError::Lease(_))), "after release, a write is refused");
}

#[test]
fn a_stale_write_is_refused_after_takeover() {
    let mut s = svc();
    s.create_space("team-alpha", &["alice"]).unwrap();
    s.join("team-alpha", "alice", "laptop-a", &code_at(&secret_of(&s, "team-alpha"), T0), T0).unwrap();
    s.join("team-alpha", "alice", "laptop-b", &code_at(&secret_of(&s, "team-alpha"), T0 + 30), T0 + 30).unwrap();
    let a = s.acquire("team-alpha", "ses_1", "laptop-a", T0).unwrap();
    let after = T0 + 30 * 60;
    s.acquire("team-alpha", "ses_1", "laptop-b", after).unwrap();
    assert!(matches!(s.put_record("team-alpha", "ses_1", "laptop-a", a.version, b"stale".to_vec(), after + 1),
        Err(ServiceError::Lease(_))), "a's stale write must be refused");
}

#[test]
fn records_are_stored_sealed_and_read_back_only_by_members() {
    let mut s = svc();
    s.create_space("team-alpha", &["alice", "bob"]).unwrap();
    s.join("team-alpha", "alice", "laptop-a", &code_at(&secret_of(&s, "team-alpha"), T0), T0).unwrap();
    let lease = s.acquire("team-alpha", "ses_1", "laptop-a", T0).unwrap();
    s.put_record("team-alpha", "ses_1", "laptop-a", lease.version, b"plain session".to_vec(), T0).unwrap();
    let raw = s.raw_stored("team-alpha", "ses_1").unwrap();
    assert!(!raw.windows(13).any(|w| w == b"plain session"), "the store must hold ciphertext only");
    assert_eq!(s.get_record("team-alpha", "ses_1", "alice").unwrap(), b"plain session".to_vec());
    assert!(matches!(s.get_record("team-alpha", "ses_1", "mallory"), Err(ServiceError::NotMember(_))));
}

#[test]
fn backup_policy_is_validated_before_it_is_saved() {
    let mut s = svc();
    s.create_space("team-alpha", &["alice"]).unwrap();
    assert!(s.set_backup("team-alpha", true, 0, 7).is_err(), "a zero interval is never saved");
    s.set_backup("team-alpha", true, 24, 7).expect("a valid policy is saved");
}

#[test]
fn a_machine_writes_as_the_member_who_joined_on_it() {
    // The machine is registered to the member who joined from it. A write is sealed as
    // that member, never as a name guessed from the machine's label.
    let mut s = svc();
    s.create_space("team-alpha", &["alice"]).unwrap();
    s.join("team-alpha", "alice", "box-1", &code_at(&secret_of(&s, "team-alpha"), T0), T0).unwrap();
    let lease = s.acquire("team-alpha", "ses_9", "box-1", T0).unwrap();
    s.put_record("team-alpha", "ses_9", "box-1", lease.version, b"owned by alice".to_vec(), T0)
        .expect("the registered member may write");
    assert_eq!(s.get_record("team-alpha", "ses_9", "alice").unwrap(), b"owned by alice".to_vec());
}

#[test]
fn an_unregistered_machine_cannot_write() {
    let mut s = svc();
    s.create_space("team-alpha", &["alice"]).unwrap();
    let lease = s.acquire("team-alpha", "ses_9", "stranger-box", T0).unwrap();
    assert!(matches!(s.put_record("team-alpha", "ses_9", "stranger-box", lease.version, b"x".to_vec(), T0),
        Err(ServiceError::NotMember(_))), "a machine that never joined cannot write");
}

#[test]
fn http_enrol_and_acquire_return_the_right_status_codes() {
    use sesync_team::http::{acquire, enrol, EnrolBody, LeaseBody};
    let mut s = svc();
    s.create_space("team-alpha", &["alice"]).unwrap();
    let good = EnrolBody { space: "team-alpha".into(), member: "alice".into(), machine: "box-1".into(),
                           code: code_at(&secret_of(&s, "team-alpha"), T0), device: None };
    let (status, json) = enrol(&mut s, &good, T0);
    assert_eq!(status, 200);
    let header = format!("Bearer {}", json["token"].as_str().unwrap());
    let bad = EnrolBody { code: "000000".into(), ..good };
    assert_eq!(enrol(&mut s, &bad, T0).0, 401, "a wrong code is 401");
    let a = LeaseBody { space: "team-alpha".into(), conversation: "ses_1".into(), machine: "box-1".into() };
    let (code, body) = acquire(&mut s, Some(&header), &a, T0);
    assert_eq!(code, 200);
    assert_eq!(body["version"], 1);
}

#[test]
fn http_errors_never_carry_plaintext_or_key_material() {
    use sesync_team::http::error_response;
    use sesync_team::service::ServiceError;
    for e in [ServiceError::Crypto("secret key bytes".into()), ServiceError::Auth("code 123".into())] {
        let (_, body) = error_response(&e);
        let text = body.to_string();
        assert!(!text.contains("secret"), "no key material in an error: {text}");
        assert!(!text.contains("123"), "no code in an error: {text}");
    }
}
