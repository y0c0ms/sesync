//! The client's seal and open, the code the sync loop calls. A session is sealed for the devices
//! in the team's admin-signed list, signed by the writer, and opened by a listed reader. The list
//! must verify against the pinned admin key, and a session that does not is never returned as
//! text. A record is bound to its space, conversation and sequence: one sealed for another space or
//! conversation is refused, and one below the reader's floor is a rollback.
use sesync::device_keys::{load_or_create, DeviceFile};
use sesync::sealed::{open_for_device, seal_for_devices, SealError, Trust, MAX_SESSION_BYTES};
use sesync_team::membership::{device_entry, Device, MemberList};
use sesync_team::session_keys::{open_session_bound, seal_session_bound, session_aad, unwrap_for, wrap_for, DataKey, Wrap};
use sesync_team::signed_records::{open_record, seal_record, Binding};

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-sealed-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn team_device(d: &DeviceFile) -> Device {
    Device::from_keys(d.signing_pkcs8(), *d.box_secret()).unwrap()
}

/// The trust root a join would have pinned: the admin's key and an admin-signed list.
fn trust(admin: &Device, members: &[&DeviceFile]) -> Trust {
    let entries = members.iter().map(|d| device_entry(&team_device(d))).collect();
    Trust::new(admin.public_key(), MemberList::new(1, entries).sign(admin).to_bytes())
}

/// What a reader expects of a record in `space` and `conv`, with no sequence floor.
fn expect(space: &str, conv: &str) -> Binding {
    Binding { space: space.into(), conversation: conv.into(), min_sequence: 0 }
}

#[test]
fn a_session_seals_and_a_listed_reader_opens_the_exact_text() {
    let w = dir("w"); let r = dir("r");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let rec = seal_for_devices(&writer, &t, "personal", "ses_s", 1, b"the session text").unwrap();
    let opened = open_for_device(&reader, &t, &expect("personal", "ses_s"), &rec).unwrap();
    assert_eq!(opened.content, b"the session text");
    assert_eq!(opened.sequence, 1, "the sequence comes back with the text");
    let _ = std::fs::remove_dir_all(&w); let _ = std::fs::remove_dir_all(&r);
}

#[test]
fn a_session_from_an_unlisted_writer_is_not_opened() {
    let w = dir("uw"); let r = dir("ur");
    let admin = Device::generate();
    let outsider = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&reader]);
    let rec = seal_for_devices(&outsider, &t, "personal", "ses_x", 1, b"x").unwrap();
    let r2 = open_for_device(&reader, &t, &expect("personal", "ses_x"), &rec);
    assert!(matches!(r2, Err(SealError::UnknownAuthor)), "{r2:?}");
    let _ = std::fs::remove_dir_all(&w); let _ = std::fs::remove_dir_all(&r);
}

