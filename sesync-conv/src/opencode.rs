//! Journal -> OpenCode import bundle.
//!
//! The bundle is the exact JSON `opencode export` prints and `opencode import`
//! consumes: `{ info: Session, messages: [{ info: Message, parts: [Part] }] }`.
//! Shapes below were copied from a native 1.18.16 export on 2026-10-12.
//! `opencode import` updates the session row in place by `info.id`, but message
//! and part inserts are conflict-ignore (upstream `cli/cmd/import.ts`), so a
//! re-import alone does NOT replace the conversation: the caller must delete the
//! session first. Deterministic ids make that delete-then-import replace the
//! same mirror every time instead of leaving duplicates.
//!
//! Mapping:
//!   journal user msg        -> message{role:user} + text parts (+ file parts for images)
//!   journal assistant msg   -> message{role:assistant} + step-start, reasoning (thinking),
//!                              text, tool parts (state filled from the matching
//!                              toolResult), step-finish
//!   journal toolResult msg  -> folded into the tool part of the assistant message
//!                              that issued the call (by toolCallId)
//!   compaction record       -> part{type:compaction} on a synthetic assistant message
//! Dropped: system/developer/bashExecution/custom records (not conversation).
//!
//! Thinking IS carried as `reasoning` parts: OpenCode stores them natively.

use crate::journal::{Journal, Msg};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const B62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// OpenCode ids: prefix + 12 hex chars (ms time) + 14 base62 chars.
/// Deterministic from (seed, counter) so a re-run yields identical ids and
/// `import` updates in place.
fn oc_id(prefix: &str, seed: &str, time_ms: i64, n: u64) -> String {
    let h = Sha256::digest(format!("{seed}|{n}").as_bytes());
    let tail: String = h.iter().take(14).map(|b| B62[(*b as usize) % 62] as char).collect();
    format!("{prefix}_{:012x}{tail}", time_ms.max(0) as u64 & 0xffff_ffff_ffff)
}

pub fn session_id_for(seed: &str) -> String {
    let h = Sha256::digest(format!("{seed}|opencode").as_bytes());
    let hexs = hex::encode(&h[..6]);
    let tail: String = h.iter().skip(6).take(14).map(|b| B62[(*b as usize) % 62] as char).collect();
    format!("ses_{hexs}{tail}")
}

pub struct Options<'a> {
    pub session_id: &'a str,
    pub title: &'a str,
    pub directory: &'a str,
    pub cli_version: &'a str,
    pub include_thinking: bool,
}

