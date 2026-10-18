//! The sesync binary syncing an O-penCode session, end to end. A sandbox
//! O-penCode database holds one live session; with the user's choice pre-seeded, one --once
//! pass must mirror it into the omp store, and a second pass must do nothing. Uses the real
//! sesync_conv and the real opencode CLI, with XDG_* pointed at the sandbox. Skipped without
//! the CLI.

use sesync::state::{Source, State};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-cliocs-{}-{name}", std::process::id()));
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

fn run(sb: &Path, extra: &[&str]) -> Output {
    Command::new(BIN)
        .args(extra)
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

#[test]
fn an_opencode_session_is_mirrored_into_omp_and_not_resynced() {
    if !have_cli() { eprintln!("skipped: opencode CLI not installed"); return; }
    let sb = sandbox("sync");
    let db_dir = sb.join("xdg").join("data").join("opencode");
    fs::create_dir_all(&db_dir).unwrap();
    fs::create_dir_all(sb.join("omp").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    // A store with one live session with a real user prompt; time_updated is old enough.
    let db = db_dir.join("opencode.db");
    // time_updated in milliseconds, set to 10 minutes ago so it is inside the 14-day
    // window and past the quiet period.
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let updated = now_ms - 600_000;
    // Let the real opencode CLI create the store and its schema, then add the rows.
    // Building the tables by hand clashes with the CLI's own migrations.
    let mut init = std::process::Command::new("opencode");
    init.args(["session", "list"])
        .env("XDG_DATA_HOME", sb.join("xdg").join("data"))
        .env("XDG_CONFIG_HOME", sb.join("xdg").join("cfg"))
        .env("XDG_STATE_HOME", sb.join("xdg").join("state"))
        .env("XDG_CACHE_HOME", sb.join("xdg").join("cache"));
    assert!(init.output().unwrap().status.success(), "the CLI must create its store");
    assert!(db.exists(), "the store must exist at {}", db.display());
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute(
        "insert into project (id, worktree, sandboxes, time_created, time_updated) values ('p1', '/tmp/proj', '[]', 1, 1)",
        [],
    ).unwrap();
    c.execute(
        "insert into session (id, project_id, slug, directory, title, version, time_created, time_updated) \
         values ('ses_cliocs0000000000000000', 'p1', 'cli', '/tmp/proj', 'OC CLI title', '1', ?1, ?1)",
        [updated],
    ).unwrap();
    c.execute(
        "insert into message (id, session_id, time_created, time_updated, data) values ('m1', 'ses_cliocs0000000000000000', 1, 1, ?1)",
        ["{\"role\":\"user\"}"],
    ).unwrap();
    c.execute(
        "insert into part (id, message_id, session_id, time_created, time_updated, data) values ('p1', 'm1', 'ses_cliocs0000000000000000', 1, 1, ?1)",
        ["{\"type\":\"text\",\"text\":\"hello from opencode\"}"],
    ).unwrap();
    drop(c);

    // The user's choice, exactly as the TUI would save it: this session is mirrored into omp.
    fs::create_dir_all(sb.join("state")).unwrap();
    let mut state = State::default();
    let mut src = Source::new("opencode", "ses_cliocs0000000000000000");
    src.chosen_targets.insert("omp".to_string());
    state.sources.insert("ses_cliocs0000000000000000".to_string(), src);
    state.save(&sb.join("state").join("state.json")).unwrap();

    let o = run(&sb, &["--once"]);
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("one pass done, 1 source(s) changed"), "{out}");
    let mirrors = walk(&sb.join("omp").join("sessions"));
    assert_eq!(mirrors.len(), 1, "an opencode session must produce one omp mirror");

    let again = run(&sb, &["--once"]);
    assert!(String::from_utf8_lossy(&again.stdout).contains("0 source(s) changed"),
            "an unchanged opencode session must not be re-synced");
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
