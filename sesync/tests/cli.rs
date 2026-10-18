//! End-to-end tests for the sesync binary, the Rust replacement for
//! `python daemon/sesync.py --status | --once`. Every store lives in a
//! private temp directory; the real omp, pi, claude and opencode stores are never
//! touched. The converter is the real sesync_conv binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-cli-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Run the binary with every store pointed into the sandbox.
fn run(sb: &Path, extra: &[&str], converter: &Path) -> Output {
    Command::new(BIN)
        .args(extra)
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("SESYNC_CONV", converter)
        // Point O-penCode at an empty sandbox path. Without this the binary reads the
        // user's real opencode.db and mirrors their live sessions.
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .output()
        .expect("run sesync")
}

/// A native omp journal, padded past the 512-byte minimum a session needs.
fn omp_session(sb: &Path) -> PathBuf {
    let id = "aaaaaaaa-1111-2222-3333-444444444444";
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl"));
    let header = format!(r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/proj"}}"#);
    let msg = format!(r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"user","content":"{}"}}}}"#, "hello ".repeat(120));
    fs::write(&path, format!("{header}\n{msg}\n")).unwrap();
    path
}

fn pi_mirrors(sb: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let root = sb.join("pi").join("sessions");
    if let Ok(rd) = fs::read_dir(&root) {
        for b in rd.flatten() {
            if let Ok(files) = fs::read_dir(b.path()) {
                out.extend(files.flatten().map(|f| f.path()));
            }
        }
    }
    out
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn status_on_an_empty_state_prints_nothing_and_succeeds() {
    let sb = sandbox("status-empty");
    let o = run(&sb, &["--status"], &sesync_conv());
    assert!(o.status.success(), "{o:?}");
    assert_eq!(stdout(&o), "");
}

#[test]
fn status_lists_sources_from_the_state_file() {
    let sb = sandbox("status-list");
    fs::create_dir_all(sb.join("state")).unwrap();
    fs::write(sb.join("state").join("state.json"), r#"{"sources":{"/x/2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl":{"fmt":"omp","key":"k","last_seen_change":5.0,"last_hash":"","last_stamp":"","failures":2,"mirrors":{}}},"mirror_keys":[]}"#).unwrap();
    let o = run(&sb, &["--status"], &sesync_conv());
    assert!(o.status.success(), "{o:?}");
    let out = stdout(&o);
    assert!(out.contains("omp       2026-10-01T10-00-00-000Z_"), "{out}");
    assert!(out.contains("failures: 2"), "{out}");
}

#[test]
fn once_records_an_omp_session_and_writes_no_copy_into_pi() {
    let sb = sandbox("once-record");
    omp_session(&sb);
    fs::create_dir_all(sb.join("pi")).unwrap();
    let o = run(&sb, &["--once"], &sesync_conv());
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    assert!(stdout(&o).contains("one pass done, 1 source(s) changed"), "{}", stdout(&o));
    assert!(pi_mirrors(&sb).is_empty(), "a pass writes no copy into the pi store");
    let state = fs::read_to_string(sb.join("state").join("state.json")).unwrap();
    assert!(state.contains("aaaaaaaa-1111-2222-3333-444444444444"), "the session is recorded as a source");
}

#[test]
fn a_second_pass_with_no_changes_reports_nothing_changed() {
    let sb = sandbox("once-idle");
    omp_session(&sb);
    fs::create_dir_all(sb.join("pi")).unwrap();
    assert!(run(&sb, &["--once"], &sesync_conv()).status.success());
    let o = run(&sb, &["--once"], &sesync_conv());
    assert!(stdout(&o).contains("one pass done, 0 source(s) changed"), "{}", stdout(&o));
    assert!(pi_mirrors(&sb).is_empty(), "an unchanged session writes nothing");
}

#[test]
fn a_pass_never_runs_the_converter() {
    let sb = sandbox("once-no-converter");
    omp_session(&sb);
    fs::create_dir_all(sb.join("pi")).unwrap();
    // A converter that always fails would count failures if the pass ran it.
    let o = run(&sb, &["--once"], Path::new("/bin/false"));
    assert!(o.status.success(), "a pass has no converter to fail: {o:?}");
    assert!(pi_mirrors(&sb).is_empty(), "no copy is written, so no conversion runs");
    let st = run(&sb, &["--status"], &sesync_conv());
    assert!(!stdout(&st).contains("failures:"), "{}", stdout(&st));
}

#[test]
fn a_corrupt_state_file_stops_the_daemon_instead_of_forgetting_mirrors() {
    let sb = sandbox("corrupt");
    fs::create_dir_all(sb.join("state")).unwrap();
    fs::write(sb.join("state").join("state.json"), "{not json").unwrap();
    let o = run(&sb, &["--once"], &sesync_conv());
    assert!(!o.status.success(), "a corrupt state must be an error, not an empty state");
}