pub fn to_bundle(j: &Journal, o: &Options) -> Value {
    let msgs = j.messages();
    let seed = o.session_id;
    let mut n: u64 = 0;
    let mut next = |prefix: &str, t: i64| {
        n += 1;
        oc_id(prefix, seed, t, n)
    };

    // tool results by call id, so the assistant's tool part can be completed
    let mut results: HashMap<&str, &Msg> = HashMap::new();
    for m in &msgs {
        if m.role == "toolResult" {
            if let Some(cid) = m.message.get("toolCallId").and_then(Value::as_str) {
                results.insert(cid, m);
            }
        }
    }

    let mut out_msgs: Vec<Value> = Vec::new();
    let mut last_id: Option<String> = None;
    let mut model = (String::from("unknown"), String::from("unknown"));
    let mut first_ts = i64::MAX;
    let mut last_ts = 0i64;

    // The compaction's summary is the only record of the turns it archived, and those turns are
    // no longer exported. Without it a compacted journal would simply start mid-thread. (The
    // module doc has always claimed this mapping; the code never had it, which only became
    // visible once the archive stopped being exported.)
    //
    // It goes out as a *user* message, the shape the first message of a conversation already has
    // and that `opencode import` accepts: an assistant record here needs a `parentID`, and a
    // parentless one makes the whole import fail with "Missing key at [parentID]" - which creates
    // the session row and then leaves it empty, the exact symptom seen on a real mirror.
    if let (Some(note), Some((_, ts))) = (j.compaction_note(), j.live_compaction()) {
        // OpenCode orders a session by its time-derived message ids, and a compaction is usually
        // recorded after the turns it keeps: stamped with its own time, the summary of what came
        // before showed up as the LAST message. It belongs first, just before the oldest kept one.
        let compacted = crate::journal::parse_iso_ms(ts).unwrap_or(0);
        let t = match msgs.iter().map(|m| m.timestamp_ms).min() {
            Some(first_kept) => compacted.min(first_kept - 1),
            None => compacted,
        };
        first_ts = first_ts.min(t);
        last_ts = last_ts.max(t);
        let mid = next("msg", t);
        let mut part = Map::new();
        part.insert("type".into(), "text".into());
        part.insert("text".into(), note.into());
        part.insert("id".into(), next("prt", t).into());
        part.insert("sessionID".into(), o.session_id.into());
        part.insert("messageID".into(), mid.clone().into());
        let mut info = Map::new();
        info.insert("id".into(), mid.clone().into());
        info.insert("sessionID".into(), o.session_id.into());
        info.insert("role".into(), "user".into());
        info.insert("time".into(), json!({"created": t}));
        info.insert("agent".into(), "build".into());
        info.insert("model".into(), json!({"providerID": model.0, "modelID": model.1}));
        out_msgs.push(json!({"info": Value::Object(info), "parts": [Value::Object(part)]}));
        last_id = Some(mid);
    }

    for m in &msgs {
        match m.role {
            "user" | "assistant" => {}
            _ => continue, // toolResult folded; system/developer/bashExecution skipped
        }
        let t = m.timestamp_ms;
        first_ts = first_ts.min(t);
        last_ts = last_ts.max(t);
        let mid = next("msg", t);
        let mut parts: Vec<Value> = Vec::new();
        let part = |typ: &str, extra: Map<String, Value>, id: String| {
            let mut p = Map::new();
            p.insert("type".into(), typ.into());
            for (k, v) in extra {
                p.insert(k, v);
            }
            p.insert("id".into(), id.into());
            p.insert("sessionID".into(), o.session_id.into());
            p.insert("messageID".into(), mid.clone().into());
            Value::Object(p)
        };

        if m.role == "assistant" {
            if let (Some(p), Some(mo)) = (m.message.get("provider").and_then(Value::as_str), m.message.get("model").and_then(Value::as_str)) {
                model = (p.to_string(), mo.to_string());
            }
            parts.push(part("step-start", Map::new(), next("prt", t)));
        }

        let mut tool_calls = 0;
        for b in &m.content {
            match b {
                Value::String(s) => parts.push(part("text", map(json!({"text": s})), next("prt", t))),
                Value::Object(ob) => match ob.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let txt = ob.get("text").and_then(Value::as_str).unwrap_or("");
                        if !txt.is_empty() {
                            parts.push(part("text", map(json!({"text": txt})), next("prt", t)));
                        }
                    }
                    Some("thinking") if o.include_thinking => {
                        let txt = ob.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if !txt.is_empty() {
                            parts.push(part("reasoning", map(json!({"text": txt, "time": {"start": t, "end": t}})), next("prt", t)));
                        }
                    }
                    Some("thinking") => {}
                    Some("image") => {
                        let mime = ob.get("mimeType").and_then(Value::as_str).unwrap_or("image/png");
                        let data = ob.get("data").and_then(Value::as_str).unwrap_or("");
                        if !data.is_empty() && !data.starts_with("blob:") {
                            parts.push(part("file", map(json!({"mime": mime, "url": format!("data:{mime};base64,{data}")})), next("prt", t)));
                        }
                    }
                    Some("toolCall") => {
                        tool_calls += 1;
                        let cid = ob.get("id").and_then(Value::as_str).unwrap_or("");
                        let name = ob.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let input = ob.get("arguments").cloned().unwrap_or(Value::Object(Map::new()));
                        let (status, output, is_err, end) = match results.get(cid) {
                            Some(r) => {
                                let txt = r.content.iter().filter_map(|c| match c {
                                    Value::String(s) => Some(s.as_str()),
                                    Value::Object(x) => x.get("text").and_then(Value::as_str),
                                    _ => None,
                                }).collect::<Vec<_>>().join("\n");
                                let is_err = r.message.get("isError").and_then(Value::as_bool).unwrap_or(false);
                                (if is_err { "error" } else { "completed" }, txt, is_err, r.timestamp_ms)
                            }
                            None => ("error", String::from("(no result recorded)"), true, t),
                        };
                        let mut state = map(json!({"status": status, "input": input, "time": {"start": t, "end": end}}));
                        if is_err {
                            state.insert("error".into(), output.clone().into());
                        } else {
                            state.insert("output".into(), output.into());
                            state.insert("title".into(), name.into());
                            state.insert("metadata".into(), Value::Object(Map::new()));
                        }
                        parts.push(part("tool", map(json!({"tool": name, "callID": cid, "state": Value::Object(state)})), next("prt", t)));
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        let mut info = Map::new();
        info.insert("id".into(), mid.clone().into());
        info.insert("sessionID".into(), o.session_id.into());
        info.insert("role".into(), m.role.into());
        if m.role == "user" {
            info.insert("time".into(), json!({"created": t}));
            info.insert("agent".into(), "build".into());
            info.insert("model".into(), json!({"providerID": model.0, "modelID": model.1}));
        } else {
            let finish = if tool_calls > 0 { "tool-calls" } else { "stop" };
            parts.push(part("step-finish", map(json!({"reason": finish, "tokens": {"total":0,"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}, "cost": 0})), next("prt", t)));
            info.insert("time".into(), json!({"created": t, "completed": t}));
            // OpenCode's importer requires a STRING parentID on every assistant message: an absent
            // key fails with "Missing key at [parentID]", null with "Expected string, got null",
            // and any string not starting with "msg" is refused too. A conversation that opens
            // with an assistant turn (a converted transcript that starts with the model, or a
            // live window cut there) has no predecessor, so it points at a message-shaped id
            // derived from the session. The field lives inside the message data, not in a
            // foreign key, so a sentinel is enough to open the tree.
            info.insert(
                "parentID".into(),
                match &last_id {
                    Some(p) => p.clone().into(),
                    // Must look like a message id ("Expected a string starting with \"msg\""),
                    // so the session id with its prefix swapped keeps it unique per session.
                    None => o.session_id.replacen("ses_", "msg_", 1).into(),
                },
            );
            info.insert("modelID".into(), model.1.clone().into());
            info.insert("providerID".into(), model.0.clone().into());
            info.insert("mode".into(), "build".into());
            info.insert("agent".into(), "build".into());
            info.insert("path".into(), json!({"cwd": o.directory, "root": o.directory}));
            info.insert("cost".into(), 0.into());
            info.insert("tokens".into(), json!({"total":0,"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}));
            info.insert("finish".into(), finish.into());
        }
        last_id = Some(mid);
        out_msgs.push(json!({"info": Value::Object(info), "parts": parts}));
    }

    if first_ts == i64::MAX {
        first_ts = 0;
    }
    // Stored slug prefix: written to disk and matched on later runs, so it keeps its value.
    let slug = format!("harness-sync-{}", &hex::encode(Sha256::digest(seed.as_bytes()))[..12]);
    let path = o.directory.replace('\\', "/").trim_start_matches(|c: char| c.is_ascii_alphabetic() || c == ':' || c == '/').to_string();
    json!({
        "info": {
            "id": o.session_id, "slug": slug, "projectID": "global",
            "directory": o.directory.replace('\\', "/"), "path": path,
            "title": o.title, "version": o.cli_version,
            "time": {"created": first_ts, "updated": last_ts.max(first_ts)},
            "summary": {"additions": 0, "deletions": 0, "files": 0},
            "cost": 0, "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}}
        },
        "messages": out_msgs
    })
}

fn map(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod untimed_tests {
    use super::*;
    use crate::journal::Journal;

    #[test]
    fn untimed_journal_never_produces_the_epoch() {
        let path = std::env::temp_dir().join(format!("hs-untimed-{}.jsonl", std::process::id()));
        std::fs::write(&path, concat!(
            r#"{"type":"session","version":3,"id":"s","cwd":"/t","title":"x"}"#, "\n",
            r#"{"type":"message","id":"m1","parentId":null,"message":{"role":"user","content":"p"}}"#, "\n",
            r#"{"type":"message","id":"m2","parentId":"m1","message":{"role":"assistant","content":[{"type":"text","text":"a"}]}}"#, "\n",
        )).unwrap();
        let j = Journal::read(&path).unwrap();
        let v = to_bundle(&j, &Options { session_id: "ses_untimedtest000000000000", title: "x", directory: "/t", cli_version: "1", include_thinking: false });
        let _ = std::fs::remove_file(&path);
        let created = v["info"]["time"]["created"].as_i64().unwrap();
        assert!(created > 1_700_000_000_000, "epoch leaked into the O-penCode session time: {created}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::fixture::*;

    /// Same compacted journal as every other target's test: the live window is exported and the
    /// summary that replaced the archive is carried (here as a compaction part).
    #[test]
    fn a_compacted_journal_exports_only_the_live_window() {
        let j = compacted_journal("opencode");
        let b = to_bundle(&j, &Options {
            session_id: "ses_000000000000aaaaaaaaaaaaaa",
            title: "t",
            directory: "/tmp/c",
            cli_version: "1.19.0",
            include_thinking: true,
        });
        let text = serde_json::to_string(&b).unwrap();
        for kept in [KEPT_ONE, KEPT_TWO, NEWEST] {
            assert!(text.contains(kept), "the live window is exported: {kept}");
        }
        for archived in [ARCHIVED_ONE, ARCHIVED_TWO] {
            assert!(!text.contains(archived), "turns archived by the compaction must not be exported: {archived}");
        }
        assert!(text.contains(SUMMARY), "the compaction summary stands in for them");

        // `opencode import` rejects an assistant message that has no `parentID` ("Missing key at
        // [parentID]") and the whole import then leaves the session EMPTY - which is what emptied
        // a real mirror. Pin the requirement, and the shape the summary goes out as.
        let msgs = b["messages"].as_array().expect("messages");
        let first = &msgs[0];
        assert_eq!(first["info"]["role"], "user", "the summary opens the conversation as a user message");
        assert!(
            first["parts"][0]["text"].as_str().unwrap_or("").contains(SUMMARY),
            "the summary text is the first part"
        );
        // OpenCode shows a session in id and time order, not in bundle order. The compaction is
        // recorded after the turns it keeps, and stamped with that time the summary showed up as
        // the last message of the imported session.
        let note_id = first["info"]["id"].as_str().unwrap();
        let note_at = first["info"]["time"]["created"].as_i64().unwrap();
        for m in &msgs[1..] {
            assert!(m["info"]["id"].as_str().unwrap() > note_id, "the summary's id sorts first: {}", m["info"]);
            assert!(m["info"]["time"]["created"].as_i64().unwrap() > note_at, "the summary is the oldest message");
        }
        for m in msgs {
            let info = &m["info"];
            if info["role"] == "assistant" {
                assert!(info.get("parentID").is_some(), "every assistant message needs a parentID: {info}");
            }
        }
    }

    /// A conversation whose first turn is an assistant message still needs a well-formed parent:
    /// the importer rejects an absent key ("Missing key at [parentID]"), a null one ("Expected
    /// string, got null") and any string not starting with "msg". On 2026-10-21 this failed every
    /// pass, and each failed import had already deleted the session it was meant to replace.
    #[test]
    fn an_assistant_first_conversation_gets_a_message_shaped_parent() {
        let dir = std::env::temp_dir().join(format!("sesync-conv-assist-first-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("j.jsonl");
        std::fs::write(&path, concat!(
            "{\"type\":\"session\",\"id\":\"s1\",\"timestamp\":\"2026-01-01T00:00:00.000Z\"}\n",
            "{\"type\":\"message\",\"id\":\"a1\",\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"primeiro\"}]}}\n",
            "{\"type\":\"message\",\"id\":\"u1\",\"parentId\":\"a1\",\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"resposta\"}]}}\n",
        )).unwrap();
        let j = Journal::read(&path).unwrap();
        let b = to_bundle(&j, &Options {
            session_id: "ses_abc0000000000000000000000",
            title: "t",
            directory: "/tmp/c",
            cli_version: "1.19.0",
            include_thinking: true,
        });
        let first = &b["messages"][0]["info"];
        assert_eq!(first["role"], "assistant", "the conversation still opens with the assistant turn");
        let parent = first["parentID"].as_str().expect("a string parentID, not null");
        assert!(parent.starts_with("msg"), "the importer requires a msg-prefixed id, got {parent}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
