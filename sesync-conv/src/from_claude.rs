//! Claude Code session JSONL -> omp/Pi journal.
//!
//! Input is a Claude Code transcript (`~/.claude/projects/<cwd>/<uuid>.jsonl`).
//! Output is an omp journal that every other converter in this crate accepts.
//!
//! Mapping:
//!   uuid / parentUuid   -> id / parentId (the chain the reader walks)
//!   sessionId, cwd      -> session header
//!   user / assistant    -> message records (role from `message.role`)
//!   text blocks         -> text content blocks
//!   tool_use            -> toolCall block {id, name, arguments} in the assistant message
//!   tool_result         -> one toolResult message per result {toolCallId, toolName,
//!                          content, isError}, placed right after the record it came from
//!   thinking, attachment, file-history-snapshot, mode, permission-mode, system,
//!   last-prompt -> not converted; counted
//!   a line that is not a JSON object (a truncated write) -> skipped and counted;
//!   real transcripts carry these (killed processes), and Claude itself still
//!   resumes them, so one bad line must not lose the whole session
//!
//! Tool results are paired with calls by id, as Claude requires: a result whose call was
//! never seen (orphan) or a second result for the same call (duplicate) is dropped and
//! counted. Rules from session-migrate (xhluca, MIT); see docs/research/session-fidelity.md.
//!   isMeta user records and `<tag>` wrapper text -> not conversation turns
//!
//! Claude signs its reasoning blocks and cannot accept ours back, so thinking
//! is never carried into a Claude mirror; here it is dropped on the way in.

use serde_json::{json, Map, Value};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub messages: usize,
    pub text_blocks: usize,
    pub skipped_tool_blocks: usize,
    pub skipped_thinking_blocks: usize,
    pub thinking_kept: usize,
    pub images: usize,
    pub skipped_image_blocks: usize,
    pub skipped_records: usize,
    /// Lines that are not a JSON object at all: a truncated write, or a shape drift.
    pub skipped_bad_lines: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub orphan_tool_results: usize,
    pub duplicate_tool_results: usize,
}

