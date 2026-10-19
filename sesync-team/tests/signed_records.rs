//! Signed records, format v2. A record names its space, conversation and sequence, and its author's
//! signature covers all of it. A reader accepts a record only when it is bound to the space and
//! conversation the reader expects, the author is listed, the list version is not older than the
//! reader's, the signature verifies, and the sequence is not below the reader's floor.
use sesync_team::membership::{Device, MemberList, Verifier};
use sesync_team::signed_records::{open_record, seal_record, Binding, Opened, RecordError};

fn setup() -> (Device, Device, Device) {
    (Device::generate(), Device::generate(), Device::generate()) // admin, writer, reader
}

fn list_of(v: u64, admin: &Device, devices: &[&Device]) -> sesync_team::membership::SignedList {
    MemberList::new(v, devices.iter().map(|d| sesync_team::membership::device_entry(d)).collect()).sign(admin)
}

/// The binding a reader uses for `conv` in `space`, with no sequence floor.
fn expect(space: &str, conv: &str) -> Binding {
    Binding { space: space.into(), conversation: conv.into(), min_sequence: 0 }
}

/// Byte offset of the space name in a v2 record: "SR2", author, list version, sequence, space length.
const SPACE_AT: usize = 3 + 32 + 8 + 8 + 2;
/// Byte offset of the sequence field: "SR2", author, list version.
const SEQ_AT: usize = 3 + 32 + 8;

#[test]
fn a_record_from_a_listed_writer_opens_for_a_reader() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    let rec = seal_record(&writer, 1, 7, "personal", "ses_a", b"session text").unwrap();
    let got = open_record(&v, &expect("personal", "ses_a"), &rec).unwrap();
    assert_eq!(got, Opened { content: b"session text".to_vec(), sequence: 7, list_version: 1 });
}

#[test]
fn a_record_from_an_unlisted_writer_is_rejected() {
    let (admin, writer, reader) = setup();
    let outsider = Device::generate();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    let rec = seal_record(&outsider, 1, 1, "personal", "ses_a", b"forged").unwrap();
    assert_eq!(open_record(&v, &expect("personal", "ses_a"), &rec), Err(RecordError::UnknownAuthor));
}

#[test]
fn a_record_sealed_under_an_older_list_is_rejected() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(2, &admin, &[&writer, &reader])).unwrap();
    let stale = seal_record(&writer, 1, 1, "personal", "ses_a", b"old").unwrap();
    assert_eq!(open_record(&v, &expect("personal", "ses_a"), &stale),
        Err(RecordError::StaleList { have: 2, got: 1 }));
}

#[test]
fn a_record_whose_author_was_revoked_is_rejected_after_the_newer_list() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    let before = seal_record(&writer, 1, 1, "personal", "ses_a", b"before revoke").unwrap();
    assert!(open_record(&v, &expect("personal", "ses_a"), &before).is_ok());
    v.accept(&list_of(2, &admin, &[&reader])).unwrap();
    let after = seal_record(&writer, 2, 2, "personal", "ses_a", b"after revoke").unwrap();
    assert_eq!(open_record(&v, &expect("personal", "ses_a"), &after), Err(RecordError::UnknownAuthor),
        "a revoked writer cannot be accepted under the newer list");
}

#[test]
fn a_record_moved_to_another_space_is_refused() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    let rec = seal_record(&writer, 1, 1, "team-a", "ses_a", b"text").unwrap();
    assert_eq!(open_record(&v, &expect("team-b", "ses_a"), &rec), Err(RecordError::WrongSpace),
        "a reader of team-b refuses a record sealed for team-a");
}

#[test]
fn a_space_name_edited_in_the_bytes_fails_the_signature() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    let mut rec = seal_record(&writer, 1, 1, "team-a", "ses_a", b"text").unwrap();
    // Rewrite the space name in place to "team-b", the length the reader expects. The header now
    // matches the reader's binding, so only the signature can refuse it.
    rec[SPACE_AT..SPACE_AT + 6].copy_from_slice(b"team-b");
    assert_eq!(open_record(&v, &expect("team-b", "ses_a"), &rec), Err(RecordError::BadSignature));
}

