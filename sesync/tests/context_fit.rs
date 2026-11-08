//! Context fit on the mirror path: a long session written into a harness's store by the daemon's
//! own install (`Harness::install`, running the real `sesync-conv`) is fitted to that harness's
//! window, and a session whose summary cannot fit is refused, with nothing written.
//! Temp directories only; the real stores are never touched.

use serde_json::json;
use sesync::harness::{self, window, Location, Tools};
use std::fs;
use std::path::{Path, PathBuf};

const SUMMARY: &str = "SUMMARY OF THE ARCHIVED TURNS";

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-context-fit-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// An omp journal of `turns` turns: a prompt, a tool call, a 4 KB tool result and an answer. With a
/// `summary`, the first five turns are archived under a compaction record and the rest follow it.
fn long_journal(dir: &Path, turns: usize, summary: Option<&str>) -> PathBuf {
    let ts = |i: usize| format!("2026-10-01T10:{:02}:{:02}.000Z", (i / 60) % 60, i % 60);
    let mut lines = vec![json!({"type": "session", "version": 3, "id": "aaaaaaaa-1111-2222-3333-444444444444",
        "timestamp": ts(0), "cwd": "/work/fit"})
    .to_string()];
    let mut parent: Option<String> = None;
    for i in 0..turns {
        if let (Some(text), 5) = (summary, i) {
            lines.push(json!({"type": "compaction", "id": "c1", "parentId": parent, "timestamp": ts(i),
                "summary": text, "firstKeptEntryId": null})
            .to_string());
            parent = Some("c1".into());
        }
        let (u, a, r, t) = (format!("u{i}"), format!("a{i}"), format!("r{i}"), format!("t{i}"));
        let text = format!("output {i} ").repeat(400);
        let records = [
            (u.clone(), parent.clone(), "user", json!([{"type": "text", "text": format!("prompt {i}")}])),
            (a.clone(), Some(u.clone()), "assistant", json!([
                {"type": "toolCall", "id": format!("call-{i}"), "name": "bash", "arguments": {"command": "ls"}}])),
            (r.clone(), Some(a.clone()), "toolResult", json!({"toolCallId": format!("call-{i}"), "toolName": "bash",
                "content": [{"type": "text", "text": text}], "isError": false})),
            (t.clone(), Some(r.clone()), "assistant", json!([{"type": "text", "text": format!("answer {i}")}])),
        ];
        for (id, parent_id, role, body) in records {
            let message = if role == "toolResult" {
                let mut m = body.clone();
                m["role"] = json!("toolResult");
                m
            } else {
                json!({"role": role, "content": body})
            };
            lines.push(json!({"type": "message", "id": id, "parentId": parent_id, "timestamp": ts(i),
                "message": message})
            .to_string());
        }
        parent = Some(t);
    }
    let p = dir.join("source.jsonl");
    fs::write(&p, lines.join("\n") + "\n").unwrap();
    p
}

fn install(id: &str, root: &Path, home: &Path, src: &Path, mirror_id: &str) -> Result<PathBuf, String> {
    let conv = sesync_conv();
    let tools = Tools { home, sesync_conv: &conv };
    let loc = Location { root: root.to_path_buf(), cli: None };
    harness::get(id).expect("a known harness").install(&loc, &tools, src, mirror_id, "Long session").map(PathBuf::from)
}

#[test]
fn a_long_session_is_fitted_into_claudes_window_and_keeps_its_summary() {
    let d = sandbox("claude");
    let src = long_journal(&d, 300, Some(SUMMARY));
    let store = d.join("claude");
    let written = install("claude", &store, &d, &src, "bbbbbbbb-1111-2222-3333-444444444444").unwrap();
    let copy = fs::read_to_string(&written).unwrap();
    let whole = fs::metadata(&src).unwrap().len() as usize;
    eprintln!("claude mirror: {} bytes written from a {whole}-byte journal", copy.len());
    assert!(copy.len() < whole / 2, "the copy is fitted, not the whole journal");
    assert!(copy.contains(SUMMARY), "the compaction summary is kept");
    assert!(copy.contains("left out to fit"), "the cut is said");
    assert!(copy.contains(r#""content":"prompt 299""#), "the newest turn is kept");
    assert!(!copy.contains(r#""content":"prompt 0""#), "the oldest turns are left out");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn a_summary_that_cannot_fit_is_a_mirror_failure_and_nothing_is_written() {
    let d = sandbox("refused");
    let huge = "x".repeat(600_000);
    let src = long_journal(&d, 20, Some(&huge));
    let store = d.join("claude");
    let err = install("claude", &store, &d, &src, "cccccccc-1111-2222-3333-444444444444").unwrap_err();
    assert!(err.contains("sesync-conv to-claude failed") && err.contains("context fit for claude"), "{err}");
    assert!(err.contains("nothing was written"), "{err}");
    let written: Vec<_> = fs::read_dir(store.join("projects")).map(|r| r.flatten().collect()).unwrap_or_default();
    assert!(written.is_empty(), "a refused copy leaves no file behind");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn a_long_session_is_fitted_into_antigravity() {
    let d = sandbox("antigravity");
    let src = long_journal(&d, 300, Some(SUMMARY));
    let store = d.join("antigravity");
    let db = install("antigravity", &store, &d, &src, "dddddddd-1111-2222-3333-444444444444").unwrap();
    assert!(db.is_file(), "{}", db.display());
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn the_fit_flags_follow_the_targets_window() {
    let d = sandbox("flags");
    let src = long_journal(&d, 3, None);
    let no_env = |_: &str| -> Option<String> { None };
    let claude = window::flags_with("claude", &src, &no_env, Some(&d));
    assert_eq!(claude[1], "200000", "Claude's table window");
    let ag = window::flags_with("antigravity", &src, &no_env, Some(&d));
    assert_eq!(ag[1], "128000", "no table entry: the conservative window");
    let _ = fs::remove_dir_all(&d);
}
