//! One omp source into several chosen targets. The user ticked pi and O-penCode for the
//! session, so a single pass must write the pi mirror and import the same conversation into
//! the O-penCode store. Uses the real sesync_conv and the real opencode CLI; skipped without
//! the CLI.

use sesync::harness::opencode::mirror_opencode_id;
use sesync::state::{Source, State};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-fanout-{}-{name}", std::process::id()));
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

fn run(sb: &Path) -> Output {
    Command::new(BIN)
        .arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", sb.join("xdg").join("data").join("opencode").join("opencode.db"))
        .env("OPENCODE_BIN", "opencode")
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .env("XDG_DATA_HOME", sb.join("xdg").join("data"))
        .env("XDG_CONFIG_HOME", sb.join("xdg").join("cfg"))
        .env("XDG_STATE_HOME", sb.join("xdg").join("state"))
        .env("XDG_CACHE_HOME", sb.join("xdg").join("cache"))
        .output()
        .expect("run sesync")
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { out.extend(walk(&p)); } else if p.extension().map_or(false, |x| x == "jsonl") { out.push(p); }
        }
    }
    out
}

#[test]
fn an_omp_session_is_mirrored_into_the_targets_the_user_chose() {
    if !have_cli() { eprintln!("skipped: opencode CLI not installed"); return; }
    let sb = sandbox("fanout");
    // Source: one omp session, old enough to pass the quiet check (QUIET=0 here).
    let id = "aaaaaaaa-1111-2222-3333-444444444444";
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let msg = format!("{}", "hello fanout ".repeat(60));
    let path = dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl"));
    fs::write(&path,
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-10-01T10:00:00.000Z\",\"cwd\":\"/tmp/proj\"}}\n\
{{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-10-01T10:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"{msg}\"}}}}\n")).unwrap();

    // The user chose both targets for this session; an empty choice would write nothing.
    fs::create_dir_all(sb.join("state")).unwrap();
    let key = path.display().to_string();
    let mut state = State::default();
    let mut src = Source::new("omp", &key);
    src.chosen_targets.insert("pi".to_string());
    src.chosen_targets.insert("opencode".to_string());
    state.sources.insert(key.clone(), src);
    state.save(&sb.join("state").join("state.json")).unwrap();

    // An O-penCode store must exist for that target to be installed.
    let mut init = Command::new("opencode");
    init.args(["session", "list"])
        .env("XDG_DATA_HOME", sb.join("xdg").join("data"))
        .env("XDG_CONFIG_HOME", sb.join("xdg").join("cfg"))
        .env("XDG_STATE_HOME", sb.join("xdg").join("state"))
        .env("XDG_CACHE_HOME", sb.join("xdg").join("cache"));
    assert!(init.output().unwrap().status.success());

    let o = run(&sb);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(walk(&sb.join("pi").join("sessions")).len(), 1, "one pi mirror");

    let listed = Command::new("opencode")
        .args(["session", "list", "--format", "json"])
        .env("XDG_DATA_HOME", sb.join("xdg").join("data"))
        .env("XDG_CONFIG_HOME", sb.join("xdg").join("cfg"))
        .env("XDG_STATE_HOME", sb.join("xdg").join("state"))
        .env("XDG_CACHE_HOME", sb.join("xdg").join("cache"))
        .output().unwrap();
    let text = String::from_utf8_lossy(&listed.stdout);
    let sessions: serde_json::Value = serde_json::from_str(&text).expect("session list --format json");
    let want = mirror_opencode_id(&sesync::manage::conversation_id(&key));
    assert!(
        sessions.as_array().map_or(false, |a| a.iter().any(|s| s["id"] == serde_json::json!(want))),
        "the chosen O-penCode import must be listed under {want}:\n{text}"
    );
}
