//! Hardening rules for the team server before it can run as a service:
//! per-space authenticator secrets (never a shared or public one), join state kept in the
//! store, and every route requires a caller token bound to a machine.

use sesync_team::service::{MemoryStore, Service, ServiceError};
use sesync_team::totp::code_at;

const T0: u64 = 1_000_020;

#[test]
fn two_spaces_get_different_authenticator_secrets() {
    // A shared or public secret would let anyone with the code join any space.
    let mut s = Service::new(MemoryStore::default());
    s.create_space("team-a", &["alice"]).unwrap();
    s.create_space("team-b", &["alice"]).unwrap();
    let secret_a = s.space_secret("team-a").unwrap();
    let secret_b = s.space_secret("team-b").unwrap();
    assert_ne!(secret_a, secret_b, "each space needs its own secret");
    assert_ne!(secret_a, b"12345678901234567890".to_vec(), "the RFC test vector must never be used");
    assert!(secret_a.len() >= 20, "the secret must be at least 160 bits");
}

#[test]
fn a_code_for_one_space_does_not_join_another() {
    let mut s = Service::new(MemoryStore::default());
    s.create_space("team-a", &["alice"]).unwrap();
    s.create_space("team-b", &["alice"]).unwrap();
    let code_a = code_at(&s.space_secret("team-a").unwrap(), T0);
    assert!(s.enrol("team-b", "alice", "box-1", &code_a, None, T0).is_err(),
        "a code from team-a must not join team-b");
}

#[test]
fn join_state_survives_a_restart_so_a_used_code_stays_used() {
    // The store keeps the join gate, so a restarted service still refuses a used code.
    let mut s = Service::new(MemoryStore::default());
    s.create_space("team-a", &["alice"]).unwrap();
    let code = code_at(&s.space_secret("team-a").unwrap(), T0);
    s.enrol("team-a", "alice", "box-1", &code, None, T0).map(|_| ()).unwrap();
    let store = s.into_store();
    let mut restarted = Service::new(store);
    assert_eq!(
        restarted.enrol("team-a", "alice", "box-1", &code, None, T0).map(|_| ()),
        Err(ServiceError::Auth("AlreadyUsed".into())),
        "a used code must stay used across a restart"
    );
}

#[test]
fn a_lockout_survives_a_restart() {
    let mut s = Service::new(MemoryStore::default());
    s.create_space("team-a", &["alice"]).unwrap();
    for _ in 0..5 {
        let _ = s.enrol("team-a", "alice", "box-1", "000000", None, T0).map(|_| ());
    }
    let store = s.into_store();
    let mut restarted = Service::new(store);
    let good = code_at(&restarted.space_secret("team-a").unwrap(), T0 + 1);
    assert!(matches!(restarted.enrol("team-a", "alice", "box-1", &good, None, T0 + 1), Err(ServiceError::Auth(_))),
        "the lockout must survive a restart");
}

#[test]
fn every_route_requires_a_token_bound_to_the_machine() {
    use sesync_team::auth::{check_token, issue_token};
    let key = [3u8; 32];
    let token = issue_token(&key, "alice", "box-1", T0 + 3600);
    assert!(check_token(&key, &token, "box-1", T0).is_ok(), "a valid token for its machine");
    assert!(check_token(&key, &token, "box-2", T0).is_err(), "a token is bound to one machine");
    assert!(check_token(&key, &token, "box-1", T0 + 7200).is_err(), "an expired token is refused");
    assert!(check_token(&[4u8; 32], &token, "box-1", T0).is_err(), "a token from another issuer is refused");
    assert!(check_token(&key, "", "box-1", T0).is_err(), "no token is refused");
}

#[test]
fn enrol_issues_a_token_only_after_the_authenticator_code() {
    use sesync_team::service::TOKEN_LIFETIME_SECS;
    let mut s = Service::new(MemoryStore::default());
    s.create_space("team-a", &["alice"]).unwrap();
    assert!(s.enrol("team-a", "alice", "box-1", "000000", None, T0).is_err(), "a wrong code issues nothing");
    let code = code_at(&s.space_secret("team-a").unwrap(), T0);
    let token = s.enrol("team-a", "alice", "box-1", &code, None, T0).map(|a| a.token().to_string()).expect("the right code enrols");
    assert_eq!(s.authenticate("team-a", &token, "box-1", T0 + 1).unwrap(), "alice");
    assert!(s.authenticate("team-a", &token, "box-1", T0 + TOKEN_LIFETIME_SECS + 1).is_err(),
        "the token expires after its lifetime");
}

#[test]
fn a_token_works_only_on_its_machine_and_its_space() {
    let mut s = Service::new(MemoryStore::default());
    s.create_space("team-a", &["alice"]).unwrap();
    s.create_space("team-b", &["alice"]).unwrap();
    let code = code_at(&s.space_secret("team-a").unwrap(), T0);
    let token = s.enrol("team-a", "alice", "box-1", &code, None, T0).map(|a| a.token().to_string()).unwrap();
    assert!(s.authenticate("team-a", &token, "box-2", T0).is_err(), "another machine is refused");
    assert!(s.authenticate("team-b", &token, "box-1", T0).is_err(), "another space is refused");
}

#[test]
fn a_restarted_service_still_accepts_an_issued_token() {
    let mut s = Service::new(MemoryStore::default());
    s.create_space("team-a", &["alice"]).unwrap();
    let code = code_at(&s.space_secret("team-a").unwrap(), T0);
    let token = s.enrol("team-a", "alice", "box-1", &code, None, T0).map(|a| a.token().to_string()).unwrap();
    let restarted = Service::new(s.into_store());
    assert_eq!(restarted.authenticate("team-a", &token, "box-1", T0 + 1).unwrap(), "alice",
        "a token must survive a restart");
}
