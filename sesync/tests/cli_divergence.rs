//! End to end: a harness continues a conversation inside a mirror. The next pass,
//! after the source changes, must not overwrite that mirror. Real sesync_conv, sandbox
//! stores only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-cliDiv-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn run(sb: &Path) -> Output {
    Command::new(BIN)
        .arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .output()
        .expect("run sesync")
}

fn session_file(sb: &Path, id: &str, body: &str) -> PathBuf {
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl"));
    let header = format!(r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/proj"}}"#);
    let msg = format!(r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"user","content":"{body}"}}}}"#);
    fs::write(&p, format!("{header}\n{msg}\n")).unwrap();
    p
}

fn pi_mirror(sb: &Path) -> PathBuf {
    let mut found = Vec::new();
    if let Ok(rd) = fs::read_dir(sb.join("pi").join("sessions")) {
        for b in rd.flatten() {
            if let Ok(files) = fs::read_dir(b.path()) {
                found.extend(files.flatten().map(|f| f.path()));
            }
        }
    }
    assert_eq!(found.len(), 1, "exactly one pi mirror expected, got {found:?}");
    found.remove(0)
}

fn count_files(dir: &Path) -> usize {
    fs::read_dir(dir).map(|rd| rd.flatten()
        .map(|bucket| fs::read_dir(bucket.path()).map(|f| f.flatten().count()).unwrap_or(0))
        .sum()).unwrap_or(0)
}

#[test]
fn a_harness_turn_in_its_own_store_is_never_overwritten() {
    let sb = sandbox("freeze");
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let id = "aaaaaaaa-1111-2222-3333-444444444444";
    session_file(&sb, id, &"first ".repeat(120));
    // A harness keeps working in its own session file in the pi store.
    let own_dir = sb.join("pi").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&own_dir).unwrap();
    let own = own_dir.join("2026-10-01T10-00-00-000Z_harness.jsonl");
    let harness_text = "HARNESS WROTE THIS TURN";
    fs::write(&own, format!("first\n{harness_text}\n")).unwrap();
    // The omp source changes, so the next pass has work to do.
    std::thread::sleep(std::time::Duration::from_millis(20));
    session_file(&sb, id, &"second ".repeat(120));
    assert!(run(&sb).status.success());
    let after = fs::read_to_string(&own).unwrap();
    assert!(after.contains(harness_text), "the harness's turn must survive");
    assert_eq!(count_files(&sb.join("pi").join("sessions")), 1, "a pass adds no file to the pi store");
}

