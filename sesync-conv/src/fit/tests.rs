//! Context-fit tests: long synthetic sessions from each source (omp, Claude, OpenCode), fitted for
//! each message-based target (Claude, OpenCode, Antigravity). Every case checks the same things:
//! the copy fits its budget, the compaction summary survives, the cut falls on a user turn, no tool
//! call is split from its result, and an impossible fit is refused.

use super::*;
use crate::{claude, from_claude, from_opencode, opencode, to_antigravity};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Bytes of each tool result: a long session overflows a 200k window many times over.
const RESULT_BYTES: usize = 4_000;
const SUMMARY: &str = "SUMMARY OF THE ARCHIVED TURNS";
/// 2026-01-01T00:00:00Z, the base of every synthetic timestamp.
const BASE_MS: i64 = 1_767_225_600_000;
const MODEL: &str = "claude-sonnet-4-5";

fn sandbox(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("sesync-conv-fit-{}-{name}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Millisecond time of step `i` (four steps per turn, one second apart).
fn ms(i: usize) -> i64 {
    BASE_MS + i as i64 * 1_000
}

fn iso(i: usize) -> String {
    iso_from_ms(ms(i))
}

fn result_text(i: usize) -> String {
    format!("output line {i}: ").repeat(RESULT_BYTES / 16)
}

fn journal_from_lines(dir: &Path, name: &str, lines: &[String]) -> Journal {
    let p = dir.join(name);
    fs::write(&p, lines.join("\n") + "\n").unwrap();
    Journal::read(&p).unwrap()
}

/// omp's journal shape: a session header, then turns. Each turn is a user prompt, an assistant
/// call, its tool result and an answer, linked by `parentId`.
///
/// With `compact = (archived, kept)`, turns `0..archived` are archived, turns `archived..archived +
/// kept` are kept, and a compaction record follows them with `firstKeptEntryId` at the first kept
/// prompt. The remaining turns come after it, as omp writes a compacted session.
fn omp_lines(turns: usize, compact: Option<(usize, usize)>) -> Vec<String> {
    let mut lines = vec![json!({"type": "session", "id": "omp-s", "timestamp": iso(0), "cwd": "/work/proj"}).to_string()];
    let mut parent: Option<String> = None;
    let mut first_kept: Option<String> = None;
    for i in 0..turns {
        if let Some((archived, kept)) = compact {
            if i == archived {
                first_kept = Some(format!("u{i}"));
            }
            if i == archived + kept {
                lines.push(json!({
                    "type": "compaction", "id": "c1", "parentId": parent, "timestamp": iso(i * 4),
                    "summary": SUMMARY, "firstKeptEntryId": first_kept,
                }).to_string());
                parent = Some("c1".into());
            }
        }
        // Each record links to the one before it, through `parent`.
        let u = format!("u{i}");
        let a = format!("a{i}");
        let r = format!("r{i}");
        let t = format!("t{i}");
        let mut recs = Vec::new();
        recs.push(record(&u, &parent, i * 4, "user", json!([{"type": "text", "text": format!("prompt {i}")}])));
        parent = Some(u.clone());
        recs.push(record(&a, &parent, i * 4 + 1, "assistant", json!([
            {"type": "text", "text": "checking"},
            {"type": "toolCall", "id": format!("call-{i}"), "name": "bash", "arguments": {"command": "ls"}},
        ])));
        recs.push(json!({
            "type": "message", "id": r, "parentId": a, "timestamp": iso(i * 4 + 2),
            "message": {"role": "toolResult", "toolCallId": format!("call-{i}"), "toolName": "bash",
                "content": [{"type": "text", "text": result_text(i)}], "isError": false},
        }));
        parent = Some(r.clone());
        recs.push(record(&t, &parent, i * 4 + 3, "assistant", json!([{"type": "text", "text": format!("answer {i}")}])));
        parent = Some(t);
        lines.extend(recs.iter().map(Value::to_string));
    }
    lines
}

fn record(id: &str, parent: &Option<String>, t: usize, role: &str, content: Value) -> Value {
    json!({
        "type": "message", "id": id, "parentId": parent, "timestamp": iso(t),
        "message": {"role": role, "provider": "anthropic", "model": MODEL, "content": content},
    })
}

/// A Claude Code transcript (`from-claude` input): the same turns as uuid / parentUuid records,
/// with tool_use and tool_result blocks. With `compact_after = Some(k)`, a `compact_boundary`
/// system record and an `isCompactSummary` user record follow turn `k`, and the turns after it
/// hang from the summary, as Claude Code writes them.
fn claude_raw(turns: usize, compact_after: Option<usize>) -> String {
    let rec = |v: Value| v.to_string();
    let mut out = Vec::new();
    let mut parent: Option<String> = None;
    let mut last_uuid: Option<String> = None;
    for i in 0..turns {
        if compact_after == Some(i) {
            let boundary = format!("b{i}");
            out.push(rec(json!({
                "type": "system", "subtype": "compact_boundary", "uuid": boundary, "parentUuid": null,
                "logicalParentUuid": last_uuid, "sessionId": "claude-s", "cwd": "/work/proj",
                "timestamp": iso(i * 4), "compactMetadata": {"trigger": "auto", "preTokens": 180000},
            })));
            let summary = format!("s{i}");
            out.push(rec(json!({
                "type": "user", "uuid": summary, "parentUuid": boundary, "isCompactSummary": true,
                "sessionId": "claude-s", "cwd": "/work/proj", "timestamp": iso(i * 4),
                "message": {"role": "user", "content": SUMMARY},
            })));
            parent = Some(summary.clone());
        }
        let (u, a, r, t) = (format!("u{i}"), format!("a{i}"), format!("r{i}"), format!("t{i}"));
        out.push(rec(json!({
            "type": "user", "uuid": u, "parentUuid": parent, "sessionId": "claude-s", "cwd": "/work/proj",
            "timestamp": iso(i * 4), "message": {"role": "user", "content": format!("prompt {i}")},
        })));
        out.push(rec(json!({
            "type": "assistant", "uuid": a, "parentUuid": u, "sessionId": "claude-s", "timestamp": iso(i * 4 + 1),
            "message": {"role": "assistant", "model": MODEL, "content": [
                {"type": "text", "text": "checking"},
                {"type": "tool_use", "id": format!("toolu_{i}"), "name": "Bash", "input": {"command": "ls"}},
            ]},
        })));
        out.push(rec(json!({
            "type": "user", "uuid": r, "parentUuid": a, "sessionId": "claude-s", "timestamp": iso(i * 4 + 2),
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("toolu_{i}"), "content": result_text(i), "is_error": false},
            ]},
        })));
        out.push(rec(json!({
            "type": "assistant", "uuid": t, "parentUuid": r, "sessionId": "claude-s", "timestamp": iso(i * 4 + 3),
            "message": {"role": "assistant", "model": MODEL, "content": [{"type": "text", "text": format!("answer {i}")}]},
        })));
        parent = Some(t.clone());
        last_uuid = Some(t);
    }
    out.join("\n") + "\n"
}

