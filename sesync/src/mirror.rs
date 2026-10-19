//! Deterministic mirror ids and content hashes. Ported from daemon/sesync.py.
//! These must match the Python values exactly: a different id creates a second
//! copy of every mirror instead of replacing the existing one.

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Same namespace as the Python daemon (`NAMESPACE`).
const NAMESPACE: &str = "5e55a0b1-7c3e-4d2a-9f1b-0a12ab34cd56";

/// uuid5 of "<source_key>|<target>", the id of a jsonl mirror.
pub fn mirror_uuid(source_key: &str, target: &str) -> String {
    let ns = Uuid::parse_str(NAMESPACE).expect("constant namespace");
    Uuid::new_v5(&ns, format!("{source_key}|{target}").as_bytes()).to_string()
}

const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// "ses_" + 12 hex of sha256("<key>|opencode") + 14 base62 characters, the shape
/// OpenCode uses for session ids. Mirrors Python's byte pairs at [12..40) step 2.
pub fn mirror_opencode_id(source_key: &str) -> String {
    let digest = Sha256::digest(format!("{source_key}|opencode").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let mut tail = String::new();
    for i in (12..40).step_by(2) {
        let v = u8::from_str_radix(&hex[i..i + 2], 16).expect("hex") as usize;
        tail.push(ALPHABET[v % 62] as char);
    }
    format!("ses_{}{}", &hex[..12], tail)
}

/// Lowercase sha256 hex of a byte slice (the Python file_hash, for one buffer).
pub fn file_hash_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a source or mirror key names a file in a harness store, as opposed to an OpenCode
/// session id (`ses_...`), which is opaque and lives in the database.
///
/// This used to be spelled `key.starts_with('/')`. A Windows path is absolute but never starts
/// with `/`, so the test was false for every path on Windows: the demoted source in `promote`
/// was never recorded in `mirror_keys`, the scan re-adopted it as a source, and a conversation
/// ended up claiming to be both the source and its own mirror.
pub fn is_file_key(key: &str) -> bool {
    std::path::Path::new(key).is_absolute()
}

/// Lowercase sha256 hex of a file, read in 1 MiB chunks (the Python file_hash).
pub fn file_hash(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
