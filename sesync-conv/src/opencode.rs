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
            if let Some(p) = &last_id {
                info.insert("parentID".into(), p.clone().into());
            }
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
