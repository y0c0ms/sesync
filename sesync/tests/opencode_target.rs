//! Mirror targets that need more than a file copy: the O-penCode import and the Claude
//! transcript source. A chosen session is converted and loaded with the CLI (a fake here that
//! records its invocations, so no real harness runs), delete-first so a refresh replaces the
//! same session. Claude transcripts must be converted to a journal before any target
//! converter sees them.

use sesync::mirror::mirror_opencode_id;
use sesync::state::{Source, State};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-oc-target-{}-{name}", std::process::id()));
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

fn run(sb: &Path, fake_opencode: &Path) -> Output {
    Command::new(BIN)
        .arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", fake_opencode)
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .env("SESYNC_MAX_AGE_DAYS", "3650")
        .output()
        .expect("run sesync")
}

/// A fake OpenCode CLI: every invocation is appended to oc.log, `session delete` says the
/// session is missing (the normal first-write case), `import` saves the bundle for inspection.
fn fake_opencode(sb: &Path) -> PathBuf {
    let cmd = sb.join("fake-opencode");
    fs::write(
        &cmd,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\n\
             case \"$1\" in\n\
             session) echo 'Session not found' >&2; exit 1;;\n\
             import) cp \"$2\" {bundle}; echo 'Imported session: ok';;\n\
             esac\n",
            log = sb.join("oc.log").display(),
            bundle = sb.join("imported.json").display(),
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&cmd, fs::Permissions::from_mode(0o755)).unwrap();
    cmd
}

/// A native omp journal, padded past the 512-byte minimum a session needs.
fn omp_session(sb: &Path) -> PathBuf {
    let id = "aaaaaaaa-1111-2222-3333-444444444444";
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl"));
    let header = format!(r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/proj"}}"#);
    let msg = format!(r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"user","content":"{}"}}}}"#, "hello opencode target ".repeat(30));
    fs::write(&path, format!("{header}\n{msg}\n")).unwrap();
    path
}

