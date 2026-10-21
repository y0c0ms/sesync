//! End-to-end tests for the sesync binary, the Rust replacement for
//! `python daemon/sesync.py --status | --once`. Every store lives in a
//! private temp directory; the real omp, pi, claude and opencode stores are never
//! touched. The converter is the real sesync_conv binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-cli-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Run the binary with every store pointed into the sandbox.
fn run(sb: &Path, extra: &[&str], converter: &Path) -> Output {
    Command::new(BIN)
        .args(extra)
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("SESYNC_CONV", converter)
        // Point O-penCode at an empty sandbox path. Without this the binary reads the
        // user's real opencode.db and mirrors their live sessions.
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        // And never its real CLI: a pass that reached it would import into the user's store.
        .env("OPENCODE_BIN", sb.join("no-opencode-cli"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
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
    let msg = format!(r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"user","content":"{}"}}}}"#, "hello ".repeat(120));
    fs::write(&path, format!("{header}\n{msg}\n")).unwrap();
    path
}

fn pi_mirrors(sb: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let root = sb.join("pi").join("sessions");
    if let Ok(rd) = fs::read_dir(&root) {
        for b in rd.flatten() {
            if let Ok(files) = fs::read_dir(b.path()) {
                out.extend(files.flatten().map(|f| f.path()));
            }
        }
    }
    out
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn status_on_an_empty_state_prints_nothing_and_succeeds() {
    let sb = sandbox("status-empty");
    let o = run(&sb, &["--status"], &sesync_conv());
    assert!(o.status.success(), "{o:?}");
    assert_eq!(stdout(&o), "");
}

#[test]
fn status_lists_sources_from_the_state_file() {
    let sb = sandbox("status-list");
    fs::create_dir_all(sb.join("state")).unwrap();
    fs::write(sb.join("state").join("state.json"), r#"{"sources":{"/x/2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl":{"fmt":"omp","key":"k","last_seen_change":5.0,"last_hash":"","last_stamp":"","failures":2,"mirrors":{}}},"mirror_keys":[]}"#).unwrap();
    let o = run(&sb, &["--status"], &sesync_conv());
    assert!(o.status.success(), "{o:?}");
    let out = stdout(&o);
    assert!(out.contains("omp       2026-10-01T10-00-00-000Z_"), "{out}");
    assert!(out.contains("failures: 2"), "{out}");
}

#[test]
fn once_records_an_omp_session_and_writes_no_copy_into_pi() {
    let sb = sandbox("once-record");
    omp_session(&sb);
    fs::create_dir_all(sb.join("pi")).unwrap();
    let o = run(&sb, &["--once"], &sesync_conv());
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    assert!(stdout(&o).contains("one pass done, 1 source(s) changed"), "{}", stdout(&o));
    assert!(pi_mirrors(&sb).is_empty(), "a pass writes no copy into the pi store");
    let state = fs::read_to_string(sb.join("state").join("state.json")).unwrap();
    assert!(state.contains("aaaaaaaa-1111-2222-3333-444444444444"), "the session is recorded as a source");
}

#[test]
fn a_second_pass_with_no_changes_reports_nothing_changed() {
    let sb = sandbox("once-idle");
    omp_session(&sb);
    fs::create_dir_all(sb.join("pi")).unwrap();
    assert!(run(&sb, &["--once"], &sesync_conv()).status.success());
    let o = run(&sb, &["--once"], &sesync_conv());
    assert!(stdout(&o).contains("one pass done, 0 source(s) changed"), "{}", stdout(&o));
    assert!(pi_mirrors(&sb).is_empty(), "an unchanged session writes nothing");
}

#[test]
fn a_pass_never_runs_the_converter() {
    let sb = sandbox("once-no-converter");
    omp_session(&sb);
    fs::create_dir_all(sb.join("pi")).unwrap();
    // A converter that always fails would count failures if the pass ran it.
    let o = run(&sb, &["--once"], Path::new("/bin/false"));
    assert!(o.status.success(), "a pass has no converter to fail: {o:?}");
    assert!(pi_mirrors(&sb).is_empty(), "no copy is written, so no conversion runs");
    let st = run(&sb, &["--status"], &sesync_conv());
    assert!(!stdout(&st).contains("failures:"), "{}", stdout(&st));
}

#[test]
fn a_corrupt_state_file_stops_the_daemon_instead_of_forgetting_mirrors() {
    let sb = sandbox("corrupt");
    fs::create_dir_all(sb.join("state")).unwrap();
    fs::write(sb.join("state").join("state.json"), "{not json").unwrap();
    let o = run(&sb, &["--once"], &sesync_conv());
    assert!(!o.status.success(), "a corrupt state must be an error, not an empty state");
}

#[test]
fn a_title_renamed_in_the_harness_reaches_the_mirror_on_the_next_pass() {
    use sesync::harness::journal as titles;
    let sb = sandbox("rename-fast");
    let src = omp_session(&sb);
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    assert!(run(&sb, &["--once"], &sesync_conv()).status.success());

    // The TUI's mirror choice is just this field; set it the way the TUI saves it.
    let state_path = sb.join("state").join("state.json");
    let mut st: serde_json::Value = serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    let key = src.display().to_string();
    st["sources"][key.as_str()]["chosen_targets"] = serde_json::json!(["pi"]);
    fs::write(&state_path, st.to_string()).unwrap();
    assert!(run(&sb, &["--once"], &sesync_conv()).status.success());
    let mirrors = pi_mirrors(&sb);
    assert_eq!(mirrors.len(), 1, "the chosen target must be written");

    // omp's own rename appends a title record. The very next pass must see it and update the mirror.
    let mut text = fs::read_to_string(&src).unwrap();
    text.push_str("{\"type\":\"title\",\"title\":\"Renamed Fast\"}\n");
    fs::write(&src, text).unwrap();
    let o = run(&sb, &["--once"], &sesync_conv());
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(stdout(&o).contains("1 source(s) changed"), "{}", stdout(&o));
    let mirror = fs::read_to_string(&mirrors[0]).unwrap();
    let lines: Vec<&str> = mirror.lines().collect();
    assert_eq!(titles::omp_title_from_lines(&lines).0, "Renamed Fast",
               "the mirror must carry the title renamed in the harness");
}

#[test]
fn a_choice_saved_while_the_daemon_runs_is_picked_up_on_the_next_pass() {
    let sb = sandbox("live-choice");
    let src = omp_session(&sb);
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    let mut child = Command::new(BIN)
        .arg("--watch")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_LOCK_DIR", sb.join("locks"))
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_QUIET", "0")
        .env("SESYNC_PASSES", "600")
        .env("SESYNC_POLL", "0.2")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start the daemon");
    let state_path = sb.join("state").join("state.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !state_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(state_path.exists(), "the running daemon wrote its first state");

    // Exactly what the TUI does now: load, set the choice, save only the user-owned fields.
    let mut st = sesync::state::State::load(&state_path).unwrap();
    let key = src.display().to_string();
    st.sources.entry(key.clone())
        .or_insert_with(|| sesync::state::Source::new("omp", &key))
        .chosen_targets.insert("pi".into());
    st.save_user(&state_path).unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while pi_mirrors(&sb).is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let mirrors = pi_mirrors(&sb);
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(mirrors.len(), 1, "the running daemon must pick up the choice and write the mirror");
    let after = sesync::state::State::load(&state_path).unwrap();
    assert!(after.sources[&key].chosen_targets.contains("pi"),
        "the daemon's own saves must not erase the choice");
}

#[cfg(unix)]
#[test]
fn an_opencode_session_mirrors_into_the_chosen_harness() {
    let sb = sandbox("opencode-mirror");
    let db = sb.join("opencode.db");
    let old_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
        - 30 * 86_400_000;
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(&format!(
            "create table session (id text primary key, directory text, title text,
                time_updated integer, time_archived integer);
             insert into session values ('ses_live', '/tmp/proj', 'OC Mirror Title', {old_ms}, null);"
        )).unwrap();
    }
    // The fake CLI answers `export ses_live` with a real export payload; no harness CLI runs.
    let json = r#"{"info":{"id":"ses_live","directory":"/tmp/proj","title":"OC Mirror Title","time":{"created":1,"updated":1}},"messages":[{"info":{"id":"m1","role":"user","sessionID":"ses_live","time":{"created":1}},"parts":[{"id":"p1","type":"text","text":"opencode mirror text","messageID":"m1","sessionID":"ses_live"}]}]}"#;
    let fake = sb.join("fake-opencode");
    fs::write(&fake, format!("#!/bin/sh\ncat <<'EOF'\n{json}\nEOF\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();

    let run = |max_age: &str| {
        Command::new(BIN)
            .arg("--once")
            .env("SESYNC_DIR", sb.join("state"))
            .env("OMP_AGENT_DIR", sb.join("omp"))
            .env("PI_AGENT_DIR", sb.join("pi"))
            .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
            .env("SESYNC_CONV", sesync_conv())
            .env("OPENCODE_DB", &db)
            .env("OPENCODE_BIN", &fake)
            .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
            .env("SESYNC_QUIET", "0")
            .env("SESYNC_MAX_AGE_DAYS", max_age)
            .output()
            .expect("run sesync")
    };

    // The first pass records the source while the age window is open.
    let o = run("60");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let state_path = sb.join("state").join("state.json");
    let mut st = sesync::state::State::load(&state_path).unwrap();
    assert!(st.sources.contains_key("ses_live"), "the opencode source is recorded");
    st.sources.get_mut("ses_live").unwrap().chosen_targets.insert("pi".into());
    st.save_user(&state_path).unwrap();

    // The window is now closed: a pending choice must still be mirrored, like any other source.
    let o = run("0");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let mirrors = pi_mirrors(&sb);
    assert_eq!(mirrors.len(), 1, "the opencode session must be mirrored into pi");
    let body = fs::read_to_string(&mirrors[0]).unwrap();
    assert!(body.contains("opencode mirror text"), "the conversation must be carried: {body}");
    assert!(body.contains("OC Mirror Title"), "the title must be carried: {body}");
}

/// The TUI's SYNCED column and details pane read what the daemon records per target: when the
/// last write landed, and the error when it did not. One pass with one target that can be written
/// and one that cannot (OpenCode's CLI is missing here, so nothing reaches a real store). An
/// un-chosen target loses its record on the next pass, changed source or not.
#[test]
fn a_pass_records_each_targets_last_write_and_its_error() {
    let sb = sandbox("per-target");
    let src = omp_session(&sb);
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    let pass = || Command::new(BIN)
        .arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("SESYNC_CONV", sesync_conv())
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", sb.join("no-opencode-cli"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .output()
        .expect("run sesync");
    assert!(pass().status.success());
    let state_path = sb.join("state").join("state.json");
    let key = src.display().to_string();
    let mut st = sesync::state::State::load(&state_path).unwrap();
    st.sources.get_mut(&key).expect("the session is recorded").chosen_targets =
        ["opencode".to_string(), "pi".to_string()].into_iter().collect();
    st.save_user(&state_path).unwrap();

    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let o = pass();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let st = sesync::state::State::load(&state_path).unwrap();
    let sync = &st.sources[&key].sync;
    let pi = sync.get("pi").expect("the written target has a record");
    assert!(pi.ok_at >= before.floor(), "the pi write is timed: {pi:?}");
    assert!(pi.problem.is_empty(), "{pi:?}");
    let oc = sync.get("opencode").expect("the failed target has a record");
    assert_eq!(oc.ok_at, 0.0, "opencode was never written: {oc:?}");
    assert!(!oc.problem.is_empty() && oc.problem_at >= before.floor(), "the error and its time are kept: {oc:?}");
    assert!(!sesync::session_cols::is_transient(&oc.problem), "a missing CLI is a failure, not a busy copy: {oc:?}");

    // Un-choosing the failed target drops its record on the next pass, though the source did
    // not change: the TUI stops reporting a failure the user no longer asked for.
    let mut st = sesync::state::State::load(&state_path).unwrap();
    st.sources.get_mut(&key).unwrap().chosen_targets.remove("opencode");
    st.save_user(&state_path).unwrap();
    let o = pass();
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("0 source(s) changed"), "{}", String::from_utf8_lossy(&o.stdout));
    let st = sesync::state::State::load(&state_path).unwrap();
    assert!(!st.sources[&key].sync.contains_key("opencode"), "{:?}", st.sources[&key].sync);
    assert!(st.sources[&key].sync.contains_key("pi"), "the chosen target keeps its record");
}

/// Copies recorded before the per-target record existed: a file copy is dated once from its own
/// modified time; an OpenCode copy is not, because its row time is the imported conversation's.
#[test]
fn a_pass_dates_an_older_file_copy_from_the_file_and_leaves_an_opencode_copy_undated() {
    use sesync::state::{Mirror, State};
    let sb = sandbox("backfill");
    let src = omp_session(&sb);
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    assert!(run(&sb, &["--once"], &sesync_conv()).status.success());
    let state_path = sb.join("state").join("state.json");
    let key = src.display().to_string();

    // The pi copy sits in the session's own bucket and holds what sesync wrote; it is old.
    let copy = sb.join("pi").join("sessions").join("--tmp-proj--").join("copy.jsonl");
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::write(&copy, fs::read(&src).unwrap()).unwrap();
    let written = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    fs::File::options().write(true).open(&copy).unwrap().set_modified(written).unwrap();
    let mut st = State::load(&state_path).unwrap();
    let s = st.sources.get_mut(&key).expect("the session is recorded");
    s.chosen_targets = ["opencode".to_string(), "pi".to_string()].into_iter().collect();
    s.mirrors.insert("pi".into(), Mirror {
        target: "pi".into(), path: copy.display().to_string(), source_hash: s.last_hash.clone(),
        installed_hash: sesync::mirror::file_hash(&copy).unwrap(),
        edit_stamp: sesync::mirror::copy_stamp("pi", &copy), ..Default::default()
    });
    s.mirrors.insert("opencode".into(), Mirror {
        target: "opencode".into(), path: "ses_old".into(), source_hash: s.last_hash.clone(),
        installed_hash: "fingerprint".into(), target_updated: 1_500_000_000.0, ..Default::default()
    });
    st.save(&state_path).unwrap();

    let o = run(&sb, &["--once"], &sesync_conv());
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("0 source(s) changed"),
        "nothing is rewritten, so the time can only come from the copy: {}", String::from_utf8_lossy(&o.stdout));
    let st = State::load(&state_path).unwrap();
    let sync = &st.sources[&key].sync;
    let pi = sync.get("pi").expect("the file copy is dated");
    assert!((pi.ok_at - 1_600_000_000.0).abs() < 1.0, "dated from the file, not the pass: {pi:?}");
    assert!(!sync.contains_key("opencode"), "an OpenCode copy waits for its next write: {sync:?}");
}
