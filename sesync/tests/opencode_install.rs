//! O-penCode mirror install, ported from daemon/sesync.py install_opencode.
//! Converts with the real sesync_conv to-opencode, then imports with `opencode import`.
//! Every test runs the real CLI with XDG_* pointed at a sandbox, so the real store
//! is never written. Skipped when the CLI is not installed.

use sesync::harness::opencode::store::install_opencode;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-ocinst-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn opencode_cli() -> Option<PathBuf> {
    // Real harness CLIs run only when HS_E2E=1. Plain `cargo test` never starts them,
    // so a local run cannot touch a live session store or call a model.
    if std::env::var("HS_E2E").map_or(true, |v| v != "1") {
        return None;
    }
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p).map(|d| d.join("opencode")).find(|c| c.is_file())
    })
}

/// Point every XDG store at the sandbox, so the import lands there and nowhere else.
fn isolate<'a>(cmd: &'a mut Command, sb: &PathBuf) -> &'a mut Command {
    cmd.env("XDG_DATA_HOME", sb.join("data"))
        .env("XDG_CONFIG_HOME", sb.join("cfg"))
        .env("XDG_STATE_HOME", sb.join("state"))
        .env("XDG_CACHE_HOME", sb.join("cache"))
}

fn omp_journal(dir: &PathBuf) -> PathBuf {
    let p = dir.join("src.jsonl");
    fs::write(&p, "{\"type\":\"session\",\"version\":3,\"id\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"timestamp\":\"2026-10-01T10:00:00.000Z\",\"cwd\":\"/tmp/proj\"}\n\
{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-10-01T10:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hello opencode mirror\"}}\n").unwrap();
    p
}

#[test]
fn a_mirror_is_imported_into_the_sandbox_store_and_listed() {
    let _exec = exec_guard();
    let Some(cli) = opencode_cli() else { eprintln!("skipped: opencode CLI not installed"); return; };
    let sb = sandbox("import");
    let src = omp_journal(&sb);
    let id = "ses_installtest0000000000000000";
    let sid = install_opencode(&cli, &sesync_conv(), &src, "/tmp/proj", "", id, "Install test", Some(&sb.join("xdg-env")))
        .expect("install must succeed");
    assert_eq!(sid, id);
    let mut list = Command::new(&cli);
    list.args(["session", "list"]);
    isolate(&mut list, &sb.join("xdg-env"));
    let out = String::from_utf8_lossy(&list.output().unwrap().stdout).into_owned();
    assert!(out.contains("Install test"), "the mirror must be listed by opencode: {out}");
}

#[cfg(unix)]
#[test]
fn a_refused_bundle_is_an_error_and_never_deletes_the_existing_session() {
    // On 2026-10-21 a bundle the importer refused ("Missing key at [parentID]") cost a real
    // session all 6926 of its messages: the delete ran first, the import failed half way. The
    // bundle is now proven importable in a throwaway store before anything is deleted.
    let _exec = exec_guard();
    let sb = sandbox("failimport");
    let src = omp_journal(&sb);
    let fake = sb.join("fake-opencode");
    let log = sb.join("oc.log");
    fs::write(&fake, format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = import ]; then echo 'bad bundle' >&2; exit 4; fi\nexit 0\n",
        log.display()
    )).unwrap();
    stage_exec(&fake);
    let r = install_opencode(&fake, &sesync_conv(), &src, "/tmp", "", "ses_failimport00000000000000000", "t", Some(&sb.join("xdg-env")));
    let err = r.expect_err("a refused import must surface as an error");
    assert!(err.contains("left as it is"), "{err}");
    let calls = fs::read_to_string(&log).unwrap_or_default();
    assert!(!calls.contains("session delete"), "the existing session must not be deleted: {calls}");
}

#[cfg(unix)]
#[test]
fn an_import_that_does_not_say_imported_is_an_error() {
    let _exec = exec_guard();
    // The Python daemon requires the 'Imported session' line. Exit 0 with other output
    // is not a successful import.
    let sb = sandbox("noline");
    let src = omp_journal(&sb);
    let fake = sb.join("quiet-opencode");
    fs::write(&fake, "#!/bin/sh\necho 'something else'\nexit 0\n").unwrap();
    stage_exec(&fake);
    let r = install_opencode(&fake, &sesync_conv(), &src, "/tmp", "", "ses_noline0000000000000000000", "t", Some(&sb.join("xdg-env")));
    assert!(r.is_err(), "no 'Imported session' line means the import is not confirmed");
}

#[test]
fn a_reimport_replaces_the_session_content_instead_of_mixing_turns() {
    let _exec = exec_guard();
    let Some(cli) = opencode_cli() else { eprintln!("skipped: opencode CLI not installed"); return; };
    let sb = sandbox("refresh");
    let xdg = sb.join("xdg-env");
    let src = omp_journal(&sb);
    let id = "ses_refreshtest0000000000000000";
    install_opencode(&cli, &sesync_conv(), &src, "/tmp/proj", "", id, "Refresh test", Some(&xdg))
        .expect("first install must succeed");

    // The same mirror id with different content. `opencode import` on its own would keep the
    // old messages (conflict-ignore); the writer must delete first so the session is replaced.
    fs::write(&src, "{\"type\":\"session\",\"version\":3,\"id\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"timestamp\":\"2026-10-01T10:00:00.000Z\",\"cwd\":\"/tmp/proj\"}\n\
{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-10-01T10:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"second version of the conversation\"}}\n").unwrap();
    install_opencode(&cli, &sesync_conv(), &src, "/tmp/proj", "", id, "Refresh test", Some(&xdg))
        .expect("re-import must succeed");

    let db = xdg.join("data").join("opencode").join("opencode.db");
    let c = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let rows: i64 = c.query_row("select count(*) from session where id = ?1", [id], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1, "the refresh keeps exactly one session row");
    let mut texts: Vec<String> = Vec::new();
    let mut stmt = c.prepare("select data from part where session_id = ?1").unwrap();
    for row in stmt.query_map([id], |r| r.get::<_, String>(0)).unwrap() {
        let v: serde_json::Value = serde_json::from_str(&row.unwrap()).unwrap();
        if let Some(t) = v["text"].as_str() {
            texts.push(t.to_string());
        }
    }
    let joined = texts.join("\n");
    assert!(joined.contains("second version"), "the new content is present: {joined}");
    assert!(!joined.contains("hello opencode mirror"), "the old turns are gone: {joined}");
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