/// The text parts of an imported bundle, joined.
fn bundle_text(bundle: &serde_json::Value) -> String {
    bundle["messages"].as_array().unwrap().iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn an_omp_session_chosen_for_opencode_is_deleted_then_imported_and_refreshed() {
    let sb = sandbox("import");
    let path = omp_session(&sb);
    let key = path.display().to_string();
    let sid = mirror_opencode_id(&sesync::manage::conversation_id(&key));
    choose(&sb, "omp", &key, &["opencode"]);
    let fake = fake_opencode(&sb);

    let o = run(&sb, &fake);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    let log = fs::read_to_string(sb.join("oc.log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "one delete and one import:\n{log}");
    assert_eq!(lines[0], format!("session delete {sid}"), "the delete must come first:\n{log}");
    assert!(lines[1].starts_with("import "), "{log}");

    let bundle: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sb.join("imported.json")).unwrap()).unwrap();
    assert_eq!(bundle["info"]["id"], serde_json::json!(sid), "the bundle carries the deterministic id");
    assert!(bundle_text(&bundle).contains("hello opencode target"), "the conversation is carried");

    let state = State::load(&sb.join("state").join("state.json")).unwrap();
    let m = &state.sources[&key].mirrors["opencode"];
    assert_eq!(m.path, sid, "the mirror records the OpenCode session id");
    assert_eq!(m.source_hash, state.sources[&key].last_hash, "the mirror remembers the source hash");
    assert!(!state.mirror_keys.contains(&sid), "a session id is not a mirror file");

    // Nothing changed: no second import.
    let o = run(&sb, &fake);
    assert!(String::from_utf8_lossy(&o.stdout).contains("0 source(s) changed"), "{}", String::from_utf8_lossy(&o.stdout));
    assert_eq!(fs::read_to_string(sb.join("oc.log")).unwrap().lines().count(), 2, "an unchanged session is not re-imported");

    // The conversation changed: the same id is deleted and re-imported with the new content.
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str(&format!("{}\n", r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"assistant","content":"a later turn"}}"#));
    fs::write(&path, text).unwrap();
    let o = run(&sb, &fake);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    let log = fs::read_to_string(sb.join("oc.log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 4, "a refresh is delete+import again:\n{log}");
    assert_eq!(lines[2], format!("session delete {sid}"), "{log}");
    let bundle: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sb.join("imported.json")).unwrap()).unwrap();
    assert!(bundle_text(&bundle).contains("a later turn"), "the refreshed bundle carries the new turn");
}

#[test]
fn a_claude_source_is_converted_to_a_real_journal_for_the_pi_mirror() {
    let sb = sandbox("claude");
    let dir = sb.join("claude").join("projects").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    let path = dir.join("aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    let text = "hello from a claude source ".repeat(30);
    fs::write(&path, format!(
        "{{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":null,\"sessionId\":\"s1\",\"cwd\":\"/tmp/proj\",\"timestamp\":\"2026-10-01T10:00:00.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n\
         {{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":\"u1\",\"sessionId\":\"s1\",\"cwd\":\"/tmp/proj\",\"timestamp\":\"2026-10-01T10:00:02.000Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"a reply {text}\"}}]}}}}\n")).unwrap();
    choose(&sb, "claude", &path.display().to_string(), &["pi"]);

    let o = run(&sb, &fake_opencode(&sb));
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    let mut mirrors: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(sb.join("pi").join("sessions")) {
        for b in rd.flatten() {
            if let Ok(files) = fs::read_dir(b.path()) {
                mirrors.extend(files.flatten().map(|f| f.path()).filter(|p| p.extension().map_or(false, |x| x == "jsonl")));
            }
        }
    }
    assert_eq!(mirrors.len(), 1, "the chosen pi mirror must be written");
    let body = fs::read_to_string(&mirrors[0]).unwrap();
    assert!(body.contains("hello from a claude source"), "the conversation is carried:\n{body}");
    assert!(body.contains("\"type\":\"message\""), "the mirror is a journal:\n{body}");
    assert!(!body.contains("\"parentUuid\""), "the raw Claude transcript must not be copied through:\n{body}");
}

/// Un-choosing a target removes the mirror on the next pass. Nothing else did: the cleanup command
/// only plans what is still recorded and declines for OpenCode by design, so an un-ticked OpenCode
/// row kept its session (and kept showing in the TUI's MIRRORS column) with no way to drop it.
#[test]
fn un_choosing_a_target_removes_the_mirror_on_the_next_pass() {
    let sb = sandbox("unchosen-removal");
    let fake = fake_opencode(&sb);
    let src = omp_session(&sb);
    let key = src.display().to_string();
    let sid = "ses_003220097fe04c7bvvm4CnHYO6";

    // The user chose opencode once, which is why a mirror exists, and has now un-chosen it:
    // the record is there, `chosen_targets` is empty.
    fs::create_dir_all(sb.join("state")).unwrap();
    let mut state = State::default();
    let mut s = Source::new("omp", &key);
    s.mirrors.insert(
        "opencode".into(),
        sesync::state::Mirror {
            target: "opencode".into(),
            // For OpenCode the record's path is the session id: there is no file to remove.
            path: sid.into(),
            source_hash: String::new(),
            installed_hash: String::new(),
            diverged: false,
            edit_stamp: String::new(),
            target_updated: 0.0,
        },
    );
    state.sources.insert(key.clone(), s);
    state.save(&sb.join("state").join("state.json")).unwrap();

    let out = run(&sb, &fake);
    assert!(out.status.success(), "pass failed: {}", String::from_utf8_lossy(&out.stderr));

    let log = fs::read_to_string(sb.join("oc.log")).unwrap_or_default();
    assert!(
        log.contains(&format!("session delete {sid}")),
        "the copy is removed through the CLI, never by file: {log:?}"
    );

    let after = State::load(&sb.join("state").join("state.json")).unwrap();
    let s = after.sources.get(&key).expect("the source survives the pass");
    assert!(s.mirrors.is_empty(), "the mirror record is gone");
    assert!(!after.mirror_keys.contains(&sid.to_string()), "and so is its mirror_keys entry");
}
