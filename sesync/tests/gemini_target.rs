//! The Gemini CLI mirror target and source, end to end through the real daemon and the real
//! converter. A chosen session is converted and written into `<GEMINI_CLI_HOME>/tmp/<slug>/chats`,
//! registered in `projects.json` with a `.project_root` marker so `gemini --list-sessions` finds
//! it, and recorded in `mirror_keys` because the Gemini store is also scanned for sources: the
//! copy must never become one. A Gemini session chosen as a source is converted to a journal
//! before any target converter sees it. No real harness runs; no real store is touched.

use sesync::state::{Source, State};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-gemini-target-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Pre-seed the user's choice exactly as the TUI would save it, before any pass runs.
fn choose(sb: &Path, fmt: &str, key: &str, targets: &[&str]) {
    fs::create_dir_all(sb.join("state")).unwrap();
    let mut state = State::default();
    let mut src = Source::new(fmt, key);
    for t in targets {
        src.chosen_targets.insert((*t).to_string());
    }
    state.sources.insert(key.to_string(), src);
    state.save(&sb.join("state").join("state.json")).unwrap();
}

fn run(sb: &Path) -> Output {
    Command::new(BIN)
        .arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("GEMINI_CLI_HOME", sb.join("gemini"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", sb.join("no-opencode-bin"))
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .env("SESYNC_MAX_AGE_DAYS", "3650")
        .output()
        .expect("run sesync")
}

/// A native omp journal, padded past the 512-byte minimum a session needs. Its header cwd
/// becomes the Gemini project.
fn omp_session(sb: &Path) -> PathBuf {
    let id = "aaaaaaaa-1111-2222-3333-444444444444";
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl"));
    let header = format!(r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/proj"}}"#);
    let msg = format!(r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"user","content":"{}"}}}}"#, "hello gemini target ".repeat(30));
    fs::write(&path, format!("{header}\n{msg}\n")).unwrap();
    path
}

