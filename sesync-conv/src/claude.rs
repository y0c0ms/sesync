//! Journal -> Claude Code project transcript (`~/.claude/projects/<bucket>/<uuid>.jsonl`).
//!
//! Shapes copied from a session-migrate mirror that Claude Code 2.1.245
//! resumed and recalled correctly (2026-10-12). Every record carries the
//! same envelope; the conversation is a `uuid`/`parentUuid` chain:
//!   user       -> {type:"user", message:{role:"user", content: string | [blocks]}}
//!   assistant  -> {type:"assistant", message:{id, type:"message", role, model,
//!                  content:[text | tool_use], stop_reason, stop_sequence, usage}}
//!   toolResult -> a user record whose content is [{type:"tool_result", tool_use_id, content, is_error}]
//!
//! Thinking is NOT carried: Claude signs thinking blocks per conversation and
//! rejects foreign ones on resume. This is the one edge where it is lost.
//!
//! Tool ids are remapped to Claude's native `toolu_` + 24 chars: its session
//! loader drops a tool_use whose id does not look native (omp mints 32-char
//! tokens), and the call then vanishes from the resumed conversation.
//!
//! Ids are uuid5 of (session seed, record id) so a re-run rewrites the same
//! uuids and the daemon's replace-in-place keeps working.

use crate::journal::{iso_from_ms, Journal};
use serde_json::{json, Map, Value};
use uuid::Uuid;

pub struct Options<'a> {
    pub session_id: &'a str,
    pub cwd: &'a str,
    pub cli_version: &'a str,
    pub title: &'a str,
}

const NS: Uuid = Uuid::from_bytes([0x5e, 0x55, 0xa0, 0xb1, 0x7c, 0x3e, 0x4d, 0x2a, 0x9f, 0x1b, 0x0a, 0x12, 0xab, 0x34, 0xcd, 0x56]);

pub fn session_id_for(seed: &str) -> String {
    Uuid::new_v5(&NS, format!("{seed}|claude").as_bytes()).to_string()
}

/// A tool id in Claude's native shape (`toolu_` + 24 chars). Deterministic per raw id,
/// so a call and its result map to the same id on every refresh.
fn tool_id(raw: &str) -> String {
    format!("toolu_{}", &Uuid::new_v5(&NS, format!("{raw}|tool").as_bytes()).simple().to_string()[..24])
}

