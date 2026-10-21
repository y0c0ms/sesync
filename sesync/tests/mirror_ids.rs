//! Mirror ids must match the Python daemon exactly. A different id would create a
//! second copy of every mirror instead of replacing the existing one.
//! Expected values were computed by daemon/sesync.py mirror_uuid and
//! mirror_opencode_id.

use sesync::harness::opencode::mirror_opencode_id;
use sesync::mirror::{file_hash_bytes, mirror_uuid};

#[test]
fn mirror_uuid_matches_python() {
    assert_eq!(mirror_uuid("omp:/x/a.jsonl", "pi"), "0457af91-9e73-5b82-87b0-cb427c4a8bfe");
    assert_eq!(mirror_uuid("ses_9", "claude"), "a24598d3-6923-5c5f-af09-eb7740bb9bc1");
}

#[test]
fn mirror_uuid_depends_on_source_and_target() {
    assert_ne!(mirror_uuid("ses_9", "pi"), mirror_uuid("ses_9", "omp"));
    assert_ne!(mirror_uuid("ses_9", "pi"), mirror_uuid("ses_8", "pi"));
}

#[test]
fn mirror_opencode_id_matches_python() {
    assert_eq!(mirror_opencode_id("omp:/x/a.jsonl"), "ses_b89c66c686fbkxuzMcZX56hltq");
    assert_eq!(mirror_opencode_id("ses_9"), "ses_fbb9c9a9ea9eH4GF27BbnBdMPd");
}

#[test]
fn mirror_opencode_id_has_the_native_shape() {
    let id = mirror_opencode_id("anything");
    assert_eq!(id.len(), 30);
    assert!(id.starts_with("ses_"));
    assert!(id[4..].bytes().all(|c| c.is_ascii_alphanumeric()));
}

#[test]
fn file_hash_is_sha256_hex() {
    // sha256("") is a well-known value.
    assert_eq!(file_hash_bytes(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
}