/// Every session file in the Gemini store, as `gemini_sessions` would find them.
fn gemini_files(sb: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let root = sb.join("gemini").join("tmp");
    let Ok(projects) = fs::read_dir(&root) else { return out };
    for p in projects.flatten() {
        let Ok(files) = fs::read_dir(p.path().join("chats")) else { continue };
        for f in files.flatten() {
            let path = f.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_file() && name.starts_with("session-") && name.ends_with(".jsonl") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Every journal in the Pi store, as the daemon writes them.
fn pi_files(sb: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let root = sb.join("pi").join("sessions");
    let Ok(dirs) = fs::read_dir(&root) else { return out };
    for d in dirs.flatten() {
        let Ok(files) = fs::read_dir(d.path()) else { continue };
        out.extend(files.flatten().map(|f| f.path()).filter(|p| p.extension().map_or(false, |x| x == "jsonl")));
    }
    out.sort();
    out
}

#[test]
fn an_omp_session_chosen_for_gemini_is_installed_registered_and_refreshed_in_place() {
    let sb = sandbox("install");
    let path = omp_session(&sb);
    let key = path.display().to_string();
    choose(&sb, "omp", &key, &["gemini"]);

    let o = run(&sb);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    let files = gemini_files(&sb);
    assert_eq!(files.len(), 1, "one chosen gemini mirror must be written");
    let body = fs::read_to_string(&files[0]).unwrap();
    assert!(body.contains("hello gemini target"), "the conversation is carried:\n{body}");
    assert!(body.contains("\"type\":\"user\""), "the mirror is a Gemini session log:\n{body}");
    assert!(!body.contains("\"version\":3"), "the raw omp journal must not be copied through:\n{body}");

    let slug = files[0].parent().unwrap().parent().unwrap().file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(slug, "proj", "the slug is the slugified directory name");
    let reg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sb.join("gemini").join("projects.json")).unwrap()).unwrap();
    assert_eq!(reg["projects"]["/tmp/proj"], serde_json::json!(slug), "the project is registered for --list-sessions");
    assert!(sb.join("gemini").join("tmp").join(&slug).join(".project_root").is_file(), "the ownership marker is written");

    let state = State::load(&sb.join("state").join("state.json")).unwrap();
    let m = &state.sources[&key].mirrors["gemini"];
    assert_eq!(m.path, files[0].display().to_string(), "the mirror records the installed file");
    assert!(state.mirror_keys.contains(&m.path), "a gemini mirror is a file inside the scanned store");
    assert_eq!(m.source_hash, state.sources[&key].last_hash, "the mirror remembers the source hash");

    // Nothing changed: no second write, and the mirror file is not a source either.
    let o = run(&sb);
    assert!(String::from_utf8_lossy(&o.stdout).contains("0 source(s) changed"), "{}", String::from_utf8_lossy(&o.stdout));
    assert_eq!(gemini_files(&sb).len(), 1, "an unchanged session is not re-written");
    assert_eq!(State::load(&sb.join("state").join("state.json")).unwrap().sources.len(), 1, "the mirror never becomes a source");

    // The conversation changed: the same file is replaced in place.
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str(&format!("{}\n", r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"assistant","content":"a later turn"}}"#));
    fs::write(&path, text).unwrap();
    let o = run(&sb);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    let files = gemini_files(&sb);
    assert_eq!(files.len(), 1, "one conversation keeps one file across refreshes");
    assert_eq!(files[0], PathBuf::from(&m.path.clone()), "the refresh replaces the same path");
    assert!(fs::read_to_string(&files[0]).unwrap().contains("a later turn"), "the refreshed file carries the new turn");
}

#[test]
fn a_gemini_source_is_converted_to_a_real_journal_for_the_pi_mirror() {
    let sb = sandbox("source");
    let dir = sb.join("gemini").join("tmp").join("-tmp-proj").join("chats");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    let path = dir.join("session-2026-10-01T10-00-00Z-abc12345.jsonl");
    let text = "hello from a gemini source ".repeat(30);
    fs::write(&path, format!(
        "{{\"sessionId\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"projectHash\":\"deadbeef\",\"startTime\":\"2026-10-01T10:00:00.000Z\",\"lastUpdated\":\"2026-10-01T10:00:02.000Z\",\"kind\":\"main\"}}\n\
         {{\"id\":\"u1\",\"timestamp\":\"2026-10-01T10:00:01.000Z\",\"type\":\"user\",\"content\":[{{\"text\":\"{text}\"}}]}}\n\
         {{\"id\":\"g1\",\"timestamp\":\"2026-10-01T10:00:02.000Z\",\"type\":\"gemini\",\"content\":[{{\"text\":\"a reply from gemini\"}}]}}\n")).unwrap();
    let key = path.display().to_string();
    choose(&sb, "gemini", &key, &["pi"]);

    let o = run(&sb);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    let state = State::load(&sb.join("state").join("state.json")).unwrap();
    assert_eq!(state.sources[&key].fmt, "gemini", "the gemini store is scanned for sources");
    let mirrors = pi_files(&sb);
    assert_eq!(mirrors.len(), 1, "the chosen pi mirror must be written");
    let body = fs::read_to_string(&mirrors[0]).unwrap();
    assert!(body.contains("hello from a gemini source"), "the conversation is carried:\n{body}");
    assert!(body.contains("\"type\":\"message\""), "the mirror is a journal:\n{body}");
    assert!(!body.contains("projectHash"), "the raw Gemini log must not be copied through:\n{body}");

    // The gemini mirror is tracked, and the source is not re-converted while it is unchanged.
    assert!(state.mirror_keys.contains(&mirrors[0].display().to_string()), "a pi mirror file is tracked");
    let o = run(&sb);
    assert!(String::from_utf8_lossy(&o.stdout).contains("0 source(s) changed"), "{}", String::from_utf8_lossy(&o.stdout));
    assert_eq!(pi_files(&sb).len(), 1);
}
