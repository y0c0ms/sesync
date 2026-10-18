//! Per-session data keys. A session is sealed under a random data key; that key is wrapped to
//! each active device. Only a listed device can unwrap it. A device that is not listed gets
//! nothing it can use, and a wrapped key cannot be altered without detection.
use sesync_team::membership::Device;
use sesync_team::session_keys::{open_session_bound, seal_session_bound, session_aad, unwrap_for, wrap_for, DataKey, WrapError};

fn fresh_devices(n: usize) -> Vec<Device> { (0..n).map(|_| Device::generate()).collect() }

#[test]
fn a_session_opens_for_every_listed_device() {
    let devs = fresh_devices(3);
    let pubs: Vec<Vec<u8>> = devs.iter().map(|d| d.box_public()).collect();
    let key = DataKey::random();
    let wraps = wrap_for(&key, &pubs).unwrap();
    let aad = session_aad("personal", "ses_1", 1);
    let sealed = seal_session_bound(&key, &aad, b"session text").unwrap();
    for d in &devs {
        let k = unwrap_for(d, &wraps).expect("a listed device unwraps");
        assert_eq!(open_session_bound(&k, &aad, &sealed).unwrap(), b"session text");
    }
}

#[test]
fn an_unlisted_device_cannot_unwrap() {
    let listed = fresh_devices(2);
    let outsider = Device::generate();
    let pubs: Vec<Vec<u8>> = listed.iter().map(|d| d.box_public()).collect();
    let key = DataKey::random();
    let wraps = wrap_for(&key, &pubs).unwrap();
    assert!(matches!(unwrap_for(&outsider, &wraps), Err(WrapError::NotForThisDevice)));
}

#[test]
fn a_tampered_wrap_is_refused() {
    let d = Device::generate();
    let key = DataKey::random();
    let mut wraps = wrap_for(&key, &[d.box_public()]).unwrap();
    let last = wraps[0].sealed.len() - 1;
    wraps[0].sealed[last] ^= 1;
    assert!(unwrap_for(&d, &wraps).is_err(), "a changed wrapped key does not open");
}

#[test]
fn each_session_gets_its_own_key() {
    let a = DataKey::random();
    let b = DataKey::random();
    assert_ne!(a.as_bytes(), b.as_bytes(), "keys are random per session");
}

#[test]
fn a_session_does_not_open_under_another_session_key() {
    let a = DataKey::random();
    let b = DataKey::random();
    let aad = session_aad("personal", "ses_1", 1);
    let sealed = seal_session_bound(&a, &aad, b"private").unwrap();
    assert!(open_session_bound(&b, &aad, &sealed).is_err());
}

#[test]
fn a_session_does_not_open_under_different_associated_data() {
    // The session is bound to its space, conversation and sequence. Moving it to any other
    // binding, even with the right key, must fail.
    let key = DataKey::random();
    let sealed = seal_session_bound(&key, &session_aad("personal", "ses_1", 1), b"bound text").unwrap();
    assert_eq!(open_session_bound(&key, &session_aad("personal", "ses_1", 1), &sealed).unwrap(), b"bound text");
    assert!(open_session_bound(&key, &session_aad("team-b", "ses_1", 1), &sealed).is_err(), "another space");
    assert!(open_session_bound(&key, &session_aad("personal", "ses_2", 1), &sealed).is_err(), "another conversation");
    assert!(open_session_bound(&key, &session_aad("personal", "ses_1", 2), &sealed).is_err(), "another sequence");
    assert!(open_session_bound(&key, b"", &sealed).is_err(), "no associated data");
}

#[test]
fn session_aad_length_prefixes_so_names_cannot_shift_into_each_other() {
    // ("ab", "c") and ("a", "bc") must not produce the same associated data.
    assert_ne!(session_aad("ab", "c", 1), session_aad("a", "bc", 1));
}