pub fn to_lines(j: &Journal, o: &Options) -> Vec<String> {
    let msgs = j.messages();
    let mut out = Vec::new();
    // The session starts with a custom-title, as /rename and `-n` write. Claude
    // prefers it over ai-title; without one it labels the session by its first
    // user message, which reads as a pre-rename name.
    if !o.title.trim().is_empty() {
        out.push(serde_json::to_string(&json!({"type": "custom-title", "customTitle": o.title.trim(), "sessionId": o.session_id})).unwrap());
    }
    let mut parent: Option<String> = None;
    let mut model = String::from("unknown");
    let mut n = 0u64;
    let mut uid = |rid: &str| {
        n += 1;
        Uuid::new_v5(&NS, format!("{}|{}|{}", o.session_id, rid, n).as_bytes()).to_string()
    };

    // The turns a compaction archived are not in `msgs` any more: its summary is the only record
    // of them, so it is carried as the first message. Without this a compacted journal exports a
    // conversation that simply starts mid-thread.
    if let (Some(text), Some((_, ts))) = (j.compaction_note(), j.live_compaction()) {
        let uuid = uid("compaction-summary");
        let rec = json!({
            "parentUuid": parent, "isSidechain": false, "userType": "external",
            "cwd": o.cwd, "sessionId": o.session_id, "version": o.cli_version, "gitBranch": "",
            "uuid": uuid, "timestamp": ts, "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": text}]}
        });
        out.push(serde_json::to_string(&rec).unwrap());
        parent = Some(uuid);
    }

    for m in &msgs {
        let ts = iso_from_ms(m.timestamp_ms);
        let (typ, message): (&str, Value) = match m.role {
            "user" => {
                let mut blocks: Vec<Value> = Vec::new();
                let mut only_text: Option<String> = None;
                for b in &m.content {
                    match b {
                        Value::String(s) => only_text = Some(s.clone()),
                        Value::Object(ob) => match ob.get("type").and_then(Value::as_str) {
                            Some("text") => blocks.push(json!({"type": "text", "text": ob.get("text").cloned().unwrap_or(Value::String(String::new()))})),
                            Some("image") => {
                                let data = ob.get("data").and_then(Value::as_str).unwrap_or("");
                                if !data.starts_with("blob:") && !data.is_empty() {
                                    blocks.push(json!({"type": "image", "source": {"type": "base64", "media_type": ob.get("mimeType").cloned().unwrap_or("image/png".into()), "data": data}}));
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                let content = match (only_text, blocks.len()) {
                    (Some(t), 0) => Value::String(t),
                    (_, 1) if blocks[0].get("type").and_then(Value::as_str) == Some("text") => blocks[0]["text"].clone(),
                    _ => Value::Array(blocks),
                };
                if content == Value::String(String::new()) || content == Value::Array(vec![]) {
                    continue;
                }
                ("user", json!({"role": "user", "content": content}))
            }
            "assistant" => {
                if let Some(mo) = m.message.get("model").and_then(Value::as_str) {
                    model = mo.to_string();
                }
                let mut blocks: Vec<Value> = Vec::new();
                for b in &m.content {
                    let Value::Object(ob) = b else { continue };
                    match ob.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let t = ob.get("text").and_then(Value::as_str).unwrap_or("");
                            if !t.is_empty() {
                                blocks.push(json!({"type": "text", "text": t}));
                            }
                        }
                        // Thinking from another model is carried as marked text, never as a
                        // thinking block (Claude signs its own and rejects foreign ones).
                        Some("thinking") => {
                            let t = ob.get("thinking").and_then(Value::as_str).unwrap_or("");
                            let model = m.message.get("model").and_then(Value::as_str).unwrap_or("unknown");
                            if let Some(w) = crate::prior_reasoning::wrap(t, "omp", &model) {
                                blocks.push(json!({"type": "text", "text": w}));
                            }
                        }
                        Some("toolCall") => blocks.push(json!({
                            "type": "tool_use",
                            "id": tool_id(ob.get("id").and_then(Value::as_str).unwrap_or("call")),
                            "name": ob.get("name").cloned().unwrap_or(Value::String("tool".into())),
                            "input": ob.get("arguments").cloned().unwrap_or(Value::Object(Map::new()))
                        })),
                        _ => {}
                    }
                }
                if blocks.is_empty() {
                    continue;
                }
                let stop = if blocks.iter().any(|b| b["type"] == "tool_use") { "tool_use" } else { "end_turn" };
                // Stored id prefix: existing converted messages carry it, so it keeps its original value.
                let mid = format!("msg_hsconv_{}", &Uuid::new_v5(&NS, m.id.as_bytes()).simple().to_string()[..24]);
                ("assistant", json!({
                    "id": mid, "type": "message", "role": "assistant", "model": model,
                    "content": blocks, "stop_reason": stop, "stop_sequence": null,
                    "usage": {"input_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 0}
                }))
            }
            "toolResult" => {
                let cid = m.message.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                let is_err = m.message.get("isError").and_then(Value::as_bool).unwrap_or(false);
                let text = m.content.iter().filter_map(|c| match c {
                    Value::String(s) => Some(s.as_str()),
                    Value::Object(x) => x.get("text").and_then(Value::as_str),
                    _ => None,
                }).collect::<Vec<_>>().join("\n");
                ("user", json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_id(cid), "content": text, "is_error": is_err}]}))
            }
            _ => continue,
        };
        let uuid = uid(m.id);
        let rec = json!({
            "parentUuid": parent, "isSidechain": false, "userType": "external",
            "cwd": o.cwd, "sessionId": o.session_id, "version": o.cli_version, "gitBranch": "",
            "uuid": uuid, "timestamp": ts, "type": typ, "message": message
        });
        out.push(serde_json::to_string(&rec).unwrap());
        parent = Some(uuid);
    }
    out
}

