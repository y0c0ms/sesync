//! Error and edge paths in the readers, the installers and the lock client. Each is a
//! deliberate rule: a broken source yields nothing rather than a crash; a converter that
//! produces no file is an error; a lock command that fails unexpectedly does not block
//! the daemon (fail open, as the Python did).

use sesync::harness::opencode::store::{first_prompt, sessions};
#[cfg(unix)]
use sesync::harness::{Location, Tools};
use sesync::lock::{with_claim, ClaimOutcome};
use rusqlite::Connection;
use std::fs;
use std::path::PathBuf;

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-edge-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn a_database_with_no_session_table_gives_no_sessions_and_does_not_crash() {
    let _exec = exec_guard();
    let sb = sandbox("noschema");
    let db = sb.join("opencode.db");
    Connection::open(&db).unwrap().execute_batch("create table other (x integer);").unwrap();
    assert!(sessions(&db).is_empty(), "a store without the session table must read as empty");
}

#[test]
fn a_prompt_row_with_broken_json_is_skipped_and_the_next_prompt_is_used() {
    let _exec = exec_guard();
    let sb = sandbox("badjson");
    let db = sb.join("opencode.db");
    let c = Connection::open(&db).unwrap();
    c.execute_batch(
        "create table message (id text primary key, session_id text, time_created integer, data text);
         create table part (id text primary key, message_id text, time_created integer, data text);
         insert into message values ('m1','s',1,'not json');
         insert into part values ('p1','m1',1,'{\"type\":\"text\",\"text\":\"ignored\"}');
         insert into message values ('m2','s',2,'{\"role\":\"user\"}');
         insert into part values ('p2','m2',2,'{\"type\":\"text\",\"text\":\"the real prompt\"}');",
    ).unwrap();
    drop(c);
    assert_eq!(first_prompt(&db, "s"), "the real prompt", "a broken row must not stop the search");
}

#[cfg(unix)]
#[test]
fn a_converter_that_writes_no_file_is_an_error_and_leaves_no_mirror() {
    let _exec = exec_guard();
    let sb = sandbox("nofile");
    let src = sb.join("src.jsonl");
    fs::write(&src, "{\"type\":\"session\",\"version\":3,\"id\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"timestamp\":\"2026-10-01T10:00:00.000Z\",\"cwd\":\"/tmp\"}\n").unwrap();
    // A converter that exits 0 but prints no output path.
    let fake = sb.join("silent-sesync_conv");
    fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
    stage_exec(&fake);
    let store = sb.join("pi-agent");
    let pi = sesync::harness::get("pi").expect("pi is a harness");
    let r = pi.install(&Location { root: store.clone(), cli: None }, &Tools { home: &sb, sesync_conv: &fake }, &src, "id-x", "t");
    let err = r.expect_err("a converter that prints no file must be an error");
    assert!(err.contains("printed no output file"),
        "the guard must be what stops it, not a later failure: {err}");
    assert!(!store.join("sessions").exists() || fs::read_dir(store.join("sessions")).unwrap().next().is_none(),
        "nothing may be written into the store");
}

#[test]
fn a_lock_command_that_fails_unexpectedly_runs_the_work_fail_open() {
    let _exec = exec_guard();
    // The daemon never blocks on the lock layer. A lock binary that cannot run (here, a
    // path that is not executable) must let the work proceed, as the Python does.
    let sb = sandbox("failopen");
    let fake = sb.join("broken-sesync_lock");
    fs::write(&fake, "not a program").unwrap();
    let mut ran = false;
    let out = with_claim(&fake, Some(&sb), "conv", "daemon", 1, "/src", || ran = true);
    assert!(ran, "the work must run when the lock command fails");
    assert_eq!(out, ClaimOutcome::Ran(()));
}

/// Make a fake CLI executable. Callers hold exec_guard(), so no fork races the write.
#[cfg(unix)]
#[allow(dead_code)]
fn stage_exec(p: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Writing an executable while another test thread forks can make that exec fail with
/// "Text file busy" (ETXTBSY): the forked child briefly inherits the open write handle.
/// Every test in this file that writes a fake CLI or runs one holds this lock, so no
/// fork happens while a script is open for writing.
#[allow(dead_code)]
fn exec_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