/// Convert the whole transcript text. Returns the omp journal lines, header
/// first. Lines that are not a JSON object are skipped and counted; the rest of
/// the transcript still converts.
pub fn to_journal_lines(raw: &str) -> Result<(Vec<String>, Stats), String> {
    let mut stats = Stats::default();
    let mut session_id = String::new();
    let mut cwd = String::new();
    let mut first_ts = String::new();
    let mut out: Vec<Map<String, Value>> = Vec::new();
    // uuid -> its parent uuid, for EVERY record (kept or skipped). A skipped
    // record (tool-only turn, attachment) is still a link in the chain, so a
    // kept message's parent is found by walking up through skipped ones.
    let mut parent_of: std::collections::HashMap<String, Option<String>> = Default::default();
    let mut kept_parent: Vec<(String, Option<String>)> = Vec::new();
    // A source record that was split into several messages: its uuid -> the id of the last
    // one, so a later record whose parent is that uuid links to the end of the split.
    let mut alias: std::collections::HashMap<String, String> = Default::default();
    // Tool call id -> tool name, for every call seen so far; results pair against it.
    let mut calls: std::collections::HashMap<String, String> = Default::default();
    let mut answered: std::collections::HashSet<String> = Default::default();
    // Results split off a source record, as (id, message) pairs placed after it.
    let mut result_msgs: Vec<(String, Map<String, Value>)>;

    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(t) else {
            stats.skipped_bad_lines += 1;
            continue;
        };
        let Value::Object(r) = rec else {
            stats.skipped_bad_lines += 1;
            continue;
        };
        if session_id.is_empty() {
            if let Some(s) = r.get("sessionId").and_then(Value::as_str) {
                session_id = s.to_string();
            }
        }
        if cwd.is_empty() {
            if let Some(c) = r.get("cwd").and_then(Value::as_str) {
                cwd = c.to_string();
            }
        }
        let uuid_here = r.get("uuid").and_then(Value::as_str).unwrap_or("").to_string();
        if !uuid_here.is_empty() {
            let parent_here = r.get("parentUuid").and_then(Value::as_str).map(str::to_string);
            parent_of.insert(uuid_here, parent_here);
        }
        let kind = r.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "user" && kind != "assistant" {
            stats.skipped_records += 1;
            continue;
        }
        if r.get("isMeta").and_then(Value::as_bool).unwrap_or(false) {
            stats.skipped_records += 1;
            continue;
        }
        let Some(msg) = r.get("message").and_then(Value::as_object) else {
            stats.skipped_records += 1;
            continue;
        };
        let role = msg.get("role").and_then(Value::as_str).unwrap_or(kind);
        let mut content: Vec<Value> = Vec::new();
        result_msgs = Vec::new();
        match msg.get("content") {
            Some(Value::String(s)) => {
                if is_real_text(s) {
                    content.push(json!({"type": "text", "text": s}));
                    stats.text_blocks += 1;
                }
            }
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let s = b.get("text").and_then(Value::as_str).unwrap_or("");
                            if is_real_text(s) {
                                content.push(json!({"type": "text", "text": s}));
                                stats.text_blocks += 1;
                            }
                        }
                        Some("thinking") => {
                            let t = b.get("thinking").and_then(Value::as_str).unwrap_or("");
                            // The signature is never copied; the words are kept as marked text.
                            match crate::prior_reasoning::wrap(t, "claude", &model_of(&msg)) {
                                Some(w) => {
                                    content.push(json!({"type": "text", "text": w}));
                                    stats.thinking_kept += 1;
                                }
                                None => stats.skipped_thinking_blocks += 1,
                            }
                        }
                        Some("redacted_thinking") => stats.skipped_thinking_blocks += 1,
                        Some("image") => {
                            let src = b.get("source").and_then(Value::as_object);
                            let kind = src.and_then(|x| x.get("type")).and_then(Value::as_str).unwrap_or("");
                            let data = src.and_then(|x| x.get("data")).and_then(Value::as_str).unwrap_or("");
                            let mime = src.and_then(|x| x.get("media_type")).and_then(Value::as_str).unwrap_or("image/png");
                            if kind == "base64" && !data.is_empty() {
                                content.push(json!({"type": "image", "data": data, "mimeType": mime}));
                                stats.images += 1;
                            } else {
                                stats.skipped_image_blocks += 1;
                            }
                        }
                        Some("tool_use") if role == "assistant" => {
                            let id = b.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                            let name = b.get("name").and_then(Value::as_str).unwrap_or("unknown_tool").to_string();
                            let mut args = b.get("input").cloned().unwrap_or_else(|| json!({}));
                            if !args.is_object() {
                                args = json!({"input": args});
                            }
                            if id.is_empty() {
                                stats.skipped_tool_blocks += 1;
                                continue;
                            }
                            calls.insert(id.clone(), name.clone());
                            content.push(json!({"type": "toolCall", "id": id, "name": name, "arguments": args}));
                            stats.tool_calls += 1;
                        }
                        Some("tool_result") => {
                            let cid = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("").to_string();
                            let Some(name) = calls.get(&cid).cloned() else {
                                stats.orphan_tool_results += 1;
                                continue;
                            };
                            if !answered.insert(cid.clone()) {
                                stats.duplicate_tool_results += 1;
                                continue;
                            }
                            let mut text_blocks: Vec<Value> = Vec::new();
                            match b.get("content") {
                                Some(Value::String(t)) => text_blocks.push(json!({"type": "text", "text": t})),
                                Some(Value::Array(items)) => {
                                    for it in items {
                                        if it.get("type").and_then(Value::as_str) == Some("text") {
                                            let t = it.get("text").and_then(Value::as_str).unwrap_or("");
                                            text_blocks.push(json!({"type": "text", "text": t}));
                                        } else {
                                            stats.skipped_tool_blocks += 1;
                                        }
                                    }
                                }
                                _ => {}
                            }
                            let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                            let rid = format!("{}-r{}", r.get("uuid").and_then(Value::as_str).unwrap_or(""), result_msgs.len());
                            let mut m = Map::new();
                            m.insert("role".into(), "toolResult".into());
                            m.insert("toolCallId".into(), cid.into());
                            m.insert("toolName".into(), name.into());
                            m.insert("content".into(), Value::Array(text_blocks));
                            m.insert("isError".into(), is_error.into());
                            result_msgs.push((rid, m));
                            stats.tool_results += 1;
                        }
                        Some(_) => stats.skipped_tool_blocks += 1,
                        None => {}
                    }
                }
            }
            _ => {}
        }
        if content.is_empty() && result_msgs.is_empty() {
            stats.skipped_records += 1;
            continue;
        }
        let id = r.get("uuid").and_then(Value::as_str).unwrap_or("").to_string();
        let parent = r.get("parentUuid").and_then(Value::as_str).map(str::to_string);
        let ts = r.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
        if first_ts.is_empty() && !ts.is_empty() {
            first_ts = ts.clone();
        }
        // The source record's own content, if any, keeps the source uuid.
        let mut chain_parent = parent.clone();
        let mut last_id: Option<String> = None;
        if !content.is_empty() {
            kept_parent.push((id.clone(), parent.clone()));
            let mut m = Map::new();
            m.insert("type".into(), "message".into());
            m.insert("id".into(), id.clone().into());
            m.insert("parentId".into(), parent.clone().map(Value::from).unwrap_or(Value::Null));
            m.insert("timestamp".into(), ts.clone().into());
            m.insert("message".into(), json!({"role": role, "content": content}));
            out.push(m);
            stats.messages += 1;
            last_id = Some(id.clone());
        }
        // Each result is its own message, chained one after another. The first result
        // links to the source's parent (the record holding the call); later ones link to
        // the result before them. Results are resolved against the source parent below.
        for (rid, rm) in result_msgs.drain(..) {
            let p = last_id.clone().or_else(|| chain_parent.clone());
            kept_parent.push((rid.clone(), p.clone()));
            let mut m = Map::new();
            m.insert("type".into(), "message".into());
            m.insert("id".into(), rid.clone().into());
            m.insert("parentId".into(), p.map(Value::from).unwrap_or(Value::Null));
            m.insert("timestamp".into(), ts.clone().into());
            m.insert("message".into(), Value::Object(rm));
            out.push(m);
            stats.messages += 1;
            last_id = Some(rid);
            chain_parent = None;
        }
        // Later records whose parent is this source uuid must link to its last message.
        if let Some(l) = &last_id {
            if *l != id {
                alias.insert(id.clone(), l.clone());
            }
        }
    }

    if stats.messages == 0 {
        return Err("transcript has no user or assistant text to convert".into());
    }
    // Map each kept message's parent to its nearest KEPT ancestor, walking up
    // through skipped records. This keeps the chain unbroken.
    let kept: std::collections::HashSet<String> = out.iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string)).collect();
    let resolve = |start: Option<String>| -> Option<String> {
        let mut cur = start;
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = cur.take() {
            if let Some(a) = alias.get(&id) {
                return Some(a.clone());
            }
            if kept.contains(&id) {
                return Some(id);
            }
            if !seen.insert(id.clone()) {
                return None; // cycle in the source; stop rather than loop
            }
            cur = parent_of.get(&id).cloned().flatten();
        }
        None
    };
    let resolved: std::collections::HashMap<String, Option<String>> = kept_parent.into_iter()
        .map(|(id, parent)| (id, resolve(parent)))
        .collect();
    for m in out.iter_mut() {
        let id = m.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let p = resolved.get(&id).cloned().flatten();
        m.insert("parentId".into(), p.map(Value::from).unwrap_or(Value::Null));
    }
    if session_id.is_empty() {
        return Err("transcript has no sessionId".into());
    }

    let mut header = Map::new();
    header.insert("type".into(), "session".into());
    header.insert("version".into(), 3.into());
    header.insert("id".into(), session_id.into());
    header.insert("timestamp".into(), if first_ts.is_empty() { "1970-01-01T00:00:00.000Z".into() } else { first_ts.into() });
    header.insert("cwd".into(), cwd.into());
    let mut lines = vec![serde_json::to_string(&header).map_err(|e| e.to_string())?];
    for m in &out {
        lines.push(serde_json::to_string(m).map_err(|e| e.to_string())?);
    }
    Ok((lines, stats))
}