/// Claude's bucket: the absolute cwd with every non-alphanumeric char as '-'.
/// Verified: <drive>:\<home>\<project> -> C--<home>-<project>
pub fn bucket(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal_of(lines: &[String]) -> Journal {
        // Unique per call: tests run in parallel, and a shared name let one test read
        // another's half-written file.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("hs-claude-rt-{}-{n}.jsonl", std::process::id()));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let j = Journal::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        j
    }

    const TOOLS: &str = concat!(
        r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","cwd":"/tmp/c","timestamp":"2026-10-01T10:00:00.000Z","message":{"role":"user","content":"list files"}}"#, "\n",
        r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"s","timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Listing."},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}"#, "\n",
        r#"{"type":"user","uuid":"r1","parentUuid":"a1","sessionId":"s","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"a.txt","is_error":true}]}}"#, "\n",
        r#"{"type":"assistant","uuid":"a2","parentUuid":"r1","sessionId":"s","timestamp":"2026-10-01T10:00:03.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Done."}]}}"#, "\n",
    );

    #[test]
    fn a_claude_tool_call_and_result_survive_a_round_trip_through_the_journal() {
        let (lines, _) = crate::from_claude::to_journal_lines(TOOLS).unwrap();
        let j = journal_of(&lines);
        let out = to_lines(&j, &Options { session_id: "s2", cwd: "/tmp/c", cli_version: "2.1.245", title: "" });
        let recs: Vec<Value> = out.iter().map(|l| serde_json::from_str(l).unwrap()).collect();

        let call = recs.iter().flat_map(|r| r["message"]["content"].as_array().cloned().unwrap_or_default())
            .find(|b| b["type"] == "tool_use").expect("tool_use carried back into Claude");
        let call_id = call["id"].as_str().unwrap().to_string();
        assert!(call_id.starts_with("toolu_") && call_id.len() == 30, "native id shape: {call_id}");
        assert_eq!(call["name"], "Bash");
        assert_eq!(call["input"]["command"], "ls");

        let result = recs.iter().flat_map(|r| r["message"]["content"].as_array().cloned().unwrap_or_default())
            .find(|b| b["type"] == "tool_result").expect("tool_result carried back into Claude");
        assert_eq!(result["tool_use_id"], call_id, "the result still answers its call");
        assert_eq!(result["is_error"], true);
        assert_eq!(result["content"], "a.txt");
    }

    #[test]
    fn a_gemini_tool_call_and_result_survive_a_round_trip_into_claude() {
        let raw = concat!(
            r#"{"sessionId":"gs","projectHash":"h","startTime":"2026-10-01T10:00:00.000Z","directories":["/tmp/g"]}"#, "\n",
            r#"{"id":"u1","timestamp":"2026-10-01T10:00:01.000Z","type":"user","content":[{"text":"list files"}]}"#, "\n",
            r#"{"id":"g1","timestamp":"2026-10-01T10:00:02.000Z","type":"gemini","content":[{"text":"Listing."},{"functionCall":{"id":"ls__1","name":"list_directory","args":{"path":"."}}}]}"#, "\n",
            r#"{"id":"r1","timestamp":"2026-10-01T10:00:03.000Z","type":"user","content":[{"functionResponse":{"id":"ls__1","name":"list_directory","response":{"output":"a.txt"}}}]}"#, "\n",
            r#"{"id":"g2","timestamp":"2026-10-01T10:00:04.000Z","type":"gemini","content":[{"text":"Done."}]}"#, "\n",
        );
        let (lines, _) = crate::from_gemini::to_journal_lines(raw).unwrap();
        let out = to_lines(&journal_of(&lines), &Options { session_id: "s3", cwd: "/tmp/g", cli_version: "2.1.245", title: "" });
        let blocks: Vec<Value> = out.iter()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .flat_map(|r| r["message"]["content"].as_array().cloned().unwrap_or_default())
            .collect();
        let call_at = blocks.iter().position(|b| b["type"] == "tool_use").expect("tool_use carried into Claude");
        let call_id = blocks[call_at]["id"].as_str().unwrap().to_string();
        assert!(call_id.starts_with("toolu_"), "gemini call ids are remapped too: {call_id}");
        assert_eq!(blocks[call_at]["name"], "list_directory");
        let res_at = blocks.iter().position(|b| b["type"] == "tool_result").expect("tool_result carried into Claude");
        assert_eq!(blocks[res_at]["tool_use_id"], call_id);
        assert!(res_at > call_at, "result must come after its call");
    }

    #[test]
    fn every_tool_result_in_the_output_answers_an_earlier_tool_use() {
        // Claude rejects a tool_result with no tool_use before it. Check the output order.
        let (lines, _) = crate::from_claude::to_journal_lines(TOOLS).unwrap();
        let out = to_lines(&journal_of(&lines), &Options { session_id: "s2", cwd: "/tmp/c", cli_version: "2.1.245", title: "" });
        let mut seen = std::collections::HashSet::new();
        for l in &out {
            let r: Value = serde_json::from_str(l).unwrap();
            for b in r["message"]["content"].as_array().cloned().unwrap_or_default() {
                if b["type"] == "tool_use" {
                    seen.insert(b["id"].as_str().unwrap().to_string());
                }
                if b["type"] == "tool_result" {
                    assert!(seen.contains(b["tool_use_id"].as_str().unwrap()),
                        "tool_result before its tool_use: {l}");
                }
            }
        }
    }

    #[test]
    fn an_opencode_tool_call_reaches_claude_paired_with_its_result() {
        let export: Value = serde_json::from_str(r#"{
          "info": {"id": "ses_x", "directory": "/tmp/p", "time": {"created": 1792000000000}},
          "messages": [
            {"info": {"id": "msg_u", "role": "user", "time": {"created": 1792000000001}},
             "parts": [{"type": "text", "text": "run ls"}]},
            {"info": {"id": "msg_a", "role": "assistant", "time": {"created": 1792000000002}},
             "parts": [{"type": "tool", "callID": "call_1", "tool": "bash",
                        "state": {"status": "completed", "input": {"command": "ls"}, "output": "a.txt"}}]},
            {"info": {"id": "msg_b", "role": "assistant", "time": {"created": 1792000000003}},
             "parts": [{"type": "text", "text": "done"}]}
          ]
        }"#).unwrap();
        let (lines, _) = crate::from_opencode::to_journal_lines(&export).unwrap();
        let out = to_lines(&journal_of(&lines), &Options { session_id: "s3", cwd: "/tmp/p", cli_version: "2.1.245", title: "" });
        let blocks: Vec<Value> = out.iter()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .flat_map(|r| r["message"]["content"].as_array().cloned().unwrap_or_default())
            .collect();
        let call = blocks.iter().position(|b| b["type"] == "tool_use" && b["name"] == "bash").expect("call");
        let mapped = blocks[call]["id"].as_str().unwrap().to_string();
        assert!(mapped.starts_with("toolu_"), "opencode call ids are remapped too: {mapped}");
        let res = blocks.iter().position(|b| b["type"] == "tool_result" && b["tool_use_id"] == mapped.as_str()).expect("result");
        assert!(call < res, "the result must come after its call");
        assert_eq!(blocks[res]["content"], "a.txt");
    }

    #[test]
    fn thinking_survives_a_round_trip_as_one_marker() {
        // omp -> Claude -> journal must keep exactly one <prior_reasoning> marker: the
        // writer carries text blocks through, and the reader must not wrap them again.
        let journal_src = r#"{"type":"session","version":3,"id":"s","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/c"}
{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"why"}]}}
{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"weigh it","thinkingSignature":"OMP-SIG"},{"type":"text","text":"Because."}]}}
"#;
        let path = std::env::temp_dir().join(format!("hs-rt-marker-{}.jsonl", std::process::id()));
        std::fs::write(&path, journal_src).unwrap();
        let j1 = journal_of(&[journal_src.lines().map(str::to_string).collect::<Vec<_>>().join("\n")]);
        let _ = std::fs::remove_file(&path);
        let claude_lines = to_lines(&j1, &Options { session_id: "s4", cwd: "/tmp/c", cli_version: "2.1.245", title: "" });
        let claude_text = claude_lines.join("\n");
        assert!(!claude_text.contains("OMP-SIG"), "omp signature must never reach Claude");
        assert!(claude_text.contains("weigh it"), "thinking words must reach Claude as text");
        // Read the Claude output back in: the marker must not be wrapped a second time.
        let (back, _) = crate::from_claude::to_journal_lines(&claude_text).unwrap();
        let back_text = back.join("\n");
        assert_eq!(back_text.matches("<prior_reasoning").count(), 1, "one marker after the round trip: {back_text}");
        assert!(!back_text.contains("<prior_reasoning harness=\\\"claude\\\" model=\\\"unknown\\\"><prior_reasoning"));
    }

    #[test]
    fn an_image_survives_omp_to_claude_and_back() {
        let journal_src = r#"{"type":"session","version":3,"id":"s","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/c"}
{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"what is this"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}]}}
{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"A logo."}]}}
"#;
        let path = std::env::temp_dir().join(format!("hs-img-rt-{}.jsonl", std::process::id()));
        std::fs::write(&path, journal_src).unwrap();
        let j = Journal::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let out = to_lines(&j, &Options { session_id: "s5", cwd: "/tmp/c", cli_version: "2.1.245", title: "" });
        let claude_text = out.join("\n");
        assert!(claude_text.contains("\"type\":\"image\""), "image must be written for Claude");
        assert!(claude_text.contains("iVBORw0KGgo="));
        let (back, stats) = crate::from_claude::to_journal_lines(&claude_text).unwrap();
        assert_eq!(stats.images, 1, "the image must come back");
        assert!(back.join("\n").contains("iVBORw0KGgo="));
    }

    #[test]
    fn the_parent_chain_is_unbroken() {
        let (lines, _) = crate::from_claude::to_journal_lines(TOOLS).unwrap();
        let out = to_lines(&journal_of(&lines), &Options { session_id: "s2", cwd: "/tmp/c", cli_version: "2.1.245", title: "" });
        let recs: Vec<Value> = out.iter().map(|l| serde_json::from_str(l).unwrap()).collect();
        for w in recs.windows(2) {
            assert_eq!(w[1]["parentUuid"], w[0]["uuid"], "each record follows the one before it");
        }
    }

    #[test]
    fn a_rename_inside_omp_becomes_the_custom_title_at_the_head() {
        // omp appends `title_change` on /rename and the slot can lag behind; the
        // mirror must carry the renamed title, not the slot's older name.
        let mut lines = vec![
            crate::journal::Journal::title_slot_line("Converted name", "2026-10-01T10:00:00.000Z"),
            r#"{"type":"session","version":3,"id":"s","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/c","title":"Old header"}"#.to_string(),
        ];
        lines.extend(TOOLS.lines().map(str::to_string));
        lines.push(r#"{"type":"title_change","id":"t1","previousTitle":"Converted name","title":"Renamed inside omp"}"#.to_string());
        let j = journal_of(&lines);
        assert_eq!(j.title_text(), "Renamed inside omp");
        let out = to_lines(&j, &Options { session_id: "sid", cwd: "/tmp/c", cli_version: "2.1.245", title: &j.title_text() });
        let first: Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(first["type"], "custom-title");
        assert_eq!(first["customTitle"], "Renamed inside omp");
        assert_eq!(first["sessionId"], "sid");
    }

    #[test]
    fn a_nameless_session_gets_no_custom_title_record() {
        let j = journal_of(&[
            r#"{"type":"session","version":3,"id":"s","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/c"}"#.to_string(),
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#.to_string(),
        ]);
        let out = to_lines(&j, &Options { session_id: "s2", cwd: "/tmp/c", cli_version: "2.1.245", title: "" });
        let first: Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(first["type"], "user");
    }

    #[test]
    fn a_foreign_tool_id_is_mapped_to_a_native_one() {
        // omp mints 32-char tokens for tool calls; Claude's session loader drops a
        // tool_use whose id is not `toolu_`-shaped, which hides the call entirely.
        let j = journal_of(&[
            r#"{"type":"session","version":3,"id":"s","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/c"}"#.to_string(),
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"run"}]}}"#.to_string(),
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"Men3jzwBrwVF1DvrkLIVDL2bmttFsODi","name":"bash","arguments":{"command":"ls"}}]}}"#.to_string(),
            r#"{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-10-01T10:00:03.000Z","message":{"role":"toolResult","toolCallId":"Men3jzwBrwVF1DvrkLIVDL2bmttFsODi","isError":false,"content":[{"type":"text","text":"ok"}]}}"#.to_string(),
        ]);
        let out = to_lines(&j, &Options { session_id: "sid", cwd: "/tmp/c", cli_version: "2.1.245", title: "" });
        let recs: Vec<Value> = out.iter().map(|l| serde_json::from_str(l).unwrap()).collect();
        let blocks: Vec<Value> = recs.iter().flat_map(|r| r["message"]["content"].as_array().cloned().unwrap_or_default()).collect();
        let call = blocks.iter().find(|b| b["type"] == "tool_use").expect("tool_use");
        let id = call["id"].as_str().unwrap();
        assert!(id.starts_with("toolu_") && id.len() == 30, "got {id}");
        let res = blocks.iter().find(|b| b["type"] == "tool_result").expect("tool_result");
        assert_eq!(res["tool_use_id"], id, "the result answers the mapped call");
    }

    /// Same compacted journal as every other target's test: the live window is exported, the
    /// archive is dropped, and the summary is carried as a message because Claude has no
    /// compaction record of its own to replace those turns.
    #[test]
    fn a_compacted_journal_exports_only_the_live_window() {
        let j = crate::journal::fixture::compacted_journal("claude");
        let out = to_lines(&j, &Options { session_id: "sid", cwd: "/tmp/c", cli_version: "2.1.245", title: "" });
        let text = out.join("\n");
        for kept in [crate::journal::fixture::KEPT_ONE, crate::journal::fixture::KEPT_TWO, crate::journal::fixture::NEWEST] {
            assert!(text.contains(kept), "the live window is exported: {kept}");
        }
        for archived in [crate::journal::fixture::ARCHIVED_ONE, crate::journal::fixture::ARCHIVED_TWO] {
            assert!(!text.contains(archived), "turns archived by the compaction must not be exported: {archived}");
        }
        assert!(text.contains(crate::journal::fixture::SUMMARY), "the compaction summary stands in for them");
    }
}
