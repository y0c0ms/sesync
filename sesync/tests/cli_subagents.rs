//! End to end: a Claude session with a subagent transcript. One pass must record the
//! subagent under its parent in the state, and must not mirror the subagent as a
//! conversation of its own. Real binary, sandbox stores only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_sesync");
const PARENT: &str = "ec447302-155c-4280-9c34-2b5122fcd4a4";

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-subagent-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn run(sb: &Path) -> std::process::Output {
    Command::new(BIN)
        .arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("ANTIGRAVITY_CLI_HOME", sb.join("antigravity"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("SESYNC_CONV", PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .output()
        .expect("run sesync")
}

#[test]
fn a_subagent_is_recorded_under_its_parent_and_is_not_a_conversation() {
    let sb = sandbox("e2e");
    let dir = sb.join("claude").join("projects").join("-proj");
    let sub = dir.join(PARENT).join("subagents");
    fs::create_dir_all(&sub).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    let body = format!(
        "{}\n",
        format!(r#"{{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"{PARENT}","cwd":"/tmp/proj","timestamp":"2026-10-01T10:00:00.000Z","message":{{"role":"user","content":"{}"}}}}"#, "hello ".repeat(120)),
    );
    fs::write(dir.join(format!("{PARENT}.jsonl")), body).unwrap();
    let sub_file = sub.join("agent-a1.jsonl");
    fs::write(&sub_file, r#"{"type":"user","uuid":"s1","sessionId":"sub","isSidechain":true,"message":{"role":"user","content":"sub work"}}"#).unwrap();

    let o = run(&sb);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));

    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(sb.join("state").join("state.json")).unwrap()).unwrap();
    let recorded = state["subagents"][PARENT].as_array().expect("subagent recorded under the parent");
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].as_str().unwrap().ends_with("agent-a1.jsonl"));

    // Nothing is copied into pi: neither the parent nor the subagent.
    let mut mirrors = 0;
    if let Ok(rd) = fs::read_dir(sb.join("pi").join("sessions")) {
        for b in rd.flatten() {
            if let Ok(files) = fs::read_dir(b.path()) { mirrors += files.flatten().count(); }
        }
    }
    assert_eq!(mirrors, 0, "no copy is written into pi, for the parent or the subagent");
}

#[test]
fn a_claude_session_is_recorded_as_a_source() {
    // Regression: the Rust port once had no Claude source at all. A plain Claude session must be
    // seen and recorded as a source, and nothing is copied into pi.
    let sb = sandbox("claude-source");
    let dir = sb.join("claude").join("projects").join("-proj");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    let body = format!(
        "{}\n",
        format!(r#"{{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"{PARENT}","cwd":"/tmp/proj","timestamp":"2026-10-01T10:00:00.000Z","message":{{"role":"user","content":"{}"}}}}"#, "hello ".repeat(120)),
    );
    fs::write(dir.join(format!("{PARENT}.jsonl")), body).unwrap();
    let o = run(&sb);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    let mut mirrors = 0;
    if let Ok(rd) = fs::read_dir(sb.join("pi").join("sessions")) {
        for b in rd.flatten() {
            if let Ok(files) = fs::read_dir(b.path()) { mirrors += files.flatten().count(); }
        }
    }
    assert_eq!(mirrors, 0, "nothing is copied into pi");
    let state = fs::read_to_string(sb.join("state").join("state.json")).unwrap();
    assert!(state.contains(PARENT), "the Claude session is recorded as a source");
}

#[test]
fn a_subagent_transcript_is_kept_as_a_verified_archive_under_its_parent() {
    // The daemon keeps a compressed copy of each subagent transcript under its parent in
    // the archive store, so the copy survives the original being removed.
    let sb = sandbox("archive-sub");
    let dir = sb.join("claude").join("projects").join("-proj");
    let parent = PARENT;
    let sub = dir.join(parent).join("subagents");
    fs::create_dir_all(&sub).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::write(dir.join(format!("{parent}.jsonl")), format!(
        "{}\n",
        format!(r#"{{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"{parent}","cwd":"/tmp/proj","timestamp":"2026-10-01T10:00:00.000Z","message":{{"role":"user","content":"{}"}}}}"#, "hello ".repeat(120))
    )).unwrap();
    let body: String = (0..400).map(|i| format!("{{\"type\":\"user\",\"uuid\":\"s{i}\",\"sessionId\":\"sub\",\"isSidechain\":true,\"message\":{{\"role\":\"user\",\"content\":\"sub work step {i}\"}}}}\n")).collect();
    let sub_file = sub.join("agent-a1.jsonl");
    fs::write(&sub_file, &body).unwrap();

    let o = run(&sb);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));

    let archived = sb.join("state").join("archive").join(parent).join("agent-a1.jsonl.hsar");
    assert!(archived.exists(), "the subagent transcript must be archived under its parent");
    let back = sesync::archive_store::read_session(&archived).expect("archive must verify");
    assert_eq!(back, body.as_bytes(), "the archived copy must equal the original");
    assert!(fs::metadata(&archived).unwrap().len() < body.len() as u64 / 3, "and must be compressed");
}
