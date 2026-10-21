//! The refusal branches that protect the user's files. Each test confirms a guard that
//! must hold: a path outside the harness stores is never touched, a conversation held by
//! another harness is never written, and a rename that does not stick is an error.
//! Uses real files in a sandbox and the real sesync_lock binary for held locks.

use sesync::manage::{delete_group, rename_group, Group, Roots};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const SID: &str = "aaaaaaaa-1111-2222-3333-444444444444";

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-guard-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn roots(sb: &PathBuf) -> Roots {
    let r = Roots::under(&sb, sb.join("no-sesync-conv"));
    fs::create_dir_all(&r.locations.sessions_dir("omp")).unwrap();
    fs::create_dir_all(&r.locations.sessions_dir("pi")).unwrap();
    fs::create_dir_all(&r.locations.sessions_dir("claude")).unwrap();
    r
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
        lock_dir.join("target").join("debug").join("sesync-lock")
    }).clone()
}

#[test]
fn delete_never_touches_a_mirror_even_one_outside_the_roots() {
    let sb = sandbox("mirror-outside");
    let r = roots(&sb);
    let src = r.locations.sessions_dir("omp").join("-tmp").join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"));
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "source").unwrap();
    let outside = sb.join("elsewhere.jsonl");
    fs::write(&outside, "precious").unwrap();

    let mut g = Group::file("omp", src.display().to_string(), "t");
    g.add_mirror("pi", outside.display().to_string());
    let res = delete_group(&r, &g, &sb.join("locks"), &sb.join("no-sesync_lock"));
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(fs::read_to_string(&outside).unwrap(), "precious", "delete never touches a mirror");
    assert!(!src.exists(), "the picked session is removed");
}

#[test]
fn a_source_outside_the_session_roots_is_never_renamed() {
    let sb = sandbox("rename-outside");
    let r = roots(&sb);
    let outside = sb.join("elsewhere.jsonl");
    let body = format!("{{\"type\":\"session\",\"id\":\"{SID}\"}}\n");
    fs::write(&outside, &body).unwrap();
    let g = Group::file("omp", outside.display().to_string(), "t");
    assert!(rename_group(&r, &g, "New", &sb.join("locks"), &sb.join("no-sesync_lock")).is_err());
    assert_eq!(fs::read_to_string(&outside).unwrap(), body, "the file outside must be untouched");
}

#[test]
fn a_delete_is_refused_while_another_harness_holds_the_conversation() {
    let hs = sesync_lock();
    if !hs.exists() {
        eprintln!("skipped: sesync_lock release binary not built");
        return;
    }
    let sb = sandbox("held-delete");
    let r = roots(&sb);
    let lock_dir = sb.join("locks");
    fs::create_dir_all(&lock_dir).unwrap();
    // Another live harness claims the conversation.
    let claim = Command::new(&hs)
        .env("SESYNC_LOCK_DIR", &lock_dir)
        .args(["claim", SID, "claude", &std::process::id().to_string(), "t"])
        .output().unwrap();
    assert!(claim.status.success());

    let src = r.locations.sessions_dir("omp").join("-tmp").join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"));
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "source").unwrap();
    let g = Group::file("omp", src.display().to_string(), "t");
    let res = delete_group(&r, &g, &lock_dir, &hs);
    assert!(res.is_err(), "a delete while the conversation is held must be refused");
    assert!(res.unwrap_err().contains("in use"), "the refusal must say why");
    assert!(src.exists(), "a refused delete must leave the source in place");
}

#[test]
fn a_rename_is_refused_while_another_harness_holds_the_conversation() {
    let hs = sesync_lock();
    if !hs.exists() {
        eprintln!("skipped: sesync_lock release binary not built");
        return;
    }
    let sb = sandbox("held-rename");
    let r = roots(&sb);
    let lock_dir = sb.join("locks");
    fs::create_dir_all(&lock_dir).unwrap();
    let claim = Command::new(&hs)
        .env("SESYNC_LOCK_DIR", &lock_dir)
        .args(["claim", SID, "claude", &std::process::id().to_string(), "t"])
        .output().unwrap();
    assert!(claim.status.success());

    let src = r.locations.sessions_dir("omp").join("-tmp").join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"));
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    let body = format!("{{\"type\":\"session\",\"id\":\"{SID}\",\"title\":\"Old\"}}\n");
    fs::write(&src, &body).unwrap();
    let g = Group::file("omp", src.display().to_string(), "Old");
    assert!(rename_group(&r, &g, "New", &lock_dir, &hs).is_err());
    assert_eq!(fs::read_to_string(&src).unwrap(), body, "a refused rename must not write the file");
}

#[test]
fn a_rename_that_is_not_written_back_is_an_error_not_a_success() {
    // A rename for a format whose rewrite drops the title must fail. The Claude fix made
    // the new name always appear; this guards the verify step itself, using an omp file
    // whose header has no session record, so the rewrite cannot place the title.
    let sb = sandbox("unwritten");
    let r = roots(&sb);
    let src = r.locations.sessions_dir("omp").join("-tmp").join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"));
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"no header\"}}\n").unwrap();
    let g = Group::file("omp", src.display().to_string(), "x");
    let res = rename_group(&r, &g, "Unplaceable title", &sb.join("locks"), &sb.join("no-sesync_lock"));
    assert!(res.is_err(), "a rename whose title cannot be written must not report success");
}
