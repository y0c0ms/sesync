//! Team creation and invitation, as the admin does them on the operator machine. The rules: a space is
//! created once, its secret is shown only at creation, and only an admin may invite.

use sesync_team::service::{MemoryStore, Service, ServiceError};
use sesync_team::totp::code_at;

const T0: u64 = 1_000_020;

#[test]
fn a_space_can_be_created_only_once() {
    let mut s = Service::new(MemoryStore::default());
    s.create_space("personal", &["alice"]).unwrap();
    let before = s.space_secret("personal").unwrap();
    assert!(s.create_space("personal", &["mallory"]).is_err(), "a second create must be refused");
    assert_eq!(s.space_secret("personal").unwrap(), before, "the existing secret is not replaced");
}

#[test]
fn an_invited_member_can_enrol_and_others_cannot() {
    let mut s = Service::new(MemoryStore::default());
    s.create_space("personal", &["alice"]).unwrap();
    s.invite("personal", "bob").unwrap();
    let code = code_at(&s.space_secret("personal").unwrap(), T0);
    assert!(s.enrol("personal", "bob", "laptop-b", &code, None, T0).is_ok(), "an invited member enrols");
    assert!(s.enrol("personal", "carol", "laptop-c", &code, None, T0).is_err(), "a non-member does not");
}

#[test]
fn inviting_into_a_missing_space_is_refused() {
    let mut s = Service::new(MemoryStore::default());
    assert!(matches!(s.invite("nope", "bob"), Err(ServiceError::NoSpace(_))));
}

#[test]
fn inviting_twice_is_harmless() {
    let mut s = Service::new(MemoryStore::default());
    s.create_space("personal", &["alice"]).unwrap();
    s.invite("personal", "bob").unwrap();
    s.invite("personal", "bob").unwrap();
    assert!(s.is_member("personal", "bob"));
}
