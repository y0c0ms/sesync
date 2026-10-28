// The fake CLIs here are `#!/bin/sh` scripts staged by `fake_cli::stage`: they cannot run on
// Windows, where the file never compiled. The real-CLI path is covered on Unix CI.
#![cfg(unix)]

//! O-penCode export and convert, ported from daemon/sesync.py
//! prepare_opencode_source. The export is saved as-is and sesync_conv from-opencode reads
//! it, skipping any banner before the first '{'. Uses the real opencode CLI and the
//! real sesync_conv in a sandbox. Skipped when the CLI is not installed.

use sesync::harness::opencode::store::export_and_convert;
use std::fs;
use std::path::PathBuf;

#[path = "support/fake_cli.rs"]
mod fake_cli;

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-ocx-{}-{name}", std::process::id()));
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
    which("opencode")
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p).map(|d| d.join(name)).find(|c| c.is_file())
    })
}

#[test]
fn a_session_exports_and_converts_to_a_journal() {
    let Some(cli) = opencode_cli() else {
        eprintln!("skipped: opencode CLI not installed");
        return;
    };
    let sb = sandbox("convert");
    // Use a session that exists in the live store; it is only read, never changed.
    let sid = std::env::var("HS_TEST_OPENCODE_SESSION").unwrap_or_default();
    if sid.is_empty() {
        eprintln!("skipped: set HS_TEST_OPENCODE_SESSION to a live session id");
        return;
    }
    let journal = export_and_convert(&cli, &sesync_conv(), &sid, None, &sb).expect("export and convert");
    let text = fs::read_to_string(&journal).unwrap();
    assert!(text.lines().count() > 0, "the journal must have records");
    assert!(text.contains("\"type\":\"session\"") || text.contains("\"type\": \"session\""),
            "a journal starts with a session header");
}

#[test]
fn a_banner_before_the_json_is_skipped() {
    // The export can print a line before the JSON. The converter must still read it.
    let sb = sandbox("banner");
    let fake_cli = sb.join("fake-opencode");
    // One real user message, so the converter has something to convert after the banner.
    let json = r#"{"info":{"id":"ses_x","directory":"/tmp","title":"t","time":{"created":1,"updated":1}},"messages":[{"info":{"id":"m1","role":"user","sessionID":"ses_x","time":{"created":1}},"parts":[{"id":"p1","type":"text","text":"hello banner","messageID":"m1","sessionID":"ses_x"}]}]}"#;
    fake_cli::stage(&fake_cli, &format!("#!/bin/sh\necho 'Exporting session: ses_x'\ncat <<'EOF'\n{json}\nEOF\n"));
    let journal = export_and_convert(&fake_cli, &sesync_conv(), "ses_x", None, &sb).expect("banner must be skipped");
    assert!(journal.exists());
}

#[test]
fn a_failing_export_is_an_error_and_writes_no_journal() {
    let sb = sandbox("fail");
    let fake_cli = sb.join("fail-opencode");
    fake_cli::stage(&fake_cli, "#!/bin/sh\necho boom >&2\nexit 3\n");
    assert!(export_and_convert(&fake_cli, &sesync_conv(), "ses_x", None, &sb).is_err());
}

