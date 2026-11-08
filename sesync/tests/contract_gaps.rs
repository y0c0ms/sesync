//! The last rules in the essential modules that no other test reaches: the lock holder
//! contract (holder_of reports who holds a conversation), the O-penCode default-name
//! rule, the O-penCode no-database fast path, and the to-opencode converter guard. Each
//! checks a behaviour a user or a harness relies on.

use sesync::harness::opencode::store::{clean_title, install_opencode, sessions};
use sesync::lock::holder_of;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[cfg(unix)]
#[path = "support/fake_cli.rs"]
mod fake_cli;

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-contract-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// The sesync_lock binary, built on demand so the test does not depend on a release build
/// that a fresh checkout does not have. Panics with the cargo output if the build fails.
fn sesync_lock() -> PathBuf {
    use std::sync::OnceLock;
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let lock_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-lock");
        let out = Command::new("cargo")
            .args(["build", "--quiet", "--manifest-path"])
            .arg(lock_dir.join("Cargo.toml"))
            .output()
            .expect("run cargo to build sesync-lock");
        assert!(out.status.success(), "building sesync_lock failed:
{}",
                String::from_utf8_lossy(&out.stderr));
        lock_dir.join("target").join("debug").join(format!("sesync-lock{}", std::env::consts::EXE_SUFFIX))
    }).clone()
}

#[test]
fn holder_of_names_the_live_holder_of_a_conversation() {
    let hs = sesync_lock();
    let sb = sandbox("holder");
    let lock_dir = sb.join("locks");
    fs::create_dir_all(&lock_dir).unwrap();
    let claim = Command::new(&hs).env("SESYNC_LOCK_DIR", &lock_dir)
        .args(["claim", "conv-h", "omp", &std::process::id().to_string(), "t"]).output().unwrap();
    assert!(claim.status.success());
    let who = holder_of(&hs, Some(&lock_dir), "conv-h").expect("a held conversation has a holder");
    assert!(who.starts_with("omp "), "the holder is named as 'harness pid', got {who:?}");
}

#[test]
fn holder_of_is_none_for_a_free_conversation_and_when_the_binary_is_missing() {
    let hs = sesync_lock();
    let sb = sandbox("free");
    let lock_dir = sb.join("locks");
    fs::create_dir_all(&lock_dir).unwrap();
    assert_eq!(holder_of(&hs, Some(&lock_dir), "nobody"), None);
    assert_eq!(holder_of(&PathBuf::from("/no/such/sesync_lock"), Some(&lock_dir), "x"), None);
}

#[test]
fn an_opencode_title_that_is_only_the_default_name_is_not_a_title() {
    assert_eq!(clean_title("New session - 2026-10-14T16:52:39.241Z"), "");
    assert_eq!(clean_title("  New session - 2026-10-14T16:52:39Z  "), "");
}

#[test]
fn a_real_title_that_starts_like_the_default_is_kept() {
    assert_eq!(clean_title("New session plan"), "New session plan");
    assert_eq!(clean_title("New session - Q3 plan"), "New session - Q3 plan");
}

#[test]
fn a_missing_opencode_database_is_an_empty_list_and_is_not_created() {
    let sb = sandbox("nodb");
    let db = sb.join("absent").join("opencode.db");
    assert!(sessions(&db).is_empty());
    assert!(!db.exists(), "reading a missing store must not create it");
}

#[cfg(unix)]
#[test]
fn a_failed_to_opencode_conversion_is_an_error_and_nothing_is_imported() {
    let sb = sandbox("convfail");
    let src = sb.join("src.jsonl");
    fs::write(&src, "{\"type\":\"session\",\"version\":3,\"id\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"timestamp\":\"2026-10-01T10:00:00.000Z\",\"cwd\":\"/tmp\"}\n").unwrap();
    // A converter that fails: install must surface the error and never call import.
    let fake_sesync_conv = sb.join("failing-sesync_conv");
    fake_cli::stage(&fake_sesync_conv, "#!/bin/sh\necho 'bad journal' >&2\nexit 3\n");
    let marker = sb.join("import-was-called");
    let fake_oc = sb.join("oc");
    fake_cli::stage(&fake_oc, &format!("#!/bin/sh\ntouch {}\necho 'Imported session'\n", marker.display()));
    let r = install_opencode(&fake_oc, &fake_sesync_conv, &src, "/", "", "ses_convfail00000000000000000", "t", Some(&sb.join("xdg")));
    let err = r.expect_err("a failed conversion must be an error");
    assert!(err.contains("to-opencode failed"), "the guard must be what stops it: {err}");
    assert!(!marker.exists(), "import must not run when the conversion failed");
}