/// An OpenCode export (`from-opencode` input). With `compact_after = Some(k)`, a user message
/// carrying a compaction part and a finished assistant summary follow turn `k`, as OpenCode
/// writes them; the turns after it are the live window.
fn opencode_export(turns: usize, compact_after: Option<usize>) -> Value {
    let mut messages = Vec::new();
    for i in 0..turns {
        if compact_after == Some(i) {
            messages.push(json!({
                "info": {"id": "msg_c", "sessionID": "ses_long", "role": "user", "time": {"created": ms(i * 4)}},
                "parts": [{"id": "prt_c", "type": "compaction", "auto": true}],
            }));
            messages.push(json!({
                "info": {"id": "msg_cs", "sessionID": "ses_long", "role": "assistant", "summary": true,
                    "finish": "stop", "parentID": "msg_c", "modelID": MODEL, "providerID": "anthropic",
                    "time": {"created": ms(i * 4), "completed": ms(i * 4)}},
                "parts": [{"id": "prt_cs", "type": "text", "text": SUMMARY}],
            }));
        }
        messages.push(json!({
            "info": {"id": format!("msg_u{i}"), "sessionID": "ses_long", "role": "user", "time": {"created": ms(i * 4)}},
            "parts": [{"id": format!("prt_u{i}"), "type": "text", "text": format!("prompt {i}")}],
        }));
        messages.push(json!({
            "info": {"id": format!("msg_a{i}"), "sessionID": "ses_long", "role": "assistant", "parentID": format!("msg_u{i}"),
                "finish": "tool-calls", "modelID": MODEL, "providerID": "anthropic",
                "time": {"created": ms(i * 4 + 1), "completed": ms(i * 4 + 2)}},
            "parts": [
                {"id": format!("prt_a{i}"), "type": "text", "text": "checking"},
                {"id": format!("prt_t{i}"), "type": "tool", "callID": format!("call-{i}"), "tool": "bash",
                    "state": {"status": "completed", "input": {"command": "ls"}, "output": result_text(i),
                        "title": "bash", "metadata": {}, "time": {"start": ms(i * 4 + 1), "end": ms(i * 4 + 2)}}},
            ],
        }));
        messages.push(json!({
            "info": {"id": format!("msg_t{i}"), "sessionID": "ses_long", "role": "assistant", "parentID": format!("msg_a{i}"),
                "finish": "stop", "modelID": MODEL, "providerID": "anthropic",
                "time": {"created": ms(i * 4 + 3), "completed": ms(i * 4 + 3)}},
            "parts": [{"id": format!("prt_r{i}"), "type": "text", "text": format!("answer {i}")}],
        }));
    }
    json!({
        "info": {"id": "ses_long", "directory": "/work/proj", "title": "Long session", "time": {"created": BASE_MS}},
        "messages": messages,
    })
}

