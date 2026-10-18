//! The verified pull: fetch a team's sealed records, keep only those a listed author signed and a
//! listed device can open, and return them for display. It never writes a file, and a record that
//! fails any check is dropped and counted, never shown as text. A record older than the sequence
//! this device already accepted for its conversation is a rollback: dropped and counted as such.
use std::cell::Cell;
use std::collections::BTreeMap;

use sesync::device_keys::{load_or_create, DeviceFile};
use sesync::sealed::{seal_for_devices, Trust};
use sesync::team_pull_verified::{pull_verified, pull_verified_all, PulledRecord, RecordSource};
use sesync_team::membership::{device_entry, Device, MemberList};
use sesync_team::signed_records::sequence_of;

struct Fake { records: Vec<(String, Vec<u8>)> }

impl RecordSource for Fake {
    fn space(&self) -> &str { "personal" }
    fn list(&self) -> Result<Vec<String>, String> { Ok(self.records.iter().map(|(c, _)| c.clone()).collect()) }
    fn fetch(&self, conv: &str) -> Result<Vec<u8>, String> {
        self.records.iter().find(|(c, _)| c == conv).map(|(_, b)| b.clone()).ok_or_else(|| "missing".into())
    }
}

/// A source that counts its fetches. It advertises each stored record's sequence, as the server
/// does, unless `advertise` is off, and it can be told to advertise a fixed (possibly false) value.
struct Counting {
    records: Vec<(String, Vec<u8>)>,
    advertise: bool,
    lie: Option<u64>,
    fetches: Cell<usize>,
}

impl Counting {
    fn new(records: Vec<(String, Vec<u8>)>) -> Counting {
        Counting { records, advertise: true, lie: None, fetches: Cell::new(0) }
    }
}

impl RecordSource for Counting {
    fn space(&self) -> &str { "personal" }
    fn list(&self) -> Result<Vec<String>, String> { Ok(self.records.iter().map(|(c, _)| c.clone()).collect()) }
    fn list_advertised(&self) -> Result<Vec<(String, Option<u64>)>, String> {
        Ok(self.records.iter().map(|(c, rec)| {
            let adv = match (self.advertise, self.lie) {
                (false, _) => None,
                (true, Some(v)) => Some(v),
                (true, None) => sequence_of(rec),
            };
            (c.clone(), adv)
        }).collect())
    }
    fn fetch(&self, conv: &str) -> Result<Vec<u8>, String> {
        self.fetches.set(self.fetches.get() + 1);
        self.records.iter().find(|(c, _)| c == conv).map(|(_, b)| b.clone()).ok_or_else(|| "missing".into())
    }
}

/// The floors a device holds after accepting what `out` accepted: the caller keeps the maximum.
fn floors_after(out: &sesync::team_pull_verified::PullOutcome, mut floors: BTreeMap<String, u64>) -> BTreeMap<String, u64> {
    for (conv, seq) in &out.accepted {
        let f = floors.entry(conv.clone()).or_insert(0);
        *f = (*f).max(*seq);
    }
    floors
}

fn no_floor() -> BTreeMap<String, u64> { BTreeMap::new() }

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-pullv-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn team_device(d: &DeviceFile) -> Device {
    Device::from_keys(d.signing_pkcs8(), *d.box_secret()).unwrap()
}

fn trust(admin: &Device, members: &[&DeviceFile]) -> Trust {
    let entries = members.iter().map(|d| device_entry(&team_device(d))).collect();
    Trust::new(admin.public_key(), MemberList::new(1, entries).sign(admin).to_bytes())
}

