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

#[cfg(unix)]
#[path = "support/fake_cli.rs"]
mod fake_cli;

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-edge-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn a_database_with_no_session_table_gives_no_sessions_and_does_not_crash() {
    let sb = sandbox("noschema");
    let db = sb.join("opencode.db");
    Connection::open(&db).unwrap().execute_batch("create table other (x integer);").unwrap();
    assert!(sessions(&db).is_empty(), "a store without the session table must read as empty");
}

#[test]
fn a_prompt_row_with_broken_json_is_skipped_and_the_next_prompt_is_used() {
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
    let sb = sandbox("nofile");
    let src = sb.join("src.jsonl");
    fs::write(&src, "{\"type\":\"session\",\"version\":3,\"id\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"timestamp\":\"2026-10-01T10:00:00.000Z\",\"cwd\":\"/tmp\"}\n").unwrap();
    // A converter that exits 0 but prints no output path.
    let fake = sb.join("silent-sesync_conv");
    fake_cli::stage(&fake, "#!/bin/sh\nexit 0\n");
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
fn a_lock_command_that_cannot_run_skips_the_work_fail_closed() {
    // The lock tool is installed but cannot run. A write without the claim could race a
    // live turn, so the work must not run; the reason is reported instead.
    let sb = sandbox("failclosed");
    let fake = sb.join("broken-sesync_lock");
    fs::write(&fake, "not a program").unwrap();
    let mut ran = false;
    let out = with_claim(&fake, Some(&sb), "conv", "daemon", 1, "/src", || ran = true);
    assert!(!ran, "the work must not run when the lock tool cannot claim");
    assert!(matches!(&out, ClaimOutcome::Failed(e) if e.starts_with("cannot run lock tool")), "got {out:?}");
}