/// The four sources as canonical journals: omp directly; Claude and OpenCode through their
/// converters, exactly as the daemon runs them. Each has the same long shape and a compaction.
fn sources(dir: &Path) -> Vec<(&'static str, Journal)> {
    let omp = journal_from_lines(dir, "omp.jsonl", &omp_lines(300, Some((60, 20))));
    let (claude_lines, _) = from_claude::to_journal_lines(&claude_raw(300, Some(60))).unwrap();
    let claude = journal_from_lines(dir, "claude.jsonl", &claude_lines);
    let (oc_lines, _) = from_opencode::to_journal_lines(&opencode_export(300, Some(60))).unwrap();
    let opencode = journal_from_lines(dir, "opencode.jsonl", &oc_lines);
    vec![("omp", omp), ("claude", claude), ("opencode", opencode)]
}

fn budget(window: usize) -> Budget {
    Budget { window, share: DEFAULT_SHARE, overhead: DEFAULT_OVERHEAD }
}

/// True when every tool result in `msgs` answers a call earlier in `msgs`.
fn pairs_whole(msgs: &[Msg]) -> bool {
    let mut calls = std::collections::HashSet::new();
    for m in msgs {
        for b in &m.content {
            if b.get("type").and_then(Value::as_str) == Some("toolCall") {
                if let Some(id) = b.get("id").and_then(Value::as_str) {
                    calls.insert(id.to_string());
                }
            }
        }
        if m.role == "toolResult" {
            let answered = m.message.get("toolCallId").and_then(Value::as_str).is_some_and(|id| calls.contains(id));
            if !answered {
                return false;
            }
        }
    }
    true
}

/// Every string in a JSON value, as the target reads it: the message or part content, not the
/// record envelope (ids, timestamps) that the target never shows the model.
fn strings_tokens(v: &Value) -> usize {
    match v {
        Value::String(s) => tokens_for_bytes(s.len()),
        Value::Array(a) => a.iter().map(strings_tokens).sum(),
        Value::Object(o) => o.values().map(strings_tokens).sum(),
        _ => 0,
    }
}

/// The content tokens a Claude transcript (one JSON record per line) carries.
fn claude_content_tokens(lines: &[String]) -> usize {
    lines.iter().map(|l| serde_json::from_str::<Value>(l).unwrap()).map(|r| strings_tokens(&r["message"])).sum()
}

/// The content tokens an OpenCode bundle carries: the parts of every message.
fn opencode_content_tokens(bundle: &Value) -> usize {
    bundle["messages"].as_array().unwrap().iter().map(|m| strings_tokens(&m["parts"])).sum()
}

