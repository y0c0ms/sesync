//! Guards against lock leaks into a user's real lock directory. A test that runs the daemon must
//! sandbox its lock client, or the daemon's startup sweep and claims touch the real directory.
//! A test fixture ended up in a real lock directory this way.

use std::fs;
use std::path::Path;

#[test]
fn every_test_that_runs_the_daemon_sandboxes_the_lock_client() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut offenders = Vec::new();
    for entry in fs::read_dir(&dir).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("lock_sandbox_guard.rs") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        if text.contains("CARGO_BIN_EXE_sesync") && !text.contains("\"SESYNC_LOCK") {
            offenders.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    assert!(offenders.is_empty(), "these tests run the daemon without sandboxing its lock client: {offenders:?}");
}

#[test]
fn the_daemon_never_claims_without_naming_its_lock_directory() {
    let src = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/sesync.rs")).unwrap();
    assert!(!src.contains(", None, &conv"), "every claim must name its lock directory");
}

#[test]
fn the_default_lock_dir_is_the_stored_one_not_the_state_dir() {
    use sesync::lock::lock_dir_for;
    let home = Path::new("/home/someone");
    assert_eq!(lock_dir_for(home, None), home.join(".local/share/harness-sync/locks"));
}

#[test]
fn an_explicit_lock_dir_wins_over_the_default() {
    use sesync::lock::lock_dir_for;
    assert_eq!(lock_dir_for(Path::new("/home/someone"), Some("/sandbox/locks".into())), Path::new("/sandbox/locks"));
}