#[test]
fn stdout_to_a_file_keeps_a_large_export_whole() {
    // The truncation happens in the CLI when its stdout is a pipe: it writes the first
    // 196608 bytes and exits 0. The export is written to a file, not a pipe, so the
    // whole payload must survive. The fake CLI writes the full payload to stdout;
    // stdout is a file here, so nothing is cut.
    let sb = sandbox("large");
    let fake_cli = sb.join("big-opencode");
    let filler = "x".repeat(300_000);
    let json = format!(r#"{{"info":{{"id":"ses_big","directory":"/tmp","title":"big","time":{{"created":1,"updated":1}}}},"messages":[{{"info":{{"id":"m1","role":"user","sessionID":"ses_big","time":{{"created":1}}}},"parts":[{{"id":"p1","type":"text","text":"start {filler} END_MARKER","messageID":"m1","sessionID":"ses_big"}}]}}]}}"#);
    let payload = sb.join("payload.json");
    fs::write(&payload, &json).unwrap();
    fake_cli::stage(&fake_cli, &format!("#!/bin/sh\ncat '{}'\n", payload.display()));
    let journal = export_and_convert(&fake_cli, &sesync_conv(), "ses_big", None, &sb).expect("large export must convert");
    let text = fs::read_to_string(&journal).unwrap();
    assert!(text.contains("END_MARKER"), "the end of a 300 KB export must survive");
}

#[test]
fn a_pipe_truncation_is_detected_as_a_failed_export_not_a_short_journal() {
    // If the export were truncated (as the CLI does through a pipe), sesync_conv must reject
    // the invalid JSON. A short journal must never be produced silently.
    let sb = sandbox("trunc");
    let fake_cli = sb.join("trunc-opencode");
    let filler = "y".repeat(300_000);
    let json = format!(r#"{{"info":{{"id":"ses_t","directory":"/tmp","title":"t","time":{{"created":1,"updated":1}}}},"messages":[{{"info":{{"id":"m1","role":"user","sessionID":"ses_t","time":{{"created":1}}}},"parts":[{{"id":"p1","type":"text","text":"{filler}","messageID":"m1","sessionID":"ses_t"}}]}}]}}"#);
    let payload = sb.join("payload.json");
    fs::write(&payload, &json).unwrap();
    fake_cli::stage(&fake_cli, &format!("#!/bin/sh\nhead -c 196608 '{}'\n", payload.display()));
    assert!(export_and_convert(&fake_cli, &sesync_conv(), "ses_t", None, &sb).is_err(),
            "a truncated export must be an error, never a short journal");
    assert!(!sb.join("ses_t.journal.jsonl").exists(), "no journal may be left from a truncated export");
}

/// `opencode export` finds a session only from its own project, so the conversion runs there. A fake CLI
/// answers only from that directory, as the real one does, and the conversion must still succeed.
#[test]
fn a_session_is_exported_from_its_own_project_directory() {
    use sesync::harness::{Harness, Location, Tools};
    use sesync::harness::opencode::OpenCode;
    let sb = sandbox("export-dir");
    let project = sb.join("project");
    fs::create_dir_all(&project).unwrap();
    let db = sb.join("opencode.db");
    let con = rusqlite::Connection::open(&db).unwrap();
    con.execute_batch(
        "create table session (id text primary key, parent_id text, directory text not null, title text not null, time_updated integer not null, time_archived integer);",
    )
    .unwrap();
    con.execute(
        "insert into session (id, directory, title, time_updated) values ('ses_d', ?1, 'd', 1000)",
        [project.display().to_string()],
    )
    .unwrap();
    drop(con);
    let export = sb.join("export.json");
    fs::write(&export, r#"{"info":{"id":"ses_d","directory":"/x","title":"d","time":{"created":1}},"messages":[{"info":{"id":"m1","role":"user","sessionID":"ses_d","time":{"created":1}},"parts":[{"id":"p1","type":"text","text":"from its project","messageID":"m1","sessionID":"ses_d"}]}]}"#).unwrap();
    let cli = sb.join("opencode-cli");
    let here = fs::canonicalize(&project).unwrap();
    fake_cli::stage(
        &cli,
        &format!(
            "#!/bin/sh\n[ \"$(pwd -P)\" = '{}' ] || {{ echo 'Session not found' >&2; exit 1; }}\ncat '{}'\n",
            here.display(),
            export.display()
        ),
    );
    let loc = Location { root: db, cli: Some(cli) };
    let tools = Tools { home: &sb, sesync_conv: &sesync_conv() };
    let scratch = sb.join("scratch");
    let journal = OpenCode.to_journal(&loc, &tools, "ses_d", &scratch).expect("exported from the project").expect("converted");
    assert!(fs::read_to_string(&journal).unwrap().contains("from its project"));
}
