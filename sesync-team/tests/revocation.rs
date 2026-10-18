//! Revocation by a newer signed list. The revoked device is dropped from the list, so it cannot
//! write new records that others accept. Records written before the revocation stay readable to
//! it, as the operator approved: revocation is forward-only.
use sesync_team::membership::{device_entry, Device, MemberList, Verifier};
use sesync_team::session_keys::{open_session_bound, seal_session_bound, session_aad, unwrap_for, wrap_for, DataKey};
use sesync_team::signed_records::{open_record, seal_record, Binding, RecordError};

fn expect() -> Binding {
    Binding { space: "personal".into(), conversation: "ses_r".into(), min_sequence: 0 }
}

#[test]
fn a_revoked_device_cannot_write_records_accepted_after_revocation() {
    let admin = Device::generate();
    let kept = Device::generate();
    let revoked = Device::generate();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&MemberList::new(1, vec![device_entry(&kept), device_entry(&revoked)]).sign(&admin)).unwrap();

    // Revoke: a newer list without the revoked device.
    v.accept(&MemberList::new(2, vec![device_entry(&kept)]).sign(&admin)).unwrap();

    let after = seal_record(&revoked, 2, 2, "personal", "ses_r", b"sneaky write").unwrap();
    assert_eq!(open_record(&v, &expect(), &after), Err(RecordError::UnknownAuthor));
    let fine = seal_record(&kept, 2, 2, "personal", "ses_r", b"normal write").unwrap();
    assert_eq!(open_record(&v, &expect(), &fine).unwrap().content, b"normal write", "others keep writing");
}

#[test]
fn a_revoked_device_still_reads_what_it_had_before_revocation() {
    let admin = Device::generate();
    let kept = Device::generate();
    let revoked = Device::generate();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&MemberList::new(1, vec![device_entry(&kept), device_entry(&revoked)]).sign(&admin)).unwrap();

    // A session written before revocation is wrapped to the device that was active then.
    let key = DataKey::random();
    let wraps = wrap_for(&key, &[kept.box_public(), revoked.box_public()]).unwrap();
    let aad = session_aad("personal", "ses_r", 1);
    let sealed = seal_session_bound(&key, &aad, b"written before revocation").unwrap();

    // Revoke.
    v.accept(&MemberList::new(2, vec![device_entry(&kept)]).sign(&admin)).unwrap();

    let k = unwrap_for(&revoked, &wraps).expect("the old wrap still opens for the revoked device");
    assert_eq!(open_session_bound(&k, &aad, &sealed).unwrap(), b"written before revocation",
        "forward-only: the past stays readable, as approved");
}

#[test]
fn a_session_written_after_revocation_is_not_wrapped_to_the_revoked_device() {
    let admin = Device::generate();
    let kept = Device::generate();
    let revoked = Device::generate();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&MemberList::new(1, vec![device_entry(&kept), device_entry(&revoked)]).sign(&admin)).unwrap();
    v.accept(&MemberList::new(2, vec![device_entry(&kept)]).sign(&admin)).unwrap();

    // The writer wraps only to the devices in the current list.
    let key = DataKey::random();
    let wraps = wrap_for(&key, &v.box_keys()).unwrap();
    assert!(unwrap_for(&revoked, &wraps).is_err(), "a new session is not openable by the revoked device");
    assert!(unwrap_for(&kept, &wraps).is_ok());
}
