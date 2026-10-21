//! O-penCode as a source, ported from daemon/sesync.py opencode_sessions and
//! opencode_first_prompt. Reads the real schema (session, message, part) read-only.
//! Tests use a database built with the same table and column names in a sandbox.

use sesync::harness::opencode::store::{first_prompt, sessions};
use rusqlite::Connection;
use std::fs;
use std::path::PathBuf;

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-oc-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn make_db(path: &PathBuf) {
    let c = Connection::open(path).unwrap();
    c.execute_batch(
        "create table session (id text primary key, directory text, title text,
            time_updated integer, time_archived integer);
         create table message (id text primary key, session_id text, time_created integer, data text);
         create table part (id text primary key, message_id text, time_created integer, data text);
         insert into session values ('ses_live', '/proj', 'Live title', 1792083252020, null);
         insert into session values ('ses_archived', '/proj', 'Old', 1792083252020, 1);
         insert into message values ('m1', 'ses_live', 1, '{\"role\":\"user\"}');
         insert into part values ('p1', 'm1', 1, '{\"type\":\"text\",\"text\":\"first question here\\nmore\"}');
         insert into message values ('m2', 'ses_noprompt', 1, '{\"role\":\"user\"}');
         insert into part values ('p2', 'm2', 1, '{\"type\":\"text\",\"text\":\"<command-name>x\"}');",
    ).unwrap();
}

#[test]
fn live_sessions_are_listed_archived_ones_are_not() {
    let sb = sandbox("list");
    let db = sb.join("opencode.db");
    make_db(&db);
    let got = sessions(&db);
    let ids: Vec<&str> = got.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"ses_live"), "{ids:?}");
    assert!(!ids.contains(&"ses_archived"), "archived sessions are not synced: {ids:?}");
}

#[test]
fn updated_time_is_seconds_not_milliseconds() {
    let sb = sandbox("time");
    let db = sb.join("opencode.db");
    make_db(&db);
    let s = sessions(&db).into_iter().find(|s| s.id == "ses_live").unwrap();
    assert!((s.updated - 1792083252.020).abs() < 0.001, "got {}", s.updated);
}

#[test]
fn first_prompt_is_the_first_real_user_text_line() {
    let sb = sandbox("prompt");
    let db = sb.join("opencode.db");
    make_db(&db);
    assert_eq!(first_prompt(&db, "ses_live"), "first question here");
}

#[test]
fn command_wrappers_are_not_a_prompt() {
    let sb = sandbox("wrapper");
    let db = sb.join("opencode.db");
    make_db(&db);
    assert_eq!(first_prompt(&db, "ses_noprompt"), "", "text starting with '<' is a wrapper, not a prompt");
}

#[test]
fn a_missing_database_gives_nothing_and_is_not_created() {
    let sb = sandbox("missing");
    let db = sb.join("absent.db");
    assert!(sessions(&db).is_empty());
    assert_eq!(first_prompt(&db, "ses_x"), "");
    assert!(!db.exists(), "reading must never create the database");
}

#[test]
fn the_database_is_opened_read_only() {
    let sb = sandbox("ro");
    let db = sb.join("opencode.db");
    make_db(&db);
    // A write through the same path the reader uses must fail, so the reader cannot
    // change the store. This pins the mode=ro open, not just the queries.
    assert!(sesync::harness::opencode::store::write_probe(&db).is_err(), "the reader's connection must be read-only");
}

/// A store whose session table has a parent_id column, as real OpenCode stores do:
/// one root session and one child session that a subagent created under it.
fn make_db_with_child(path: &PathBuf) {
    let c = Connection::open(path).unwrap();
    c.execute_batch(
        "create table session (id text primary key, parent_id text, directory text, title text,
            time_updated integer, time_archived integer);
         insert into session values ('ses_root', null, '/proj', 'Root task', 1792083252020, null);
         insert into session values ('ses_child', 'ses_root', '/proj', 'Subagent run', 1792083252020, null);",
    ).unwrap();
}

#[test]
fn a_subagent_child_session_is_not_listed_as_its_own_conversation() {
    // A subagent run is part of its parent; mirroring it separately would put an
    // unrelated conversation in every other harness's list.
    let sb = sandbox("child");
    let db = sb.join("opencode.db");
    make_db_with_child(&db);
    let ids: Vec<String> = sessions(&db).into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec!["ses_root".to_string()], "only the root session is a source");
}

#[test]
fn a_store_without_a_parent_column_still_lists_its_sessions() {
    // Older stores have no parent_id column; reading them must not fail.
    let sb = sandbox("noparentcol");
    let db = sb.join("opencode.db");
    make_db(&db);
    assert!(sessions(&db).iter().any(|s| s.id == "ses_live"));
}
