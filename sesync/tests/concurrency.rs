//! Concurrency: two daemons on the same stores must not write the same mirror twice or
//! corrupt state, and a conversation held by another harness must stay untouched while a
//! sync is refused. Uses the real binary, the real sesync_conv and the real sesync_lock, in a
//! sandbox.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-conc-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn env_for(mut cmd: Command, sb: &Path) -> Command {
    cmd.env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .env("SESYNC_POLL", "0.2");
    cmd
}

fn session(sb: &Path, id: &str) {
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let header = format!(r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/proj"}}"#);
    let msg = format!(r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"user","content":"{}"}}}}"#, "hello ".repeat(120));
    fs::write(dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl")), format!("{header}\n{msg}\n")).unwrap();
}

fn pi_mirror_count(sb: &Path) -> usize {
    let mut n = 0;
    if let Ok(rd) = fs::read_dir(sb.join("pi").join("sessions")) {
        for b in rd.flatten() {
            if let Ok(files) = fs::read_dir(b.path()) {
                n += files.flatten().count();
            }
        }
    }
    n
}

#[test]
fn two_daemons_on_the_same_stores_record_each_session_once() {
    let sb = sandbox("two-daemons");
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    session(&sb, "aaaaaaaa-1111-2222-3333-444444444444");
    session(&sb, "bbbbbbbb-1111-2222-3333-444444444444");

    // Two watchers run the same passes at once against the same state file.
    let mut a = env_for(Command::new(BIN), &sb)
        .arg("--watch").env("SESYNC_PASSES", "6")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let mut b = env_for(Command::new(BIN), &sb)
        .arg("--watch").env("SESYNC_PASSES", "6")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    for c in [&mut a, &mut b] {
        loop {
            if c.try_wait().unwrap().is_some() { break; }
            if Instant::now() > deadline { let _ = c.kill(); panic!("a watcher did not finish"); }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    // Concurrent passes write nothing into pi, and must not duplicate or drop a record.
    assert_eq!(pi_mirror_count(&sb), 0, "concurrent passes write nothing into pi");

    // The state file must be valid JSON after concurrent saves.
    let state = fs::read_to_string(sb.join("state").join("state.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&state).expect("state.json must stay valid JSON");
    assert!(v.get("sources").is_some());
    assert_eq!(v["sources"].as_object().unwrap().len(), 2, "each session is recorded exactly once");
}

#[test]
fn a_conversation_held_by_another_harness_is_not_mirrored() {
    let hs = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../sesync-lock/target/debug/sesync-lock{}", std::env::consts::EXE_SUFFIX));
    let _ = Command::new("cargo").args(["build", "--quiet", "--manifest-path"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-lock/Cargo.toml")).status();
    if !hs.exists() {
        panic!("sesync_lock not built at {}", hs.display());
    }
    let sb = sandbox("held");
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let id = "cccccccc-1111-2222-3333-444444444444";
    session(&sb, id);
    let lock_dir = sb.join("locks");
    fs::create_dir_all(&lock_dir).unwrap();
    let claim = Command::new(&hs).env("SESYNC_LOCK_DIR", &lock_dir)
        .args(["claim", id, "claude", &std::process::id().to_string(), "t"]).output().unwrap();
    assert!(claim.status.success());

    let mut cmd = env_for(Command::new(BIN), &sb);
    cmd.arg("--once").env("SESYNC_LOCK", &hs).env("SESYNC_LOCK_DIR", &lock_dir);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    assert_eq!(pi_mirror_count(&sb), 0, "a held conversation must not be mirrored while held");
}

#[test]
fn a_second_watcher_on_the_same_state_refuses_instead_of_racing() {
    // Two daemons on one state directory must not both run passes: the second refuses
    // with a message while the first holds the daemon lock. Without this, concurrent
    // passes wrote duplicate mirrors and corrupted state.json (found by the tests above).
    let sb = sandbox("single-instance");
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    session(&sb, "dddddddd-1111-2222-3333-444444444444");
    let mut first = env_for(Command::new(BIN), &sb)
        .arg("--watch").env("SESYNC_PASSES", "200").env("SESYNC_POLL", "0.2")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    // Give the first watcher time to take the daemon lock.
    std::thread::sleep(Duration::from_millis(600));
    let second = env_for(Command::new(BIN), &sb)
        .arg("--watch").env("SESYNC_PASSES", "2")
        .output().unwrap();
    let _ = first.kill();
    let _ = first.wait();
    assert!(!second.status.success(), "a second watcher on the same state must not run");
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(err.contains("already running"), "the refusal must say why: {err}");
}