#[test]
fn budget_defaults_and_flags_give_the_documented_limit() {
    let none = Budget::from_args(&[]).unwrap();
    assert_eq!(none, Budget { window: 128_000, share: 0.75, overhead: 16_384 });
    assert_eq!(none.limit(), 79_616, "128000 x 0.75 - 16384");
    let claude_window = Budget::from_args(&["--context-window".into(), "200000".into()]).unwrap();
    assert_eq!(claude_window.limit(), 133_616, "200000 x 0.75 - 16384");
    let custom = Budget::from_args(&[
        "--context-window".into(), "100000".into(), "--context-share".into(), "0.5".into(), "--context-overhead".into(), "1000".into(),
    ])
    .unwrap();
    assert_eq!(custom.limit(), 49_000);
}

#[test]
fn bad_budget_values_are_refused_not_defaulted() {
    let flag = |k: &str, v: &str| vec![k.to_string(), v.to_string()];
    assert!(Budget::from_args(&flag("--context-window", "0")).is_err());
    assert!(Budget::from_args(&flag("--context-window", "lots")).is_err());
    assert!(Budget::from_args(&flag("--context-share", "1.5")).is_err());
    assert!(Budget::from_args(&flag("--context-share", "0")).is_err());
    assert!(Budget::from_args(&flag("--context-share", "NaN")).is_err());
    assert!(Budget::from_args(&flag("--context-overhead", "-1")).is_err());
    let e = Budget::from_args(&flag("--context-overhead", "999999")).unwrap_err();
    assert!(e.contains("leaves no room"), "{e}");
}

#[test]
fn a_session_that_fits_is_exported_whole() {
    let dir = sandbox("fits");
    let j = journal_from_lines(&dir, "small.jsonl", &omp_lines(5, None));
    let whole = j.window();
    let fitted = j.fit("claude", &budget(200_000)).unwrap();
    assert!(fitted.note().is_none(), "nothing left out, so no note");
    assert_eq!(fitted.msgs.len(), whole.msgs.len());
    assert_eq!(fitted.report.tokens_after, fitted.report.tokens_before);
    assert_eq!(fitted.report.turns_kept, fitted.report.turns);
    let _ = fs::remove_dir_all(&dir);
}

