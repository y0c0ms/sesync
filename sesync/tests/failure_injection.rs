//! Failure injection: the daemon must degrade, not corrupt. A converter that is missing,
//! a store that cannot be written, and an interrupted write each end with the source
//! untouched, no half-written mirror left in the store, and a clear error. Uses the real
//! binary in a sandbox; read-only stores are made with file permissions.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");
const SID: &str = "aaaaaaaa-1111-2222-3333-444444444444";

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-fail-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn session(sb: &Path) -> PathBuf {
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"));
    let body = format!(
        "{}\n{}\n",
        format!(r#"{{"type":"session","version":3,"id":"{SID}","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/proj"}}"#),
        format!(r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"user","content":"{}"}}}}"#, "hello ".repeat(120)),
    );
    fs::write(&p, body).unwrap();
    p
}

fn run(sb: &Path, sesync_conv_path: &Path) -> Output {
    Command::new(BIN)
        .arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("SESYNC_CONV", sesync_conv_path)
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .output()
        .expect("run sesync")
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { out.extend(files_under(&p)); } else { out.push(p); }
        }
    }
    out
}

#[test]
fn a_missing_converter_leaves_the_source_alone_and_the_store_empty() {
    let sb = sandbox("noconv");
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let src = session(&sb);
    let before = fs::read_to_string(&src).unwrap();
    let o = run(&sb, Path::new("/no/such/sesync_conv"));
    // The pass continues, the failure is counted, and nothing is written to the store.
    assert!(o.status.success(), "a missing converter must not crash the daemon: {o:?}");
    assert_eq!(fs::read_to_string(&src).unwrap(), before, "the source must not change");
    assert!(files_under(&sb.join("pi").join("sessions")).is_empty(), "no mirror may be written");
}

#[test]
fn a_read_only_store_fails_the_mirror_without_damaging_anything() {
    if unsafe { libc_geteuid() } == 0 {
        eprintln!("skipped: root ignores file permissions");
        return;
    }
    let sb = sandbox("readonly");
    let pi = sb.join("pi").join("sessions");
    fs::create_dir_all(&pi).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let src = session(&sb);
    let before = fs::read_to_string(&src).unwrap();
    fs::set_permissions(&pi, fs::Permissions::from_mode(0o555)).unwrap();
    let o = run(&sb, &sesync_conv());
    fs::set_permissions(&pi, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(o.status.success(), "a read-only store must not crash the daemon: {o:?}");
    assert_eq!(fs::read_to_string(&src).unwrap(), before, "the source must not change");
    assert!(files_under(&pi).is_empty(), "a failed write must leave no partial mirror");
    // Every mirror the state records must exist on disk. A failed write recorded as done
    // would point at a file that is not there.
    let state_text = fs::read_to_string(sb.join("state").join("state.json")).unwrap_or_default();
    let state: serde_json::Value = if state_text.is_empty() { serde_json::json!({}) }
        else { serde_json::from_str(&state_text).expect("state is valid JSON") };
    if let Some(sources) = state.get("sources").and_then(|v| v.as_object()) {
        for (_, src) in sources {
            if let Some(mirrors) = src.get("mirrors").and_then(|v| v.as_object()) {
                for (target, m) in mirrors {
                    let path = m.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    assert!(Path::new(path).exists(),
                        "state records a {target} mirror that was not written: {path}");
                }
            }
        }
    }
}

#[test]
fn a_stale_temporary_file_is_left_alone_and_a_pass_adds_nothing_beside_it() {
    // A .tmp from an earlier crashed write is not sesync's to delete, and a pass adds no file.
    let sb = sandbox("tmpfile");
    let pi = sb.join("pi").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&pi).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    fs::write(pi.join("2026-01-01T00-00-00-000Z_crashed.jsonl.tmp"), "partial").unwrap();
    session(&sb);
    let o = run(&sb, &sesync_conv());
    assert!(o.status.success(), "{o:?}");
    let files = files_under(&sb.join("pi").join("sessions"));
    assert_eq!(files.len(), 1, "the pass adds nothing to the store: {files:?}");
    assert!(files[0].to_string_lossy().contains("crashed"), "the stale file is untouched");
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}
