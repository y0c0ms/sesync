//! Mirror targets that need more than a file copy: the O-penCode import and the Claude
//! transcript source. A chosen session is converted and loaded with the CLI (a fake here that
//! records its invocations, so no real harness runs): the bundle is first proven importable in a
//! throwaway store, then the session is deleted and imported, so a refresh replaces the same
//! session and a refused bundle never costs the existing one. Claude transcripts must be
//! converted to a journal before any target converter sees them.

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

/// The OpenCode store a test may create; absent unless the test writes it.
fn store(sb: &Path) -> PathBuf {
    sb.join("opencode").join("opencode.db")
}

fn run(sb: &Path, opencode_cli: &Path) -> Output {
    Command::new(BIN)
        .arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", store(sb))
        .env("OPENCODE_BIN", opencode_cli)
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .env("SESYNC_MAX_AGE_DAYS", "3650")
        .output()
        .expect("run sesync")
}

/// A fake OpenCode CLI: every invocation is appended to oc.log, `import` saves the bundle for
/// inspection. `session delete` either says the session is missing (the normal first write) or,
/// with `deletes`, removes the store file, so the daemon can see the session is gone.
#[cfg(unix)]
fn fake_opencode(sb: &Path, deletes: bool) -> PathBuf {
    let cmd = sb.join("fake-opencode");
    let delete = if deletes {
        format!("rm -f '{}'; echo 'Session deleted' >&2; exit 0", store(sb).display())
    } else {
        "echo 'Session not found' >&2; exit 1".to_string()
    };
    fs::write(
        &cmd,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\n\
             case \"$1\" in\n\
             session) {delete};;\n\
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

#[cfg(unix)]
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

/// An OpenCode store holding one session with one user message, the way an import leaves it.
#[cfg(unix)]
fn store_with_session(sb: &Path, sid: &str) {
    let db = store(sb);
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let con = rusqlite::Connection::open(&db).unwrap();
    con.execute_batch(
        "create table session (id text primary key, directory text, title text, time_created integer, \
                               time_updated integer, time_archived integer, parent_id text);
         create table message (id text primary key, session_id text, time_created integer, time_updated integer, data text);
         create table part (id text primary key, message_id text, session_id text, time_created integer, time_updated integer, data text);",
    )
    .unwrap();
    con.execute("insert into session values (?1, '/tmp/proj', 'copy', 1, 1000, null, null)", [sid]).unwrap();
    con.execute("insert into message values ('msg_1', ?1, 1000, 1000, '{\"role\":\"user\"}')", [sid]).unwrap();
    con.execute("insert into part values ('prt_1', 'msg_1', ?1, 1000, 1000, '{\"type\":\"text\",\"text\":\"hello\"}')", [sid]).unwrap();
}

/// The text parts of an imported bundle, joined.
#[cfg(unix)]
fn bundle_text(bundle: &serde_json::Value) -> String {
    bundle["messages"].as_array().unwrap().iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A state where the omp session once chose OpenCode and has un-chosen it, with the copy's
/// record as an import left it.
#[cfg(unix)]
fn unchosen_opencode_copy(sb: &Path, key: &str, sid: &str, installed_hash: &str) {
    fs::create_dir_all(sb.join("state")).unwrap();
    let mut state = State::default();
    let mut s = Source::new("omp", key);
    s.mirrors.insert("opencode".into(), sesync::state::Mirror {
        target: "opencode".into(),
        // For OpenCode the record's path is the session id: there is no file to remove.
        path: sid.into(),
        installed_hash: installed_hash.into(),
        target_updated: 1.0,
        ..Default::default()
    });
    state.sources.insert(key.to_string(), s);
    state.save(&sb.join("state").join("state.json")).unwrap();
}

#[cfg(unix)]
#[test]
fn an_omp_session_chosen_for_opencode_is_validated_deleted_then_imported_and_refreshed() {
    let sb = sandbox("import");
    let path = omp_session(&sb);
    let key = path.display().to_string();
    let sid = sesync::harness::opencode::mirror_opencode_id(&sesync::manage::conversation_id(&key));
    choose(&sb, "omp", &key, &["opencode"]);
    let fake = fake_opencode(&sb, false);

    let o = run(&sb, &fake);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    let log = fs::read_to_string(sb.join("oc.log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 3, "a validating import, the delete, the import:\n{log}");
    assert!(lines[0].starts_with("import "), "the bundle is proven importable before anything is deleted:\n{log}");
    assert_eq!(lines[1], format!("session delete {sid}"), "{log}");
    assert!(lines[2].starts_with("import "), "{log}");

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
    assert_eq!(fs::read_to_string(sb.join("oc.log")).unwrap().lines().count(), 3, "an unchanged session is not re-imported");

    // The conversation changed: the same id is deleted and re-imported with the new content.
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str(&format!("{}\n", r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"assistant","content":"a later turn"}}"#));
    fs::write(&path, text).unwrap();
    let o = run(&sb, &fake);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    let log = fs::read_to_string(sb.join("oc.log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 6, "a refresh validates, deletes and imports again:\n{log}");
    assert_eq!(lines[4], format!("session delete {sid}"), "{log}");
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

    // OpenCode plays no part here: a missing CLI must not matter for a pi mirror.
    let o = run(&sb, &sb.join("no-opencode-cli"));
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

/// Un-choosing a target removes the mirror on the next pass, when the copy still holds exactly
/// what sesync imported. Nothing else did: the cleanup command only plans what is still recorded
/// and declines for OpenCode by design.
#[cfg(unix)]
#[test]
fn un_choosing_opencode_deletes_the_copy_sesync_still_owns() {
    let sb = sandbox("unchosen-removal");
    let fake = fake_opencode(&sb, true);
    let key = omp_session(&sb).display().to_string();
    let sid = "ses_003220097fe04c7bvvm4CnHYO6";
    store_with_session(&sb, sid);
    let ours = sesync::harness::opencode::store::content_fingerprint(&store(&sb), sid).expect("the store holds the copy");
    unchosen_opencode_copy(&sb, &key, sid, &ours);

    let out = run(&sb, &fake);
    assert!(out.status.success(), "pass failed: {}", String::from_utf8_lossy(&out.stderr));
    let log = fs::read_to_string(sb.join("oc.log")).unwrap_or_default();
    assert!(log.contains(&format!("session delete {sid}")), "the copy is removed through the CLI, never by file: {log:?}");
    let after = State::load(&sb.join("state").join("state.json")).unwrap();
    let s = after.sources.get(&key).expect("the source survives the pass");
    assert!(s.mirrors.is_empty(), "the mirror record is gone");
}

/// The other half of the rule. A copy somebody continued in OpenCode, or the old source a
/// promotion demoted (the user's own conversation), is never deleted: sesync stops claiming it
/// and the session stays, to show up as a session of its own.
#[cfg(unix)]
#[test]
fn un_choosing_opencode_releases_a_copy_sesync_did_not_write() {
    let sb = sandbox("unchosen-release");
    let fake = fake_opencode(&sb, true);
    let key = omp_session(&sb).display().to_string();
    let sid = "ses_003220097fe04c7bvvm4CnHYO7";
    store_with_session(&sb, sid);
    // What sesync imported had one message less: the user answered in the copy since.
    unchosen_opencode_copy(&sb, &key, sid, "oc:0:0:0");

    let out = run(&sb, &fake);
    assert!(out.status.success(), "pass failed: {}", String::from_utf8_lossy(&out.stderr));
    let log = fs::read_to_string(sb.join("oc.log")).unwrap_or_default();
    assert!(!log.contains("session delete"), "a continued copy must never be deleted: {log:?}");
    assert!(store(&sb).exists() && sesync::harness::opencode::store::content_fingerprint(&store(&sb), sid).is_some(),
        "the session is still in the store");
    let after = State::load(&sb.join("state").join("state.json")).unwrap();
    assert!(after.sources[&key].mirrors.is_empty(), "the record is released");
    assert!(String::from_utf8_lossy(&out.stdout).contains("released opencode"),
        "{}", String::from_utf8_lossy(&out.stdout));
}