#[test]
fn a_record_moved_to_another_conversation_is_refused() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    let rec = seal_record(&writer, 1, 1, "personal", "ses_a", b"text").unwrap();
    assert_eq!(open_record(&v, &expect("personal", "ses_b"), &rec), Err(RecordError::WrongConversation));
}

#[test]
fn a_tampered_sequence_fails_its_signature() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    let mut rec = seal_record(&writer, 1, 5, "personal", "ses_a", b"text").unwrap();
    // Lower the sequence from 5 to 4 in the bytes: a rollback attempt by editing, not by replaying.
    rec[SEQ_AT + 7] = 4;
    assert_eq!(open_record(&v, &expect("personal", "ses_a"), &rec), Err(RecordError::BadSignature));
}

#[test]
fn a_record_below_the_readers_sequence_floor_is_a_rollback() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    let old = seal_record(&writer, 1, 5, "personal", "ses_a", b"old text").unwrap();
    let floor = Binding { space: "personal".into(), conversation: "ses_a".into(), min_sequence: 6 };
    assert_eq!(open_record(&v, &floor, &old), Err(RecordError::Rollback { have: 6, got: 5 }),
        "a validly signed but older record is refused once a newer one was seen");
}

#[test]
fn a_record_at_the_readers_sequence_floor_is_accepted() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    let same = seal_record(&writer, 1, 6, "personal", "ses_a", b"same lease").unwrap();
    let floor = Binding { space: "personal".into(), conversation: "ses_a".into(), min_sequence: 6 };
    assert!(open_record(&v, &floor, &same).is_ok(), "equal to the floor is not a rollback");
}

#[test]
fn a_v1_record_is_refused_as_unsupported() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    // The v1 layout: author(32) || version(8) || sig(64) || content. Built by hand, since the
    // current sealer only writes v2.
    let mut v1 = writer.public_key();
    v1.extend_from_slice(&1u64.to_be_bytes());
    v1.extend_from_slice(&[0u8; 64]);
    v1.extend_from_slice(b"old format");
    assert_eq!(open_record(&v, &expect("personal", "ses_a"), &v1), Err(RecordError::UnsupportedFormat));
}

#[test]
fn an_empty_or_foreign_record_is_refused_as_unsupported() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    assert_eq!(open_record(&v, &expect("personal", "ses_a"), b""), Err(RecordError::UnsupportedFormat));
    assert_eq!(open_record(&v, &expect("personal", "ses_a"), b"SR1 something"), Err(RecordError::UnsupportedFormat));
}

#[test]
fn a_truncated_v2_record_is_malformed() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    let rec = seal_record(&writer, 1, 1, "personal", "ses_a", b"text").unwrap();
    assert_eq!(open_record(&v, &expect("personal", "ses_a"), &rec[..SEQ_AT + 4]), Err(RecordError::Malformed));
}

#[test]
fn a_tampered_content_fails_its_signature() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    let mut rec = seal_record(&writer, 1, 1, "personal", "ses_a", b"payload").unwrap();
    let last = rec.len() - 1;
    rec[last] ^= 1;
    assert_eq!(open_record(&v, &expect("personal", "ses_a"), &rec), Err(RecordError::BadSignature));
}

#[test]
fn every_byte_is_covered_so_no_single_flipped_byte_opens() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(1, &admin, &[&writer, &reader])).unwrap();
    let rec = seal_record(&writer, 1, 3, "personal", "ses_a", b"covered content").unwrap();
    assert!(open_record(&v, &expect("personal", "ses_a"), &rec).is_ok(), "the untouched record opens");
    for i in 0..rec.len() {
        let mut bad = rec.clone();
        bad[i] ^= 0x01;
        assert!(open_record(&v, &expect("personal", "ses_a"), &bad).is_err(),
            "flipping byte {i} must not open the record");
    }
}

#[test]
fn a_record_round_trips_through_seal_and_open_with_its_metadata() {
    let (admin, writer, reader) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&list_of(4, &admin, &[&writer, &reader])).unwrap();
    let rec = seal_record(&writer, 4, 9, "personal", "ses_rt", b"").unwrap();
    let got = open_record(&v, &expect("personal", "ses_rt"), &rec).unwrap();
    assert_eq!(got.content, Vec::<u8>::new(), "empty content is allowed");
    assert_eq!((got.sequence, got.list_version), (9, 4));
}