/// The acceptance case: long sessions from every source, through every message-based target.
#[test]
fn long_sessions_fit_every_message_target_with_the_summary_and_a_clean_cut() {
    let dir = sandbox("matrix");
    let targets: [(&str, usize); 3] = [("claude", 200_000), ("opencode", 128_000), ("antigravity", 128_000)];
    for (source, j) in sources(&dir) {
        for (target, window) in targets {
            let b = budget(window);
            let limit = b.limit();
            let win = j.fit(target, &b).unwrap_or_else(|e| panic!("{source} -> {target}: {e}"));
            let r = &win.report;
            eprintln!("{source} -> {target}: before ~{} after ~{} tokens (limit {limit}); kept {} of {} turns",
                r.tokens_before, r.tokens_after, r.turns_kept, r.turns);
            assert!(r.tokens_before > limit, "{source} -> {target}: the case must overflow the budget");
            assert!(r.tokens_after <= limit, "{source} -> {target}: {} tokens over the {limit} limit", r.tokens_after);
            assert!(r.turns_kept < r.turns, "{source} -> {target}: some turns are left out");
            assert_eq!(win.msgs[0].role, "user", "{source} -> {target}: the cut falls on a user turn");
            assert!(pairs_whole(&win.msgs), "{source} -> {target}: a tool call was split from its result");
            let (note, _) = win.note().expect("a note for what was left out");
            assert!(note.contains(SUMMARY), "{source} -> {target}: the compaction summary survives");
            assert!(note.contains("left out to fit"), "{source} -> {target}: the cut is said, not silent");
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_written_claude_and_opencode_copies_stay_within_the_window_share() {
    let dir = sandbox("written");
    for (source, j) in sources(&dir) {
        let win = j.fit("claude", &budget(200_000)).unwrap();
        let lines = claude::to_lines(&win, &claude::Options { session_id: "sid", cwd: "/work/proj", cli_version: "2.1.245", title: "" });
        let content = claude_content_tokens(&lines);
        eprintln!("{source} -> claude: written content ~{content} tokens (window share {})", 200_000.0 * DEFAULT_SHARE);
        assert!(content <= (200_000.0 * DEFAULT_SHARE) as usize, "{source} -> claude: {content} content tokens");
        let first: Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(first["message"]["content"][0]["text"].as_str().unwrap().contains(SUMMARY), "{source}: the note leads the copy");

        let win = j.fit("opencode", &budget(128_000)).unwrap();
        let bundle = opencode::to_bundle(&win, &opencode::Options {
            session_id: "ses_fit", title: "t", directory: "/work/proj", cli_version: "1.18.16", include_thinking: true,
        });
        let content = opencode_content_tokens(&bundle);
        eprintln!("{source} -> opencode: written content ~{content} tokens (window share {})", 128_000.0 * DEFAULT_SHARE);
        assert!(content <= (128_000.0 * DEFAULT_SHARE) as usize, "{source} -> opencode: {content} content tokens");
        let msgs = bundle["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["info"]["role"], "user", "{source}: the copy opens on the note, a user message");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_omp_summary_survives_into_an_antigravity_copy() {
    let dir = sandbox("antigravity");
    let store = dir.join("store");
    let j = journal_from_lines(&dir, "omp.jsonl", &omp_lines(300, Some((60, 20))));
    let win = j.fit("antigravity", &budget(128_000)).unwrap();
    let (db, stats, _) = to_antigravity::install(&j, &win, &store, "fit-conv", "").unwrap();
    assert!(db.is_file());
    assert!(stats.user_steps > 0 && stats.tool_calls > 0, "{stats:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_cut_never_splits_a_call_from_its_result_at_any_position() {
    // A Claude user record carrying text and a tool_result: the text becomes a user message
    // between the call and its result. A cut there would orphan the result, so it is not a cut point.
    let dir = sandbox("pairs");
    let mut raw = String::new();
    raw.push_str(&json!({"type": "user", "uuid": "p0", "parentUuid": null, "sessionId": "x", "cwd": "/w",
        "timestamp": iso(0), "message": {"role": "user", "content": "start"}}).to_string());
    raw.push('\n');
    for i in 1..40 {
        let (a, u, t) = (format!("a{i}"), format!("u{i}"), format!("t{i}"));
        let prev = if i == 1 { "p0".to_string() } else { format!("t{}", i - 1) };
        raw.push_str(&json!({"type": "assistant", "uuid": a, "parentUuid": prev, "sessionId": "x", "timestamp": iso(i * 4),
            "message": {"role": "assistant", "model": MODEL, "content": [
                {"type": "tool_use", "id": format!("toolu_{i}"), "name": "Bash", "input": {"command": "ls"}}]}}).to_string());
        raw.push('\n');
        raw.push_str(&json!({"type": "user", "uuid": u, "parentUuid": a, "sessionId": "x", "timestamp": iso(i * 4 + 1),
            "message": {"role": "user", "content": [
                {"type": "text", "text": format!("while it runs {i}: {}", result_text(i))},
                {"type": "tool_result", "tool_use_id": format!("toolu_{i}"), "content": result_text(i), "is_error": false}]}}).to_string());
        raw.push('\n');
        raw.push_str(&json!({"type": "assistant", "uuid": t, "parentUuid": u, "sessionId": "x", "timestamp": iso(i * 4 + 2),
            "message": {"role": "assistant", "model": MODEL, "content": [{"type": "text", "text": format!("ok {i}")}]}}).to_string());
        raw.push('\n');
    }
    let (lines, _) = from_claude::to_journal_lines(&raw).unwrap();
    let j = journal_from_lines(&dir, "mixed.jsonl", &lines);
    let whole = j.window();
    let points = cut_points(&whole.msgs);
    assert_eq!(points.len(), 40, "the opening prompt and every prompt sent during a tool run are turn boundaries");
    assert!(
        whole.msgs.iter().all(|m| m.record.get("parentId").and_then(Value::as_str) != Some(m.id)),
        "no record links to itself"
    );
    for c in &points {
        assert!(pairs_whole(&whole.msgs[*c..]), "cut at {c} splits a call from its result");
    }
    let win = j.fit("claude", &budget(60_000)).unwrap();
    assert!(pairs_whole(&win.msgs), "the fitted window keeps every call with its result");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_summary_that_cannot_fit_is_refused_and_nothing_is_written() {
    let dir = sandbox("refuse-summary");
    let mut lines = omp_lines(300, Some((60, 20)));
    // Replace the compaction with one whose summary alone is far over the 200k window's limit.
    let idx = lines.iter().position(|l| l.contains("\"type\":\"compaction\"")).unwrap();
    lines[idx] = json!({"type": "compaction", "id": "c1", "parentId": "a79", "timestamp": iso(320),
        "summary": "x".repeat(600_000), "firstKeptEntryId": "u60"}).to_string();
    let j = journal_from_lines(&dir, "huge-summary.jsonl", &lines);
    let e = j.fit("claude", &budget(200_000)).err().expect("the fit must refuse");
    assert!(e.contains("nothing was written") && e.contains("need about"), "{e}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_newest_turn_that_cannot_fit_is_refused() {
    let dir = sandbox("refuse-turn");
    let mut lines = omp_lines(20, None);
    let last = lines.len() - 4;
    lines[last] = json!({"type": "message", "id": "u19", "parentId": "t18", "timestamp": iso(76),
        "message": {"role": "user", "content": [{"type": "text", "text": "y".repeat(600_000)}]}}).to_string();
    let j = journal_from_lines(&dir, "huge-turn.jsonl", &lines);
    let e = j.fit("opencode", &budget(128_000)).err().expect("the fit must refuse");
    assert!(e.contains("nothing was written"), "{e}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_session_without_a_compaction_says_what_was_left_out() {
    let dir = sandbox("no-compaction");
    let j = journal_from_lines(&dir, "long.jsonl", &omp_lines(300, None));
    let win = j.fit("claude", &budget(200_000)).unwrap();
    let (note, _) = win.note().unwrap();
    assert!(note.starts_with("Earlier turns of this conversation were left out"), "{note}");
    assert!(!note.contains("Their summary"), "no summary to carry");
    assert_eq!(win.msgs[0].role, "user");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn claude_and_opencode_compactions_become_compaction_records_the_window_starts_at() {
    let dir = sandbox("compaction-records");
    let (lines, stats) = from_claude::to_journal_lines(&claude_raw(80, Some(40))).unwrap();
    assert_eq!(stats.compactions, 1);
    let j = journal_from_lines(&dir, "claude-c.jsonl", &lines);
    assert_eq!(j.live_compaction().map(|(s, _)| s), Some(SUMMARY));
    let live: Vec<&str> = j.messages().iter().filter_map(|m| m.first_text()).collect();
    assert!(live.iter().all(|t| !t.starts_with("prompt 0")), "the archived turns are not live");
    assert!(live.contains(&"prompt 79"), "the post-compaction turns are live");

    let (lines, stats) = from_opencode::to_journal_lines(&opencode_export(80, Some(40))).unwrap();
    assert_eq!(stats.compactions, 1);
    let j = journal_from_lines(&dir, "opencode-c.jsonl", &lines);
    assert_eq!(j.live_compaction().map(|(s, _)| s), Some(SUMMARY));
    let live: Vec<&str> = j.messages().iter().filter_map(|m| m.first_text()).collect();
    assert!(live.iter().all(|t| !t.starts_with("prompt 0")), "the archived turns are not live");
    assert!(live.contains(&"prompt 79"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_journal_keeps_its_archive_for_the_whole_file_copies() {
    // to-omp / to-pi copy the journal, archive included; the fit is for the message-based writers.
    let dir = sandbox("omp-whole");
    let j = journal_from_lines(&dir, "omp.jsonl", &omp_lines(300, Some((60, 20))));
    assert_eq!(j.records.iter().filter(|r| r.get("type").and_then(Value::as_str) == Some("message")).count(), 1200);
    let _ = fs::remove_dir_all(&dir);
}
