// The fake CLIs here are `#!/bin/sh` scripts made executable with Unix permissions: they cannot
// run on Windows, where the file never compiled. The real-CLI path is covered on Unix CI.
#![cfg(unix)]

//! O-penCode export and convert, ported from daemon/sesync.py
//! prepare_opencode_source. The export is saved as-is and sesync_conv from-opencode reads
//! it, skipping any banner before the first '{'. Uses the real opencode CLI and the
//! real sesync_conv in a sandbox. Skipped when the CLI is not installed.

use sesync::harness::opencode::store::export_and_convert;
use std::fs;
use std::path::PathBuf;

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
    let _exec = exec_guard();
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
    let journal = export_and_convert(&cli, &sesync_conv(), &sid, &sb).expect("export and convert");
    let text = fs::read_to_string(&journal).unwrap();
    assert!(text.lines().count() > 0, "the journal must have records");
    assert!(text.contains("\"type\":\"session\"") || text.contains("\"type\": \"session\""),
            "a journal starts with a session header");
}

#[test]
fn a_banner_before_the_json_is_skipped() {
    let _exec = exec_guard();
    // The export can print a line before the JSON. The converter must still read it.
    let sb = sandbox("banner");
    let fake_cli = sb.join("fake-opencode");
    // One real user message, so the converter has something to convert after the banner.
    let json = r#"{"info":{"id":"ses_x","directory":"/tmp","title":"t","time":{"created":1,"updated":1}},"messages":[{"info":{"id":"m1","role":"user","sessionID":"ses_x","time":{"created":1}},"parts":[{"id":"p1","type":"text","text":"hello banner","messageID":"m1","sessionID":"ses_x"}]}]}"#;
    fs::write(&fake_cli, format!("#!/bin/sh\necho 'Exporting session: ses_x'\ncat <<'EOF'\n{json}\nEOF\n")).unwrap();
    make_exec(&fake_cli);
    let journal = export_and_convert(&fake_cli, &sesync_conv(), "ses_x", &sb).expect("banner must be skipped");
    assert!(journal.exists());
}

#[test]
fn a_failing_export_is_an_error_and_writes_no_journal() {
    let _exec = exec_guard();
    let sb = sandbox("fail");
    let fake_cli = sb.join("fail-opencode");
    fs::write(&fake_cli, "#!/bin/sh\necho boom >&2\nexit 3\n").unwrap();
    make_exec(&fake_cli);
    assert!(export_and_convert(&fake_cli, &sesync_conv(), "ses_x", &sb).is_err());
}

fn make_exec(p: &PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn stdout_to_a_file_keeps_a_large_export_whole() {
    let _exec = exec_guard();
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
    fs::write(&fake_cli, format!("#!/bin/sh\ncat '{}'\n", payload.display())).unwrap();
    make_exec(&fake_cli);
    let journal = export_and_convert(&fake_cli, &sesync_conv(), "ses_big", &sb).expect("large export must convert");
    let text = fs::read_to_string(&journal).unwrap();
    assert!(text.contains("END_MARKER"), "the end of a 300 KB export must survive");
}

#[test]
fn a_pipe_truncation_is_detected_as_a_failed_export_not_a_short_journal() {
    let _exec = exec_guard();
    // If the export were truncated (as the CLI does through a pipe), sesync_conv must reject
    // the invalid JSON. A short journal must never be produced silently.
    let sb = sandbox("trunc");
    let fake_cli = sb.join("trunc-opencode");
    let filler = "y".repeat(300_000);
    let json = format!(r#"{{"info":{{"id":"ses_t","directory":"/tmp","title":"t","time":{{"created":1,"updated":1}}}},"messages":[{{"info":{{"id":"m1","role":"user","sessionID":"ses_t","time":{{"created":1}}}},"parts":[{{"id":"p1","type":"text","text":"{filler}","messageID":"m1","sessionID":"ses_t"}}]}}]}}"#);
    let payload = sb.join("payload.json");
    fs::write(&payload, &json).unwrap();
    fs::write(&fake_cli, format!("#!/bin/sh\nhead -c 196608 '{}'\n", payload.display())).unwrap();
    make_exec(&fake_cli);
    assert!(export_and_convert(&fake_cli, &sesync_conv(), "ses_t", &sb).is_err(),
            "a truncated export must be an error, never a short journal");
    assert!(!sb.join("ses_t.journal.jsonl").exists(), "no journal may be left from a truncated export");
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
