//! The sealed write is lease-checked, like the plaintext write it replaces: a record is stored
//! only from the machine holding the conversation's lease, under its current version. The record's
//! sequence is the lease version it was sealed under.
use sesync_team::membership::{device_entry, Device, MemberList, Verifier};
use sesync_team::service::{MemoryStore, Service, ServiceError};
use sesync_team::session_keys::{seal_session_bound, session_aad, DataKey};
use sesync_team::signed_records::seal_record;

const T0: u64 = 1_000_020;

fn setup() -> (Service<MemoryStore>, Device, Device) {
    let admin = Device::generate();
    let writer = Device::generate();
    let mut svc = Service::new(MemoryStore::default());
    svc.create_space("personal", &["writer"]).unwrap();
    svc.pin_admin_key("personal", &admin.public_key()).unwrap();
    svc.store_trust_list("personal", &MemberList::new(1, vec![device_entry(&admin), device_entry(&writer)]).sign(&admin).to_bytes()).unwrap();
    (svc, admin, writer)
}

/// A record for `ses_l` in the personal space, sealed by `writer` at `sequence`.
fn record_for(writer: &Device, sequence: u64) -> Vec<u8> {
    let key = DataKey::random();
    let body = seal_session_bound(&key, &session_aad("personal", "ses_l", sequence), b"x").unwrap();
    seal_record(writer, 1, sequence, "personal", "ses_l", &body).unwrap()
}

#[test]
fn the_lease_holder_can_store_a_sealed_record() {
    let (mut svc, _admin, writer) = setup();
    let lease = svc.acquire("personal", "ses_l", "box-w", T0).unwrap();
    assert!(svc.put_sealed_leased("personal", "ses_l", "box-w", lease.version, record_for(&writer, lease.version)).is_ok());
}

#[test]
fn a_machine_without_the_lease_cannot_store_a_sealed_record() {
    let (mut svc, _admin, writer) = setup();
    let _lease = svc.acquire("personal", "ses_l", "box-w", T0).unwrap();
    let r = svc.put_sealed_leased("personal", "ses_l", "box-other", 1, record_for(&writer, 1));
    assert!(matches!(r, Err(ServiceError::Lease(_))), "another machine is refused: {r:?}");
}

#[test]
fn a_stale_lease_version_is_refused() {
    let (mut svc, _admin, writer) = setup();
    let lease = svc.acquire("personal", "ses_l", "box-w", T0).unwrap();
    let r = svc.put_sealed_leased("personal", "ses_l", "box-w", lease.version + 5, record_for(&writer, lease.version + 5));
    assert!(matches!(r, Err(ServiceError::Lease(_))), "a version the lease did not grant is refused");
}

#[test]
fn an_unlisted_author_is_refused_even_with_the_lease() {
    let (mut svc, _admin, _writer) = setup();
    let outsider = Device::generate();
    let lease = svc.acquire("personal", "ses_l", "box-w", T0).unwrap();
    let r = svc.put_sealed_leased("personal", "ses_l", "box-w", lease.version, record_for(&outsider, lease.version));
    assert!(r.is_err(), "the lease does not make an unlisted author acceptable");
    let _ = Verifier::new(Device::generate().public_key());
}

#[test]
fn a_record_bound_to_another_conversation_is_refused_even_with_the_lease() {
    // A record sealed for ses_other cannot be written into ses_l: the server checks the binding.
    let (mut svc, _admin, writer) = setup();
    let lease = svc.acquire("personal", "ses_l", "box-w", T0).unwrap();
    let body = seal_session_bound(&DataKey::random(), &session_aad("personal", "ses_other", 1), b"x").unwrap();
    let moved = seal_record(&writer, 1, 1, "personal", "ses_other", &body).unwrap();
    let r = svc.put_sealed_leased("personal", "ses_l", "box-w", lease.version, moved);
    assert!(matches!(r, Err(ServiceError::NotMember(_))), "a record for another conversation is refused: {r:?}");
    assert!(svc.raw_stored("personal", "ses_l").is_none(), "nothing was stored");
}

#[test]
fn a_record_whose_sequence_is_not_the_granted_version_is_refused() {
    // The writer cannot choose the sequence: a record claiming a higher sequence than the lease
    // granted would raise every reader's floor past the legitimate records after it.
    let (mut svc, _admin, writer) = setup();
    let lease = svc.acquire("personal", "ses_l", "box-w", T0).unwrap();
    let inflated = record_for(&writer, lease.version + 1_000);
    let r = svc.put_sealed_leased("personal", "ses_l", "box-w", lease.version, inflated);
    assert!(matches!(r, Err(ServiceError::Lease(_))), "a sequence other than the granted one is refused: {r:?}");
    assert!(svc.raw_stored("personal", "ses_l").is_none(), "nothing was stored");
}

#[test]
fn versions_keep_rising_across_releases_and_continue_above_the_stored_record_after_a_restart() {
    let (mut svc, _admin, writer) = setup();
    // Four push cycles: take the lease, write under it, release. Each grant must be higher.
    let mut last = 0;
    for _ in 0..4 {
        let lease = svc.acquire("personal", "ses_l", "box-w", T0).unwrap();
        assert!(lease.version > last, "a released version is never granted again");
        last = lease.version;
        svc.put_sealed_leased("personal", "ses_l", "box-w", lease.version, record_for(&writer, lease.version)).unwrap();
        svc.release("personal", "ses_l", "box-w").unwrap();
    }
    assert_eq!(last, 4);

    // The server restarts: the lease table is empty. The next grant must still be above the stored
    // record's sequence, or readers that already accepted sequence 4 would refuse it as a rollback.
    let store = svc.into_store();
    let mut restarted = sesync_team::service::Service::new(store);
    let lease = restarted.acquire("personal", "ses_l", "box-w", T0 + 1).unwrap();
    assert_eq!(lease.version, 5, "the version continues after the restart");
    restarted.put_sealed_leased("personal", "ses_l", "box-w", lease.version, record_for(&writer, 5)).unwrap();
}
