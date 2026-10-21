//! A pass never writes into a harness store. A session stays where its harness keeps it, and
//! sesync records only its own state. The old daemon copied every session into every other harness.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-nofanout-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Every file under a directory, with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.clone(), fs::read(&p).unwrap());
            }
        }
    }
    out
}

/// Run the daemon with every path and lock client inside the sandbox.
fn run(sb: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("HOME", sb.join("home"))
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("ANTIGRAVITY_CLI_HOME", sb.join("antigravity"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("SESYNC_CONV", sb.join("no-converter"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_LOCK_DIR", sb.join("locks"))
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
    let filler = "x".repeat(600);
    let msg = format!(r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"user","content":"{filler}"}}}}"#);
    fs::write(&path, format!("{header}\n{msg}\n")).unwrap();
    path
}

#[test]
fn a_pass_writes_nothing_into_any_harness_store() {
    let sb = sandbox("pass");
    omp_session(&sb);
    // Every other harness store exists, so the old fan-out would have written into all of them.
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let before: Vec<_> = ["omp", "pi", "claude"].iter().map(|h| snapshot(&sb.join(h))).collect();

    let out = run(&sb, &["--once"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let after: Vec<_> = ["omp", "pi", "claude"].iter().map(|h| snapshot(&sb.join(h))).collect();
    assert_eq!(before, after, "a pass must leave every harness store byte-identical");

    let state = fs::read_to_string(sb.join("state").join("state.json")).unwrap();
    assert!(state.contains("aaaaaaaa-1111-2222-3333-444444444444"), "the pass still records the session it saw");
    let _ = fs::remove_dir_all(&sb);
}
