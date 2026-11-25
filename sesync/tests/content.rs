//! Content is preserved end to end: the prompt and the answer of an omp session must
//! appear in the pi mirror and in the O-penCode session. Ported from the content checks
//! in daemon/tests/test_adapters.py and test_matrix.py. Real sesync_conv and the real opencode
//! CLI, in a sandbox. Skipped when the CLI is not installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-content-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn have_cli() -> bool {
    // Real harness CLIs run only when HS_E2E=1. Plain `cargo test` never starts them,
    // so a local run cannot touch a live session store or call a model.
    if std::env::var("HS_E2E").map_or(true, |v| v != "1") {
        return false;
    }
    std::env::var_os("PATH").map_or(false, |p| std::env::split_paths(&p).any(|d| d.join("opencode").is_file()))
}

fn xdg(sb: &Path) -> [(String, PathBuf); 4] {
    [
        ("XDG_DATA_HOME".into(), sb.join("xdg").join("data")),
        ("XDG_CONFIG_HOME".into(), sb.join("xdg").join("cfg")),
        ("XDG_STATE_HOME".into(), sb.join("xdg").join("state")),
        ("XDG_CACHE_HOME".into(), sb.join("xdg").join("cache")),
    ]
}

fn run_once(sb: &Path) -> Output {
    let mut c = Command::new(BIN);
    c.arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("SPIDAIR_AGENT_DIR", sb.join("spidair"))
        .env("HOME", sb.join("home"))
        .env("USERPROFILE", sb.join("home"))
        .env("ANTIGRAVITY_CLI_HOME", sb.join("antigravity"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", sb.join("xdg").join("data").join("opencode").join("opencode.db"))
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0");
    for (k, v) in xdg(sb) {
        c.env(k, v);
    }
    c.output().expect("run sesync")
}

fn files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { out.extend(files(&p)); } else { out.push(p); }
        }
    }
    out
}

#[test]
fn prompt_and_answer_survive_into_pi_and_opencode() {
    if !have_cli() { eprintln!("skipped: opencode CLI not installed"); return; }
    let sb = sandbox("roundtrip");
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    // Create the O-penCode store with its own schema.
    let mut init = Command::new("opencode");
    init.args(["session", "list"]);
    for (k, v) in xdg(&sb) { init.env(k, v); }
    assert!(init.output().unwrap().status.success());

    let id = "aaaaaaaa-1111-2222-3333-444444444444";
    let prompt = format!("PROMPT_MARKER {}", "q".repeat(300));
    let answer = format!("ANSWER_MARKER {}", "a".repeat(300));
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let rows = [
        format!(r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/proj"}}"#),
        format!(r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"user","content":"{prompt}"}}}}"#),
        format!(r#"{{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-10-01T10:00:02.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{answer}"}}]}}}}"#),
    ];
    fs::write(dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl")), rows.join("\n") + "\n").unwrap();

    let o = run_once(&sb);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));

    let pi: Vec<PathBuf> = files(&sb.join("pi").join("sessions"));
    assert_eq!(pi.len(), 1, "one pi mirror");
    let pi_text = fs::read_to_string(&pi[0]).unwrap();
    assert!(pi_text.contains("PROMPT_MARKER"), "the prompt must survive into the pi mirror");
    assert!(pi_text.contains("ANSWER_MARKER"), "the answer must survive into the pi mirror");

    let mut list = Command::new("opencode");
    list.args(["session", "list"]);
    for (k, v) in xdg(&sb) { list.env(k, v); }
    let listed = String::from_utf8_lossy(&list.output().unwrap().stdout).into_owned();
    let oc_id = listed.lines().find(|l| l.starts_with("ses_")).and_then(|l| l.split_whitespace().next()).unwrap_or("").to_string();
    assert!(!oc_id.is_empty(), "the mirror must be listed in O-penCode:\n{listed}");
    let mut ex = Command::new("opencode");
    ex.args(["export", &oc_id]);
    for (k, v) in xdg(&sb) { ex.env(k, v); }
    let exported = String::from_utf8_lossy(&ex.output().unwrap().stdout).into_owned();
    assert!(exported.contains("PROMPT_MARKER"), "the prompt must survive into the O-penCode session");
    assert!(exported.contains("ANSWER_MARKER"), "the answer must survive into the O-penCode session");
}
