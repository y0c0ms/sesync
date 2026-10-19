//! The clipboard module: Ctrl+C copies the selected session's path with the OSC52 escape
//! sequence, so no extra crate is needed and the copy works even through ssh sessions.

use sesync::clipboard::{b64, osc52_payload};

#[test]
fn base64_matches_the_standard_vectors() {
    assert_eq!(b64(b""), "");
    assert_eq!(b64(b"f"), "Zg==");
    assert_eq!(b64(b"fo"), "Zm8=");
    assert_eq!(b64(b"foo"), "Zm9v");
    assert_eq!(b64(b"foob"), "Zm9vYg==");
    assert_eq!(b64(b"fooba"), "Zm9vYmE=");
    assert_eq!(b64(b"foobar"), "Zm9vYmFy");
}

#[test]
fn the_payload_is_a_well_formed_osc52_write_to_the_system_clipboard() {
    let p = osc52_payload("some/path");
    assert!(p.starts_with("\x1b]52;c;"), "{p:?}");
    assert!(p.ends_with("\x07"), "{p:?}");
    assert!(p.contains("c29tZS9wYXRo"), "{p:?}");
}