#[test]
fn a_verified_record_is_returned_for_display() {
    let w = dir("w"); let r = dir("r");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let rec = seal_for_devices(&writer, &t, "personal", "ses_1", 1, b"the text").unwrap();
    let src = Fake { records: vec![("ses_1".into(), rec)] };
    let out = pull_verified(&src, &reader, Some(&t), &no_floor());
    assert_eq!(out.shown.len(), 1);
    assert_eq!(out.shown[0], PulledRecord { conversation: "ses_1".into(), text: b"the text".to_vec() });
    assert_eq!(out.dropped, 0);
    assert_eq!(out.rolled_back, 0);
    assert_eq!(out.accepted.get("ses_1"), Some(&1), "the accepted sequence is reported");
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_record_from_an_unlisted_author_is_dropped_and_counted() {
    let w = dir("uw"); let r = dir("ur");
    let admin = Device::generate();
    let outsider = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&reader]);
    let rec = seal_for_devices(&outsider, &t, "personal", "ses_bad", 1, b"x").unwrap();
    let src = Fake { records: vec![("ses_bad".into(), rec)] };
    let out = pull_verified(&src, &reader, Some(&t), &no_floor());
    assert!(out.shown.is_empty(), "nothing shown from an unlisted author");
    assert_eq!(out.dropped, 1);
    assert_eq!(out.rolled_back, 0, "an unlisted author is not a rollback");
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn without_a_trust_root_everything_is_dropped_and_counted() {
    let w = dir("ntw"); let r = dir("ntr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let rec = seal_for_devices(&writer, &t, "personal", "ses_x", 1, b"x").unwrap();
    let src = Fake { records: vec![("ses_x".into(), rec)] };
    let out = pull_verified(&src, &reader, None, &no_floor());
    assert!(out.shown.is_empty(), "nothing verifies without a pinned list");
    assert_eq!(out.dropped, 1);
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_tampered_record_is_dropped_never_shown() {
    let w = dir("tw"); let r = dir("tr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let mut rec = seal_for_devices(&writer, &t, "personal", "ses_t", 1, b"text").unwrap();
    let last = rec.len() - 1;
    rec[last] ^= 1;
    let src = Fake { records: vec![("ses_t".into(), rec)] };
    let out = pull_verified(&src, &reader, Some(&t), &no_floor());
    assert!(out.shown.is_empty());
    assert_eq!(out.dropped, 1);
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_record_below_the_last_seen_sequence_is_dropped_and_counted_as_rolled_back() {
    let w = dir("rbw"); let r = dir("rbr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    // The server replays an older, validly signed version of the session: sequence 2.
    let old = seal_for_devices(&writer, &t, "personal", "ses_rb", 2, b"old version").unwrap();
    let src = Fake { records: vec![("ses_rb".into(), old)] };
    let mut seen = BTreeMap::new();
    seen.insert("ses_rb".to_string(), 3u64);
    let out = pull_verified(&src, &reader, Some(&t), &seen);
    assert!(out.shown.is_empty(), "a rolled-back session is never shown");
    assert_eq!(out.dropped, 1, "it is counted as dropped");
    assert_eq!(out.rolled_back, 1, "and as a rollback");
    assert!(out.accepted.is_empty(), "nothing is accepted, so the floor does not move");
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_record_at_or_above_the_last_seen_sequence_is_shown_and_its_sequence_accepted() {
    let w = dir("okw"); let r = dir("okr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let rec = seal_for_devices(&writer, &t, "personal", "ses_ok", 5, b"newer").unwrap();
    let src = Fake { records: vec![("ses_ok".into(), rec)] };
    let mut seen = BTreeMap::new();
    seen.insert("ses_ok".to_string(), 5u64);
    let out = pull_verified(&src, &reader, Some(&t), &seen);
    assert_eq!(out.shown.len(), 1, "equal to the floor is the same lease, so it is shown");
    assert_eq!(out.rolled_back, 0);
    assert_eq!(out.accepted.get("ses_ok"), Some(&5));
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_record_moved_to_another_conversation_id_is_dropped() {
    let w = dir("mvw"); let r = dir("mvr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    // Sealed for ses_one, then served under ses_two by the server.
    let rec = seal_for_devices(&writer, &t, "personal", "ses_one", 1, b"text").unwrap();
    let src = Fake { records: vec![("ses_two".into(), rec)] };
    let out = pull_verified(&src, &reader, Some(&t), &no_floor());
    assert!(out.shown.is_empty(), "a record does not change conversation");
    assert_eq!(out.dropped, 1);
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn the_pull_writes_no_file() {
    let w = dir("nw"); let r = dir("nr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let rec = seal_for_devices(&writer, &t, "personal", "ses_1", 1, b"text").unwrap();
    let src = Fake { records: vec![("ses_1".into(), rec)] };
    let _ = pull_verified(&src, &reader, Some(&t), &no_floor());
    let count = std::fs::read_dir(&r).unwrap().count();
    assert_eq!(count, 1, "only the device key file exists; the pull wrote nothing else");
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_second_pull_with_the_same_floors_fetches_nothing_and_counts_every_conversation_unchanged() {
    let w = dir("incw"); let r = dir("incr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let a = seal_for_devices(&writer, &t, "personal", "ses_a", 1, b"a text").unwrap();
    let b = seal_for_devices(&writer, &t, "personal", "ses_b", 4, b"b text").unwrap();
    let src = Counting::new(vec![("ses_a".into(), a), ("ses_b".into(), b)]);

    let first = pull_verified(&src, &reader, Some(&t), &no_floor());
    assert_eq!(first.shown.len(), 2, "the first pull has no floors, so it fetches and shows both");
    assert_eq!(src.fetches.get(), 2);
    let floors = floors_after(&first, no_floor());

    let second = pull_verified(&src, &reader, Some(&t), &floors);
    assert_eq!(src.fetches.get(), 2, "the second pull performed zero fetches");
    assert_eq!(second.unchanged, 2, "both conversations were skipped as unchanged");
    assert!(second.shown.is_empty(), "a skipped conversation is not shown again by the daemon's pull");
    assert_eq!(second.dropped, 0, "and it is not dropped either");
    assert!(second.accepted.is_empty(), "nothing new was accepted, so the floors do not move");
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_conversation_with_no_floor_is_still_fetched_even_when_others_are_skipped() {
    let w = dir("nfw"); let r = dir("nfr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let a = seal_for_devices(&writer, &t, "personal", "ses_a", 2, b"a").unwrap();
    let b = seal_for_devices(&writer, &t, "personal", "ses_b", 2, b"b").unwrap();
    let src = Counting::new(vec![("ses_a".into(), a), ("ses_b".into(), b)]);
    let mut floors = BTreeMap::new();
    floors.insert("ses_a".to_string(), 2u64);
    let out = pull_verified(&src, &reader, Some(&t), &floors);
    assert_eq!(out.unchanged, 1, "ses_a has a floor at its advertised sequence: skipped");
    assert_eq!(src.fetches.get(), 1, "only ses_b, which has no floor, was fetched");
    assert_eq!(out.shown, vec![PulledRecord { conversation: "ses_b".into(), text: b"b".to_vec() }]);
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_conversation_is_fetched_when_the_source_advertises_no_sequence() {
    let w = dir("naw"); let r = dir("nar");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let a = seal_for_devices(&writer, &t, "personal", "ses_a", 3, b"a").unwrap();
    let mut src = Counting::new(vec![("ses_a".into(), a)]);
    src.advertise = false;
    let mut floors = BTreeMap::new();
    floors.insert("ses_a".to_string(), 3u64);
    let out = pull_verified(&src, &reader, Some(&t), &floors);
    assert_eq!(src.fetches.get(), 1, "no advertised value means no skip");
    assert_eq!(out.unchanged, 0);
    assert_eq!(out.shown.len(), 1, "fetched, verified and shown as before");
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn an_advertised_sequence_above_the_floor_is_fetched_and_its_record_still_verified() {
    // The server claims a newer record than the one it holds. The device fetches it, the fetched
    // record is the old one, and the rollback check refuses it: the advertisement bought a fetch,
    // not an acceptance.
    let w = dir("hw"); let r = dir("hr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let old = seal_for_devices(&writer, &t, "personal", "ses_h", 2, b"old").unwrap();
    let mut src = Counting::new(vec![("ses_h".into(), old)]);
    src.lie = Some(9);
    let mut floors = BTreeMap::new();
    floors.insert("ses_h".to_string(), 3u64);
    let out = pull_verified(&src, &reader, Some(&t), &floors);
    assert_eq!(src.fetches.get(), 1, "9 is above the floor 3, so the record is fetched");
    assert!(out.shown.is_empty(), "the fetched record is verified and refused as a rollback");
    assert_eq!((out.dropped, out.rolled_back, out.unchanged), (1, 1, 0));
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn a_lying_low_advertisement_can_only_skip_a_fetch_never_show_an_unverified_record() {
    // The server advertises a sequence at the floor for a record it does not actually hold as
    // described. The device skips it: nothing is shown, nothing is dropped, nothing is accepted.
    let w = dir("lw"); let r = dir("lr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let forged = seal_for_devices(&writer, &t, "personal", "ses_l", 5, b"newer").unwrap();
    let mut src = Counting::new(vec![("ses_l".into(), forged)]);
    src.lie = Some(1);
    let mut floors = BTreeMap::new();
    floors.insert("ses_l".to_string(), 4u64);
    let out = pull_verified(&src, &reader, Some(&t), &floors);
    assert_eq!(src.fetches.get(), 0, "a low advertisement skips the fetch");
    assert_eq!(out.unchanged, 1);
    assert!(out.shown.is_empty() && out.dropped == 0 && out.accepted.is_empty());
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn without_a_trust_root_a_skipped_candidate_is_still_dropped_and_counted() {
    let w = dir("ntw2"); let r = dir("ntr2");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let a = seal_for_devices(&writer, &t, "personal", "ses_n", 1, b"x").unwrap();
    let src = Counting::new(vec![("ses_n".into(), a)]);
    let mut floors = BTreeMap::new();
    floors.insert("ses_n".to_string(), 1u64);
    let out = pull_verified(&src, &reader, None, &floors);
    assert_eq!((out.dropped, out.unchanged), (1, 0), "nothing verifies without a pinned list");
    assert!(out.shown.is_empty());
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}

#[test]
fn the_full_pull_fetches_every_conversation_even_when_the_floors_would_skip_it() {
    // The app's refresh replaces its whole list with what the pull shows, so it asks for the full set.
    let w = dir("fpw"); let r = dir("fpr");
    let admin = Device::generate();
    let writer = load_or_create(&w).unwrap();
    let reader = load_or_create(&r).unwrap();
    let t = trust(&admin, &[&writer, &reader]);
    let a = seal_for_devices(&writer, &t, "personal", "ses_f1", 1, b"one").unwrap();
    let b = seal_for_devices(&writer, &t, "personal", "ses_f2", 1, b"two").unwrap();
    let src = Counting::new(vec![("ses_f1".into(), a), ("ses_f2".into(), b)]);
    let mut floors = BTreeMap::new();
    floors.insert("ses_f1".to_string(), 1u64);
    floors.insert("ses_f2".to_string(), 1u64);
    let out = pull_verified_all(&src, &reader, Some(&t), &floors);
    assert_eq!(src.fetches.get(), 2, "both fetched");
    assert_eq!(out.unchanged, 0);
    assert_eq!(out.shown.len(), 2, "both shown, so the list is complete");
    for d in [&w, &r] { let _ = std::fs::remove_dir_all(d); }
}
