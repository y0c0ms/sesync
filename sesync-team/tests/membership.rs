//! Device keys and the admin-signed member list. A member list is accepted only when the admin
//! signed it, and never replaces a newer version with an older one. A device key signs its own
//! records; a signature verifies only with that device's public key.
use sesync_team::membership::{Device, MemberList, MembershipError, Verifier};

fn list(version: u64, devices: &[&Device]) -> MemberList {
    MemberList::new(version, devices.iter().map(|d| sesync_team::membership::device_entry(d)).collect())
}

#[test]
fn a_device_signature_verifies_only_for_that_device() {
    let a = Device::generate();
    let b = Device::generate();
    let sig = a.sign(b"record bytes");
    assert!(Device::verify(&a.public_key(), b"record bytes", &sig), "its own key verifies");
    assert!(!Device::verify(&b.public_key(), b"record bytes", &sig), "another device's key does not");
    assert!(!Device::verify(&a.public_key(), b"record bytes changed", &sig), "a changed record does not");
}

#[test]
fn a_list_signed_by_the_admin_is_accepted() {
    let admin = Device::generate();
    let a = Device::generate();
    let signed = list(1, &[&admin, &a]).sign(&admin);
    let mut v = Verifier::new(admin.public_key());
    assert!(v.accept(&signed).is_ok());
    assert!(v.is_member(&a.public_key()));
}

#[test]
fn a_list_not_signed_by_the_admin_is_refused() {
    let admin = Device::generate();
    let mallory = Device::generate();
    let a = Device::generate();
    let forged = list(1, &[&a, &mallory]).sign(&mallory);
    let mut v = Verifier::new(admin.public_key());
    assert_eq!(v.accept(&forged), Err(MembershipError::BadSignature));
    assert!(!v.is_member(&a.public_key()));
}

#[test]
fn an_older_list_never_replaces_a_newer_one() {
    let admin = Device::generate();
    let a = Device::generate();
    let b = Device::generate();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list(2, &[&admin, &a]).sign(&admin)).unwrap();
    // A replayed version-1 list that still names b must not bring b back.
    let stale = list(1, &[&admin, &a, &b]).sign(&admin);
    assert_eq!(v.accept(&stale), Err(MembershipError::Stale { have: 2, got: 1 }));
    assert!(!v.is_member(&b.public_key()), "the stale list is ignored");
}

#[test]
fn a_revoked_device_is_not_a_member_in_the_newer_list() {
    let admin = Device::generate();
    let a = Device::generate();
    let b = Device::generate();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list(1, &[&admin, &a, &b]).sign(&admin)).unwrap();
    assert!(v.is_member(&b.public_key()));
    v.accept(&list(2, &[&admin, &a]).sign(&admin)).unwrap();
    assert!(!v.is_member(&b.public_key()), "b is gone once the admin signs a newer list without it");
}

#[test]
fn a_list_survives_a_round_trip_through_bytes() {
    let admin = Device::generate();
    let a = Device::generate();
    let signed = list(3, &[&admin, &a]).sign(&admin);
    let bytes = signed.to_bytes();
    let back = sesync_team::membership::SignedList::from_bytes(&bytes).unwrap();
    let mut v = Verifier::new(admin.public_key());
    assert!(v.accept(&back).is_ok(), "the bytes parse back to a list that verifies");
}
