//! "(missing)" rows and deleting sessions in the TUI. A deleted session's row goes, not to
//! "(missing)": the record is forgotten in the TUI at once, in the saved state, and by the daemon,
//! which must not bring it back; its copies stay and are listed as sessions of their own. A
//! session is missing only when its harness no longer has it. Sandboxes only.

use sesync::app::{App, Ev};
use sesync::manage::Roots;
use sesync::state::{Mirror, Source, State};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-delete-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn journal(path: &Path, id: &str, title: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let header = format!(r#"{{"type":"session","version":3,"id":"{id}","cwd":"/tmp","title":"{title}"}}"#);
    let msg = format!(r#"{{"type":"message","id":"m1","timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"user","content":"{}"}}}}"#, "hello ".repeat(120));
    fs::write(path, format!("{header}\n{msg}\n")).unwrap();
}

/// An omp session with a pi copy sesync wrote, recorded the way the daemon records them.
fn session_with_copy(sb: &Path) -> (State, String, String) {
    let src = sb.join("omp").join("sessions").join("-tmp").join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    let copy = sb.join("pi").join("sessions").join("--tmp--").join("2026-10-01T10-00-00-000Z_bbbbbbbb-1111-2222-3333-444444444444.jsonl");
    journal(&src, "aaaaaaaa-1111-2222-3333-444444444444", "Going away");
    journal(&copy, "bbbbbbbb-1111-2222-3333-444444444444", "Going away");
    let (src, copy) = (src.display().to_string(), copy.display().to_string());
    let mut state = State::default();
    let mut s = Source::new("omp", &src);
    s.chosen_targets.insert("pi".into());
    s.mirrors.insert("pi".into(), Mirror { target: "pi".into(), path: copy.clone(), installed_hash: "h".into(), ..Default::default() });
    state.sources.insert(src.clone(), s);
    state.mirror_keys.insert(copy.clone());
    (state, src, copy)
}

fn app(sb: &Path, state: State) -> App {
    App::new(Roots::under(sb, sb.join("no-sesync-conv")), state, sb.join("locks"), sb.join("no-sesync_lock"))
}

/// (harness, key, missing) for every listed row.
fn rows(a: &App) -> Vec<(String, String, bool)> {
    a.visible_rows().iter().map(|g| (g.fmt.clone(), g.key.clone(), g.orphan)).collect()
}

fn delete_highlighted(a: &mut App) {
    let now = Instant::now();
    a.handle(Ev::CtrlD, now);
    a.handle(Ev::CtrlD, now);
    a.handle(Ev::Char('y'), now);
}

#[test]
fn a_deleted_session_leaves_the_list_and_its_copy_is_listed_on_its_own() {
    let sb = sandbox("tui");
    let (state, src, copy) = session_with_copy(&sb);
    let mut a = app(&sb, state);
    assert_eq!(rows(&a), vec![("omp".to_string(), src.clone(), false)], "the copy is shown under its source");

    delete_highlighted(&mut a);
    assert!(!Path::new(&src).exists(), "the session's file is deleted");
    assert!(Path::new(&copy).exists(), "its copy stays, as the dialog says");
    let listed = rows(&a);
    assert!(listed.iter().all(|(_, k, _)| k != &src), "no row for the deleted session, not even as missing: {listed:?}");
    assert!(listed.contains(&("pi".to_string(), copy.clone(), false)), "the copy is a session of its own now: {listed:?}");
    assert!(a.take_user_dirty(), "the delete is saved for the daemon");

    // What the TUI saves, and what the next start reads.
    let file = sb.join("state.json");
    a.save_user(&file).unwrap();
    let saved = State::load(&file).unwrap();
    assert!(!saved.sources.contains_key(&src), "the record is gone from the file");
    assert!(saved.deleted.contains_key(&src), "the delete is recorded for the daemon");
    assert!(!saved.mirror_keys.contains(&copy), "the copy is no longer hidden as a copy");
    let again = app(&sb, saved);
    assert!(rows(&again).iter().all(|(_, k, _)| k != &src), "a restart does not bring it back");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_missing_row_goes_away_when_it_is_deleted() {
    let sb = sandbox("orphan");
    let (state, src, _) = session_with_copy(&sb);
    // Deleted outside sesync: the record remains and the row says "(missing)".
    fs::remove_file(&src).unwrap();
    let mut a = app(&sb, state);
    assert!(rows(&a).contains(&("omp".to_string(), src.clone(), true)), "{:?}", rows(&a));
    delete_highlighted(&mut a);
    assert!(rows(&a).iter().all(|(_, k, _)| k != &src), "deleting a missing row forgets it: {:?}", rows(&a));
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn an_opencode_session_is_missing_only_when_its_store_no_longer_lists_it() {
    let sb = sandbox("opencode");
    let db = sb.join("opencode").join("opencode.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let con = rusqlite::Connection::open(&db).unwrap();
    con.execute_batch(
        "create table session (id text primary key, directory text, title text, time_created integer,
                               time_updated integer, time_archived integer, parent_id text);
         insert into session values ('ses_live', '/p', 'Live one', 1, 1, null, null);",
    ).unwrap();
    drop(con);
    let mut state = State::default();
    for id in ["ses_live", "ses_gone"] {
        let mut s = Source::new("opencode", id);
        s.title = id.to_string();
        state.sources.insert(id.to_string(), s);
    }
    let a = app(&sb, state);
    let listed = rows(&a);
    assert!(listed.contains(&("opencode".to_string(), "ses_live".to_string(), false)), "a session in the store is not missing: {listed:?}");
    assert!(listed.contains(&("opencode".to_string(), "ses_gone".to_string(), true)), "one the store dropped is: {listed:?}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_session_whose_file_is_back_is_kept_and_a_delete_is_remembered_for_a_day() {
    let sb = sandbox("rules");
    let (mut state, src, copy) = session_with_copy(&sb);
    let now = 2_000_000_000.0;
    state.deleted.insert(src.clone(), now - 60.0);
    assert!(state.forget_deleted(now).is_empty(), "the file is still there: nothing is forgotten");
    assert!(state.sources.contains_key(&src) && state.mirror_keys.contains(&copy));

    fs::remove_file(&src).unwrap();
    assert_eq!(state.forget_deleted(now).len(), 1);
    assert!(!state.sources.contains_key(&src) && !state.mirror_keys.contains(&copy));
    assert!(state.deleted.contains_key(&src), "the delete stays recorded for a daemon that has not seen it");
    state.forget_deleted(now + 2.0 * 86_400.0);
    assert!(state.deleted.is_empty(), "after a day the record of the delete is dropped");
    let _ = fs::remove_dir_all(&sb);
}

fn run_once(sb: &Path) -> Output {
    Command::new(BIN)
        .arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("SESYNC_CONV", PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", sb.join("no-opencode-cli"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .output()
        .expect("run sesync")
}

#[test]
fn the_daemon_forgets_a_session_deleted_in_the_tui_and_does_not_bring_it_back() {
    let sb = sandbox("daemon");
    let (mut state, src, copy) = session_with_copy(&sb);
    // The daemon's file still holds the record (it has not run since the delete), and the TUI's
    // save added the delete. The file itself is gone.
    fs::remove_file(&src).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    state.deleted.insert(src.clone(), now);
    let file = sb.join("state").join("state.json");
    state.save(&file).unwrap();

    let out = run_once(&sb);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("forgot the deleted session"),
        "{}", String::from_utf8_lossy(&out.stderr));
    let after = State::load(&file).unwrap();
    assert!(!after.sources.contains_key(&src), "the daemon drops the record and saves it dropped");
    assert!(!after.mirror_keys.contains(&copy), "the copy is released");
    assert_eq!(after.sources.get(&copy).map(|s| s.fmt.as_str()), Some("pi"), "the copy is a session of its own");

    let again = run_once(&sb);
    assert!(again.status.success());
    assert!(!State::load(&file).unwrap().sources.contains_key(&src), "a later pass does not bring it back");
    let _ = fs::remove_dir_all(&sb);
}
