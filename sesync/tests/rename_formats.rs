//! Rename per format, ported from daemon/hs_manage.py. A rename must change the name
//! the harness shows. Claude and Pi are changed by appending a title record and leaving
//! the transcript as is. omp is changed in its header. A rename that changes nothing
//! must fail, not report success.

use sesync::manage::{rename_group, Group, Roots};
use std::fs;
use std::path::PathBuf;

const CID: &str = "bbbbbbbb-1111-2222-3333-444444444444";

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-renfmt-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn roots(sb: &PathBuf) -> Roots {
    Roots::under(&sb, sb.join("no-sesync-conv"))
}

fn do_rename(sb: &PathBuf, r: &Roots, g: &Group, title: &str) -> Result<String, String> {
    rename_group(r, g, title, &sb.join("locks"), &sb.join("no-sesync_lock"))
}

#[test]
fn claude_rename_appends_a_custom_title_record_and_keeps_the_transcript() {
    let sb = sandbox("claude");
    let r = roots(&sb);
    let dir = r.locations.sessions_dir("claude").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("{CID}.jsonl"));
    let original = format!("{}\n{}\n",
        format!(r#"{{"type":"user","uuid":"u1","sessionId":"{CID}","message":{{"role":"user","content":"hi"}}}}"#),
        format!(r#"{{"type":"ai-title","aiTitle":"Generated","sessionId":"{CID}"}}"#));
    fs::write(&p, &original).unwrap();

    let g = Group::file("claude", p.display().to_string(), "Generated");
    do_rename(&sb, &r, &g, "Renamed claude").expect("rename");

    let after = fs::read_to_string(&p).unwrap();
    assert!(after.starts_with(&original), "the transcript must not be rewritten, only appended to");
    let last: serde_json::Value = serde_json::from_str(after.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "custom-title", "Claude reads custom-title first");
    assert_eq!(last["customTitle"], "Renamed claude");
}

#[test]
fn pi_rename_appends_a_session_info_name_and_keeps_history() {
    let sb = sandbox("pi");
    let r = roots(&sb);
    let dir = r.locations.sessions_dir("pi").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("2026-10-01T10-00-00-000Z_{CID}.jsonl"));
    let original = "{\"type\":\"session\",\"id\":\"x\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"keep\"}}\n";
    fs::write(&p, original).unwrap();

    let g = Group::file("pi", p.display().to_string(), "x");
    do_rename(&sb, &r, &g, "Renamed pi").expect("rename");

    let after = fs::read_to_string(&p).unwrap();
    assert!(after.starts_with(original), "history must be kept");
    let last: serde_json::Value = serde_json::from_str(after.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "session_info");
    assert_eq!(last["name"], "Renamed pi");
}

#[test]
fn a_rename_that_changes_nothing_is_an_error() {
    // Regression: the rename used to rewrite Claude and Pi files unchanged and report
    // success. The new title must be visible in the file, or the rename fails.
    let sb = sandbox("noop");
    let r = roots(&sb);
    let dir = r.locations.sessions_dir("claude").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("{CID}.jsonl"));
    fs::write(&p, format!("{{\"type\":\"user\",\"uuid\":\"u1\",\"sessionId\":\"{CID}\",\"message\":{{\"role\":\"user\",\"content\":\"x\"}}}}\n")).unwrap();
    let g = Group::file("claude", p.display().to_string(), "x");
    let res = do_rename(&sb, &r, &g, "Visible name");
    assert!(res.is_ok());
    assert!(fs::read_to_string(&p).unwrap().contains("Visible name"),
        "the new title must be written into the file");
}