#[test]
fn a_device_not_in_the_session_wraps_cannot_open_it() {
    let w = dir("nw"); let r = dir("nr"); let late = dir("nl");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let outsider = load_or_create(&late).unwrap();
    // The record is sealed under the list as it stood (writer and reader); the outsider is only
    // added to a newer list afterwards, so there is no wrap addressed to it.
    let old = trust(&admin, &[&writer, &reader]);
    let newer = trust(&admin, &[&writer, &reader, &outsider]);
    let rec = seal_for_devices(&writer, &old, "personal", "ses_p", 1, b"private").unwrap();
    assert!(matches!(open_for_device(&outsider, &newer, &expect("personal", "ses_p"), &rec),
        Err(SealError::NotForThisDevice)));
    for d in [&w, &r, &late] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_list_that_the_pinned_admin_did_not_sign_is_refused() {
    let w = dir("fw"); let r = dir("fr");
    let admin = Device::generate();
    let attacker = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    // The attacker signs a list naming itself: open must refuse it, because the list does not
    // verify against the admin key pinned at join — whoever signs records cannot widen the list.
    let forged = Trust::new(admin.public_key(), trust(&attacker, &[&writer, &reader]).signed_list().to_vec());
    let rec = seal_for_devices(&writer, &trust(&admin, &[&writer, &reader]), "personal", "ses_f", 1, b"x").unwrap();
    let r2 = open_for_device(&reader, &forged, &expect("personal", "ses_f"), &rec);
    assert!(matches!(r2, Err(SealError::BadSignature)), "{r2:?}");
    let _ = std::fs::remove_dir_all(&w); let _ = std::fs::remove_dir_all(&r);
}

#[test]
fn a_record_sealed_for_another_space_is_refused() {
    let w = dir("sw"); let r = dir("sr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let rec = seal_for_devices(&writer, &t, "team-a", "ses_s", 1, b"text").unwrap();
    let r2 = open_for_device(&reader, &t, &expect("team-b", "ses_s"), &rec);
    assert_eq!(r2.map(|o| o.content).unwrap_err(), SealError::WrongSpace);
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_record_sealed_for_another_conversation_is_refused() {
    let w = dir("cw"); let r = dir("cr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let rec = seal_for_devices(&writer, &t, "personal", "ses_one", 1, b"text").unwrap();
    let r2 = open_for_device(&reader, &t, &expect("personal", "ses_two"), &rec);
    assert_eq!(r2.map(|o| o.content).unwrap_err(), SealError::WrongConversation);
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_record_below_the_readers_floor_is_a_rollback() {
    let w = dir("rw"); let r = dir("rr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let old = seal_for_devices(&writer, &t, "personal", "ses_r", 3, b"old version").unwrap();
    let floor = Binding { space: "personal".into(), conversation: "ses_r".into(), min_sequence: 4 };
    assert_eq!(open_for_device(&reader, &t, &floor, &old).map(|o| o.content).unwrap_err(),
        SealError::Rollback { have: 4, got: 3 });
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_v1_record_is_refused_as_unsupported() {
    let w = dir("vw"); let r = dir("vr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    // A v1 record: author || version || sig || content. Refused, never converted.
    let mut v1 = team_device(&writer).public_key();
    v1.extend_from_slice(&1u64.to_be_bytes());
    v1.extend_from_slice(&[0u8; 64]);
    v1.extend_from_slice(b"old");
    assert_eq!(open_for_device(&reader, &t, &expect("personal", "ses_v"), &v1).map(|o| o.content).unwrap_err(),
        SealError::UnsupportedFormat);
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

// Body codec: the session is compressed before the body AEAD. The helpers below open a record the
// way a reader does, but stop before the codec step, so a test can see the tag byte; and they craft
// records with an arbitrary framed body, so a test can feed the reader a body no writer would make.

/// Split the payload the way `seal_for_devices` frames it: wraps, then the body.
fn split_payload(b: &[u8]) -> (Vec<Wrap>, Vec<u8>) {
    let mut i = 0usize;
    let u32_at = |i: &mut usize| {
        let n = u32::from_be_bytes(b[*i..*i + 4].try_into().unwrap()) as usize;
        *i += 4;
        n
    };
    let count = u32_at(&mut i);
    let mut wraps = Vec::new();
    for _ in 0..count {
        let dl = u32_at(&mut i);
        let device = b[i..i + dl].to_vec();
        i += dl;
        let sl = u32_at(&mut i);
        let sealed = b[i..i + sl].to_vec();
        i += sl;
        wraps.push(Wrap { device, sealed });
    }
    (wraps, b[i..].to_vec())
}

/// The framed body (codec byte, then data) as a reader recovers it, before any codec step.
fn framed_body(reader: &DeviceFile, t: &Trust, space: &str, conv: &str, record: &[u8]) -> Vec<u8> {
    let opened = open_record(&t.verifier().unwrap(), &expect(space, conv), record).unwrap();
    let (wraps, body) = split_payload(&opened.content);
    let key = unwrap_for(&team_device(reader), &wraps).unwrap();
    open_session_bound(&key, &session_aad(space, conv, opened.sequence), &body).unwrap()
}

/// A record whose body is `framed` (already codec-tagged), sealed the way a writer seals one.
fn craft(writer: &DeviceFile, t: &Trust, space: &str, conv: &str, sequence: u64, framed: &[u8]) -> Vec<u8> {
    let key = DataKey::random();
    let body = seal_session_bound(&key, &session_aad(space, conv, sequence), framed).unwrap();
    let boxes = t.box_keys().unwrap();
    let wraps = wrap_for(&key, &boxes).unwrap();
    let mut payload = (wraps.len() as u32).to_be_bytes().to_vec();
    for w in &wraps {
        payload.extend_from_slice(&(w.device.len() as u32).to_be_bytes());
        payload.extend_from_slice(&w.device);
        payload.extend_from_slice(&(w.sealed.len() as u32).to_be_bytes());
        payload.extend_from_slice(&w.sealed);
    }
    payload.extend_from_slice(&body);
    seal_record(&team_device(writer), t.version().unwrap(), sequence, space, conv, &payload).unwrap()
}

/// About `size` bytes of journal-like JSON lines: the shape a real session has, with repeated keys
/// and varying text.
fn journal(size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0u64;
    while out.len() < size {
        let line = format!(
            "{{\"type\":\"message\",\"role\":\"assistant\",\"index\":{i},\"model\":\"example-model\",\
\"content\":[{{\"type\":\"text\",\"text\":\"step {} of the refactor: the parser reads the journal \
and writes the canonical form\"}}],\"usage\":{{\"input_tokens\":{},\"output_tokens\":{}}}}}\n",
            i % 17, 1000 + i % 97, 200 + i % 31);
        out.extend_from_slice(line.as_bytes());
        i += 1;
    }
    out
}

#[test]
fn a_session_round_trips_exactly_when_compressed() {
    let w = dir("zw"); let r = dir("zr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let plain = journal(200 * 1024);
    let rec = seal_for_devices(&writer, &t, "personal", "ses_z", 1, &plain).unwrap();
    let opened = open_for_device(&reader, &t, &expect("personal", "ses_z"), &rec).unwrap();
    assert_eq!(opened.content, plain, "the exact bytes come back after compress, encrypt, decrypt, inflate");
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_multi_megabyte_session_round_trips_under_the_decoder_window_limit() {
    // Larger than the zstd window, so the decoder's window bound is exercised across blocks.
    let w = dir("mmw"); let r = dir("mmr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let plain = journal(8 * 1024 * 1024 + 12345);
    let rec = seal_for_devices(&writer, &t, "personal", "ses_mm", 1, &plain).unwrap();
    assert_eq!(open_for_device(&reader, &t, &expect("personal", "ses_mm"), &rec).unwrap().content, plain);
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_repetitive_journal_seals_to_well_under_a_quarter_of_its_size() {
    let w = dir("ratw"); let r = dir("ratr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let plain = journal(200 * 1024);
    let rec = seal_for_devices(&writer, &t, "personal", "ses_r", 1, &plain).unwrap();
    eprintln!("200 KB journal: {} bytes plain, {} bytes sealed, ratio {:.4}",
        plain.len(), rec.len(), rec.len() as f64 / plain.len() as f64);
    assert!(rec.len() * 4 < plain.len(), "sealed {} of {} plain bytes", rec.len(), plain.len());
    assert_eq!(framed_body(&reader, &t, "personal", "ses_r", &rec)[0], 0x01, "the zstd codec was used");
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn incompressible_bytes_are_stored_raw_with_tag_zero_and_round_trip() {
    let w = dir("rndw"); let r = dir("rndr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let mut plain = vec![0u8; 64 * 1024];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut plain).unwrap();
    let rec = seal_for_devices(&writer, &t, "personal", "ses_rnd", 1, &plain).unwrap();
    let framed = framed_body(&reader, &t, "personal", "ses_rnd", &rec);
    assert_eq!(framed[0], 0x00, "random bytes do not compress, so they are stored");
    assert_eq!(framed.len(), 1 + plain.len(), "stored: the codec byte and the bytes, nothing else");
    let opened = open_for_device(&reader, &t, &expect("personal", "ses_rnd"), &rec).unwrap();
    assert_eq!(opened.content, plain);
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn an_empty_session_round_trips() {
    let w = dir("emw"); let r = dir("emr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let rec = seal_for_devices(&writer, &t, "personal", "ses_e", 1, b"").unwrap();
    assert_eq!(open_for_device(&reader, &t, &expect("personal", "ses_e"), &rec).unwrap().content, b"".to_vec());
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn an_unknown_codec_tag_is_refused_as_malformed() {
    let w = dir("ctw"); let r = dir("ctr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let rec = craft(&writer, &t, "personal", "ses_ct", 1, &[0x02, b'x', b'y']);
    let r2 = open_for_device(&reader, &t, &expect("personal", "ses_ct"), &rec);
    assert_eq!(r2.map(|o| o.content).unwrap_err(), SealError::Malformed);
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_corrupt_zstd_body_is_refused_as_malformed() {
    let w = dir("cxw"); let r = dir("cxr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let rec = craft(&writer, &t, "personal", "ses_cx", 1, &[0x01, 0xde, 0xad, 0xbe, 0xef]);
    let r2 = open_for_device(&reader, &t, &expect("personal", "ses_cx"), &rec);
    assert_eq!(r2.map(|o| o.content).unwrap_err(), SealError::Malformed);
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_zstd_bomb_is_refused_before_it_can_exhaust_memory() {
    // 100 MB of zeros compresses to a few kilobytes. Opening it must stop at the 64 MiB limit with
    // an error, not inflate the whole 100 MB.
    let w = dir("bw"); let r = dir("br");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let zeros = vec![0u8; 100 * 1024 * 1024];
    let packed = zstd::stream::encode_all(&zeros[..], 3).unwrap();
    drop(zeros);
    assert!(packed.len() < 1024 * 1024, "the bomb is small on the wire: {} bytes", packed.len());
    let mut framed = vec![0x01u8];
    framed.extend_from_slice(&packed);
    let rec = craft(&writer, &t, "personal", "ses_bomb", 1, &framed);
    let r2 = open_for_device(&reader, &t, &expect("personal", "ses_bomb"), &rec);
    assert_eq!(r2.map(|o| o.content).unwrap_err(), SealError::TooLarge);
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_session_over_the_size_limit_is_refused_on_seal() {
    let w = dir("ow"); let r = dir("or");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let too_big = vec![b'a'; MAX_SESSION_BYTES + 1];
    let r2 = seal_for_devices(&writer, &t, "personal", "ses_big", 1, &too_big);
    assert_eq!(r2.unwrap_err(), SealError::TooLarge, "a writer never makes a record its readers refuse");
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}
