//! --watch: the continuous loop. Ported from the Python main loop, which runs a pass,
//! then sleeps SESYNC_POLL seconds, forever. A test bound (SESYNC_PASSES)
//! lets the process exit by itself so the test can finish. A session that appears after
//! the first pass must be picked up by a later pass.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-watch-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn session(sb: &Path, id: &str, body: &str) {
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let header = format!(r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/proj"}}"#);
    let msg = format!(r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"user","content":"{body}"}}}}"#);
    fs::write(dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl")), format!("{header}\n{msg}\n")).unwrap();
}

fn pi_mirrors(sb: &Path) -> usize {
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
fn watch_records_a_session_that_appears_after_the_first_pass() {
    let sb = sandbox("late");
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    fs::create_dir_all(sb.join("omp").join("sessions")).unwrap();
    session(&sb, "aaaaaaaa-1111-2222-3333-444444444444", &"first ".repeat(120));

    // Passes: the first pass finds session A. A session B appears after pass 1 and must
    // be mirrored by pass 2 or 3.
    let mut child = Command::new(BIN)
        .arg("--watch")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .env("SESYNC_POLL", "0.3")
        .env("SESYNC_PASSES", "4")
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    session(&sb, "bbbbbbbb-1111-2222-3333-444444444444", &"second ".repeat(120));

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "--watch must exit 0 after its pass limit: {status}");
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("--watch did not exit after SESYNC_PASSES passes");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(pi_mirrors(&sb), 0, "watch writes nothing into pi");
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sb.join("state").join("state.json")).unwrap()).unwrap();
    assert_eq!(state["sources"].as_object().unwrap().len(), 2, "both sessions are recorded: one before and one after startup");
}