/// The model that wrote a Claude assistant message, from its message.model field.
fn model_of(msg: &Map<String, Value>) -> String {
    msg.get("model").and_then(Value::as_str).unwrap_or("unknown").to_string()
}

/// Claude wraps local commands and reminders in <tags>; those are not turns.
fn is_real_text(s: &str) -> bool {
    let t = s.trim();
    // A <prior_reasoning> marker is conversation (earlier reasoning), not a wrapper tag.
    crate::prior_reasoning::is_wrapped(t) || (!t.is_empty() && !t.starts_with('<'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = concat!(
        r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"sess-1","cwd":"/tmp/c","timestamp":"2026-10-01T10:00:00.000Z","message":{"role":"user","content":"hello claude"}}"#, "\n",
        r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"sess-1","cwd":"/tmp/c","timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"hi there"}]}}"#, "\n",
        r#"{"type":"attachment","uuid":"x1","sessionId":"sess-1"}"#, "\n",
        r#"{"type":"user","uuid":"u2","parentUuid":"a1","sessionId":"sess-1","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"user","content":[{"type":"text","text":"second"}]}}"#, "\n",
    );

    #[test]
    fn keeps_turns_in_order_and_links_them() {
        let (lines, stats) = to_journal_lines(T).unwrap();
        assert_eq!(lines.len(), 4, "header + 3 messages");
        let a: Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(a["parentId"], "u1");
        // The thinking marker comes first, then the answer.
        assert!(a["message"]["content"][0]["text"].as_str().unwrap().starts_with("<prior_reasoning"));
        assert_eq!(a["message"]["content"][1]["text"], "hi there");
        assert_eq!(stats.messages, 3);
    }

    // A turn with a tool call and its result, as Claude Code writes them: the call is a
    // tool_use block in an assistant record; the result is a tool_result block in the
    // next user record, linked by tool_use_id.
    const TOOLS: &str = concat!(
        r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","cwd":"/tmp/c","timestamp":"2026-10-01T10:00:00.000Z","message":{"role":"user","content":"list files"}}"#, "\n",
        r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"s","timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Listing."},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}"#, "\n",
        r#"{"type":"user","uuid":"r1","parentUuid":"a1","sessionId":"s","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"a.txt\nb.txt","is_error":false}]}}"#, "\n",
        r#"{"type":"assistant","uuid":"a2","parentUuid":"r1","sessionId":"s","timestamp":"2026-10-01T10:00:03.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Two files."}]}}"#, "\n",
    );

    fn records(lines: &[String]) -> Vec<Value> {
        lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn tool_use_becomes_a_tool_call_block() {
        let (lines, stats) = to_journal_lines(TOOLS).unwrap();
        let r = records(&lines);
        let a1 = r.iter().find(|m| m["id"] == "a1").expect("assistant turn kept");
        let call = &a1["message"]["content"][1];
        assert_eq!(call["type"], "toolCall");
        assert_eq!(call["id"], "toolu_1");
        assert_eq!(call["name"], "Bash");
        assert_eq!(call["arguments"]["command"], "ls");
        assert_eq!(stats.tool_calls, 1);
    }

    #[test]
    fn tool_result_becomes_a_tool_result_message_linked_to_its_call() {
        let (lines, stats) = to_journal_lines(TOOLS).unwrap();
        let r = records(&lines);
        let res = r.iter().find(|m| m["message"]["role"] == "toolResult").expect("result kept");
        assert_eq!(res["message"]["toolCallId"], "toolu_1");
        assert_eq!(res["message"]["toolName"], "Bash");
        assert_eq!(res["message"]["isError"], false);
        assert_eq!(res["message"]["content"][0]["text"], "a.txt\nb.txt");
        assert_eq!(res["parentId"], "a1", "the result follows the call that made it");
        assert_eq!(stats.tool_results, 1);
        assert_eq!(stats.skipped_tool_blocks, 0);
    }

    #[test]
    fn the_turn_after_a_tool_result_links_to_the_result() {
        let (lines, _) = to_journal_lines(TOOLS).unwrap();
        let r = records(&lines);
        let res_id = r.iter().find(|m| m["message"]["role"] == "toolResult").unwrap()["id"].clone();
        let a2 = r.iter().find(|m| m["id"] == "a2").unwrap();
        assert_eq!(a2["parentId"], res_id, "the chain must run through the tool result");
    }

    #[test]
    fn an_error_result_keeps_is_error_and_list_content_keeps_its_text() {
        let raw = concat!(
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":"go"}}"#, "\n",
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"s","message":{"role":"assistant","content":[{"type":"tool_use","id":"t9","name":"Read","input":{"path":"x"}}]}}"#, "\n",
            r#"{"type":"user","uuid":"r1","parentUuid":"a1","sessionId":"s","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t9","is_error":true,"content":[{"type":"text","text":"no such file"}]}]}}"#, "\n",
        );
        let (lines, _) = to_journal_lines(raw).unwrap();
        let r = records(&lines);
        let res = r.iter().find(|m| m["message"]["role"] == "toolResult").unwrap();
        assert_eq!(res["message"]["isError"], true);
        assert_eq!(res["message"]["content"][0]["text"], "no such file");
    }

    #[test]
    fn a_result_with_no_matching_call_is_dropped_and_counted() {
        // Claude rejects a tool_result whose call is missing, so it must not be carried.
        let raw = concat!(
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":"go"}}"#, "\n",
            r#"{"type":"user","uuid":"r1","parentUuid":"u1","sessionId":"s","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ghost","content":"x"}]}}"#, "\n",
        );
        let (lines, stats) = to_journal_lines(raw).unwrap();
        assert!(records(&lines).iter().all(|m| m["message"]["role"] != "toolResult"));
        assert_eq!(stats.orphan_tool_results, 1);
    }

    #[test]
    fn a_duplicate_result_for_the_same_call_is_dropped_and_counted() {
        let raw = concat!(
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":"go"}}"#, "\n",
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"s","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#, "\n",
            r#"{"type":"user","uuid":"r1","parentUuid":"a1","sessionId":"s","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"first"}]}}"#, "\n",
            r#"{"type":"user","uuid":"r2","parentUuid":"r1","sessionId":"s","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"again"}]}}"#, "\n",
        );
        let (lines, stats) = to_journal_lines(raw).unwrap();
        let n = records(&lines).iter().filter(|m| m["message"]["role"] == "toolResult").count();
        assert_eq!(n, 1, "only the first result for a call is kept");
        assert_eq!(stats.duplicate_tool_results, 1);
    }

    #[test]
    fn a_session_of_only_tool_turns_still_converts() {
        let raw = concat!(
            r#"{"type":"assistant","uuid":"a1","parentUuid":null,"sessionId":"s","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#, "\n",
            r#"{"type":"user","uuid":"r1","parentUuid":"a1","sessionId":"s","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#, "\n",
        );
        assert!(to_journal_lines(raw).is_ok(), "tool calls are conversation content");
    }

    #[test]
    fn thinking_is_kept_as_a_marker_and_its_signature_is_never_copied() {
        let raw = concat!(
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":"why"}}"#, "\n",
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"s","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"thinking","thinking":"weigh the options","signature":"SIG-SECRET"},{"type":"text","text":"Because."}]}}"#, "\n",
        );
        let (lines, stats) = to_journal_lines(raw).unwrap();
        let all = lines.join("\n");
        assert!(all.contains("weigh the options"), "thinking words must be kept");
        // The journal is JSON, so the marker's quotes are escaped; check the decoded text.
        let r = records(&lines);
        let first = r[2]["message"]["content"][0]["text"].as_str().unwrap();
        assert!(first.starts_with("<prior_reasoning harness=\"claude\" model=\"claude-opus-4\">"), "{first}");
        assert!(!all.contains("SIG-SECRET"), "signature must never be copied");
        assert_eq!(stats.thinking_kept, 1);
    }

    #[test]
    fn a_base64_image_in_a_user_turn_is_kept_as_an_image_block() {
        // Claude stores a pasted image as a base64 source; it must reach the journal.
        let raw = concat!(
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":[{"type":"text","text":"what is this"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]}}"#, "\n",
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"s","message":{"role":"assistant","content":[{"type":"text","text":"A logo."}]}}"#, "\n",
        );
        let (lines, stats) = to_journal_lines(raw).unwrap();
        let r: Vec<Value> = lines.iter().skip(1).map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(r[0]["message"]["content"][1], json!({"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}));
        assert_eq!(stats.images, 1);
    }

    #[test]
    fn an_image_url_source_is_counted_not_invented() {
        // A URL reference cannot be carried without fetching it; it is counted and skipped.
        let raw = concat!(
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":[{"type":"text","text":"see"},{"type":"image","source":{"type":"url","url":"https://example.com/x.png"}}]}}"#, "\n",
        );
        let (lines, stats) = to_journal_lines(raw).unwrap();
        assert!(!lines.join("\n").contains("example.com"));
        assert_eq!(stats.images, 0);
        assert_eq!(stats.skipped_image_blocks, 1);
    }

    #[test]
    fn redacted_thinking_has_no_text_and_stays_dropped() {
        let raw = concat!(
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":"why"}}"#, "\n",
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"s","message":{"role":"assistant","content":[{"type":"redacted_thinking","data":"ENC"},{"type":"text","text":"ok"}]}}"#, "\n",
        );
        let (lines, stats) = to_journal_lines(raw).unwrap();
        assert!(!lines.join("\n").contains("ENC"));
        assert_eq!(stats.skipped_thinking_blocks, 1);
    }

    #[test]
    fn header_carries_session_and_cwd() {
        let (lines, _) = to_journal_lines(T).unwrap();
        let h: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(h["id"], "sess-1");
        assert_eq!(h["cwd"], "/tmp/c");
    }

    #[test]
    fn meta_and_tag_wrappers_are_not_turns() {
        let raw = concat!(
            r#"{"type":"user","uuid":"m","sessionId":"s","isMeta":true,"message":{"role":"user","content":"meta"}}"#, "\n",
            r#"{"type":"user","uuid":"w","sessionId":"s","message":{"role":"user","content":"<command-name>/x</command-name>"}}"#, "\n",
            r#"{"type":"user","uuid":"r","sessionId":"s","message":{"role":"user","content":"real"}}"#, "\n",
        );
        let (lines, stats) = to_journal_lines(raw).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(stats.messages, 1);
    }

    #[test]
    fn skipped_tool_turn_does_not_break_the_chain() {
        // u1 -> a_tool (tool_use only, skipped) -> u2 must still link u2 to u1.
        let raw = concat!(
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":"first"}}"#, "\n",
            r#"{"type":"assistant","uuid":"a_tool","parentUuid":"u1","sessionId":"s","message":{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"Bash"}]}}"#, "\n",
            r#"{"type":"user","uuid":"u2","parentUuid":"a_tool","sessionId":"s","message":{"role":"user","content":"second"}}"#, "\n",
        );
        let (lines, _) = to_journal_lines(raw).unwrap();
        let u2: Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(u2["parentId"], "u1", "chain must skip the tool-only turn and link to u1");
    }

    #[test]
    fn active_path_is_reachable_through_the_reader() {
        // The converted journal must be walkable by the crate's own reader.
        let raw = concat!(
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":"one"}}"#, "\n",
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"s","message":{"role":"assistant","content":[{"type":"text","text":"two"}]}}"#, "\n",
            r#"{"type":"assistant","uuid":"a_tool","parentUuid":"a1","sessionId":"s","message":{"role":"assistant","content":[{"type":"tool_use","id":"t"}]}}"#, "\n",
            r#"{"type":"user","uuid":"u2","parentUuid":"a_tool","sessionId":"s","message":{"role":"user","content":"three"}}"#, "\n",
        );
        let (lines, _) = to_journal_lines(raw).unwrap();
        let path = std::env::temp_dir().join(format!("hs-fromclaude-{}.jsonl", std::process::id()));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let j = crate::journal::Journal::read(&path).unwrap();
        let active = j.messages();
        let _ = std::fs::remove_file(&path);
        // The tool turn is now kept as a toolCall message, so all four turns are on the path,
        // in source order. (Before tool calls were carried, the tool turn was dropped.)
        let ids: Vec<&str> = active.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["u1", "a1", "a_tool", "u2"], "every kept turn on the active path, in order");
    }

    #[test]
    fn unparseable_lines_are_skipped_and_counted() {
        // Real transcripts carry these: a line truncated by a killed process, or a shape we
        // do not know. The session must still convert; the bad lines are counted.
        let raw = concat!(
            r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-10-01T10:00:00.000Z","message":{"role":"user","content":"hello"}}"#, "\n",
            "{\"type\":\"assis", "\n",
            "[1,2,3]", "\n",
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"s","timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#, "\n",
            "{\"type\":\"user\",\"uuid\":\"u2\"", "\n",
        );
        let (lines, stats) = to_journal_lines(raw).unwrap();
        assert_eq!(lines.len(), 3, "header + two turns");
        assert_eq!(stats.messages, 2);
        assert_eq!(stats.skipped_bad_lines, 3, "truncated line, non-object, truncated tail");
        assert_eq!(stats.skipped_records, 0);
    }

    #[test]
    fn a_transcript_with_no_turns_is_still_an_error() {
        let raw = r#"{"type":"attachment","uuid":"x","sessionId":"s"}"#;
        assert!(to_journal_lines(raw).is_err());
    }
}
