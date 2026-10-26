//! OpenCode export -> omp/Pi journal.
//!
//! Input is the JSON printed by `opencode export <session-id>` (it may start
//! with a banner line; the JSON begins at the first `{`). Output is an omp
//! journal that `Journal::read` and every other converter in this crate
//! accept, so OpenCode becomes one more source for the same canonical form.
//!
//! Mapping:
//!   info.id            -> session header `id`
//!   info.directory     -> session header `cwd`
//!   info.title         -> session header `title` (O-penCode default
//!                         "New session - <ts>" titles are dropped, not kept)
//!   message.role       -> message.role (user | assistant)
//!   text parts         -> text content blocks
//!   file parts         -> image content when inline (data: URL); any other file (path, http URL,
//!                         non-image data) -> a note in its place, counted
//!   tool parts         -> a toolCall block in the assistant message, plus, when the
//!                         tool finished (status completed or error), one toolResult
//!                         message right after the assistant message, in call order.
//!                         A pending or running tool keeps its call and gets no result.
//!                         Shapes and rules from session-migrate (xhluca, MIT).
//!   reasoning parts    -> dropped (the README keeps reasoning for omp/Pi, but
//!                         an OpenCode reasoning block has no omp signature
//!                         and is not needed to resume the conversation)
//!   time.created (ms)  -> record `timestamp` (ISO); never the epoch
//!   summary assistant (summary: true, finished) -> compaction record {summary}; the live window
//!                         starts there. The user message holding the compaction part has no text
//!                         and is dropped with the other empty turns.

use crate::attach;
use serde_json::{json, Map, Value};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub messages: usize,
    pub text_parts: usize,
    pub skipped_tool_parts: usize,
    pub skipped_reasoning_parts: usize,
    pub reasoning_kept: usize,
    pub images: usize,
    pub skipped_file_parts: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub unfinished_tool_parts: usize,
    /// Completed compaction summaries converted to compaction records.
    pub compactions: usize,
}

/// Parse the export text (banner allowed) into a JSON value.
pub fn parse_export(raw: &str) -> Result<Value, String> {
    let start = raw.find('{').ok_or("export has no JSON object")?;
    serde_json::from_str(&raw[start..]).map_err(|e| format!("export is not valid JSON: {e}"))
}

/// Build the omp journal lines (header first, then one record per message).
pub fn to_journal_lines(export: &Value) -> Result<(Vec<String>, Stats), String> {
    let info = export.get("info").and_then(Value::as_object).ok_or("export has no `info`")?;
    let session_id = info.get("id").and_then(Value::as_str).ok_or("export info has no `id`")?;
    let cwd = info.get("directory").and_then(Value::as_str).unwrap_or("");
    let created_ms = info.get("time").and_then(|t| t.get("created")).and_then(Value::as_i64).unwrap_or(0);
    let title = clean_title(info.get("title").and_then(Value::as_str).unwrap_or(""));

    let messages = export.get("messages").and_then(Value::as_array).ok_or("export has no `messages`")?;
    let mut stats = Stats::default();

    let mut header = Map::new();
    header.insert("type".into(), "session".into());
    header.insert("version".into(), 3.into());
    header.insert("id".into(), session_id.into());
    header.insert("timestamp".into(), iso_ms(created_ms).into());
    header.insert("cwd".into(), cwd.into());
    if !title.is_empty() {
        header.insert("title".into(), title.clone().into());
    }
    let mut lines = vec![serde_json::to_string(&header).map_err(|e| e.to_string())?];

    let mut prev: Option<String> = None;
    for (i, m) in messages.iter().enumerate() {
        let minfo = m.get("info").and_then(Value::as_object).ok_or("message has no `info`")?;
        let role = minfo.get("role").and_then(Value::as_str).unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }
        let id = minfo.get("id").and_then(Value::as_str).map(str::to_string)
            .unwrap_or_else(|| format!("oc-{session_id}-{i}"));
        let created = minfo.get("time").and_then(|t| t.get("created")).and_then(Value::as_i64).unwrap_or(created_ms);
        // OpenCode's compaction: a finished assistant message flagged `summary` is the summary of the
        // turns before it. The live window starts there, as OpenCode's own filterCompacted stops
        // there too. It becomes a compaction record; the turns before it stay in the journal.
        if role == "assistant"
            && minfo.get("summary").and_then(Value::as_bool) == Some(true)
            && minfo.get("finish").is_some_and(|f| !f.is_null())
            && minfo.get("error").map_or(true, Value::is_null)
        {
            let summary: Vec<&str> = m
                .get("parts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .filter(|t| !t.trim().is_empty())
                .collect();
            let summary = summary.join("\n");
            if !summary.trim().is_empty() {
                let mut rec = Map::new();
                rec.insert("type".into(), "compaction".into());
                rec.insert("id".into(), id.clone().into());
                rec.insert("parentId".into(), prev.clone().map(Value::from).unwrap_or(Value::Null));
                rec.insert("timestamp".into(), iso_ms(created).into());
                rec.insert("summary".into(), summary.into());
                lines.push(serde_json::to_string(&rec).map_err(|e| e.to_string())?);
                prev = Some(id);
                stats.compactions += 1;
                continue;
            }
        }

        let mut content: Vec<Value> = Vec::new();
        // Tool results found in this message: (call id, tool name, text, is_error).
        let mut results: Vec<(String, String, String, bool)> = Vec::new();
        for p in m.get("parts").and_then(Value::as_array).into_iter().flatten() {
            match p.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = p.get("text").and_then(Value::as_str).unwrap_or("");
                    if !text.trim().is_empty() {
                        content.push(json!({"type": "text", "text": text}));
                        stats.text_parts += 1;
                    }
                }
                Some("reasoning") => {
                    let t = p.get("text").and_then(Value::as_str).unwrap_or("");
                    match crate::prior_reasoning::wrap(t, "opencode", &model_name(minfo)) {
                        Some(w) => {
                            content.push(json!({"type": "text", "text": w}));
                            stats.reasoning_kept += 1;
                        }
                        None => stats.skipped_reasoning_parts += 1,
                    }
                }
                Some("step-start") | Some("step-finish") => {}
                Some("file") => {
                    // An attachment. An inline image (data: URL) is kept. Any other file cannot be
                    // carried: it is counted, and a note takes its place in the message.
                    let mime = p.get("mime").and_then(Value::as_str).unwrap_or("");
                    let url = p.get("url").and_then(Value::as_str).unwrap_or("");
                    let inline = url.strip_prefix("data:").and_then(|rest| rest.split_once(";base64,"));
                    match inline {
                        Some((data_mime, data)) if mime.starts_with("image/") || data_mime.starts_with("image/") => {
                            let m = if mime.is_empty() { data_mime } else { mime };
                            content.push(json!({"type": "image", "data": data, "mimeType": m}));
                            stats.images += 1;
                        }
                        _ => {
                            stats.skipped_file_parts += 1;
                            let filename = p.get("filename").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
                            // The name is the source's own filename only: a path or a URL can hold private
                            // directories or a query, so it is never written.
                            let name = filename.map(str::to_string);
                            let mime = match inline {
                                Some((data_mime, _)) if mime.is_empty() => data_mime,
                                _ => mime,
                            };
                            let mime = if mime.is_empty() { None } else { Some(mime) };
                            let bytes = inline.map(|(_, data)| attach::decoded_len(data));
                            let note = attach::note(name.as_deref().unwrap_or("attachment"), mime, bytes, attach::KEPT);
                            content.push(json!({"type": "text", "text": note}));
                        }
                    }
                }
                Some("tool") if role == "assistant" => {
                    let cid = p.get("callID").and_then(Value::as_str).unwrap_or("");
                    if cid.is_empty() {
                        stats.skipped_tool_parts += 1;
                        continue;
                    }
                    let name = p.get("tool").and_then(Value::as_str).unwrap_or("unknown_tool");
                    let state = p.get("state").and_then(Value::as_object);
                    let mut args = state.and_then(|st| st.get("input")).cloned().unwrap_or_else(|| json!({}));
                    if !args.is_object() {
                        args = json!({"input": args});
                    }
                    content.push(json!({"type": "toolCall", "id": cid, "name": name, "arguments": args}));
                    stats.tool_calls += 1;
                    let status = state.and_then(|st| st.get("status")).and_then(Value::as_str).unwrap_or("");
                    let text_of = |k: &str| -> String {
                        match state.and_then(|st| st.get(k)) {
                            Some(Value::String(t)) => t.clone(),
                            Some(Value::Null) | None => String::new(),
                            Some(other) => other.to_string(),
                        }
                    };
                    match status {
                        "completed" => results.push((cid.into(), name.into(), text_of("output"), false)),
                        "error" => results.push((cid.into(), name.into(), text_of("error"), true)),
                        _ => stats.unfinished_tool_parts += 1,
                    }
                }
                // The compaction part on a user message only marks the summary above; nothing to carry.
                Some("compaction") => {}
                Some(_) => stats.skipped_tool_parts += 1,
                None => {}
            }
        }
        if content.is_empty() {
            continue; // nothing to resume from an empty turn
        }

        let mut msg = Map::new();
        msg.insert("role".into(), role.into());
        msg.insert("content".into(), Value::Array(content));
        // The model that wrote an assistant turn: the window a copy must fit is per model.
        if let (Some(p), true) = (minfo.get("providerID").and_then(Value::as_str), role == "assistant") {
            msg.insert("provider".into(), p.into());
            msg.insert("model".into(), model_name(minfo).into());
        }
        let mut rec = Map::new();
        rec.insert("type".into(), "message".into());
        rec.insert("id".into(), id.clone().into());
        rec.insert("parentId".into(), prev.clone().map(Value::from).unwrap_or(Value::Null));
        rec.insert("timestamp".into(), iso_ms(created).into());
        rec.insert("message".into(), Value::Object(msg));
        lines.push(serde_json::to_string(&rec).map_err(|e| e.to_string())?);
        prev = Some(id.clone());
        stats.messages += 1;
        // One toolResult message per finished tool, chained after the assistant message.
        for (n, (cid, name, text, is_error)) in results.into_iter().enumerate() {
            let rid = format!("{id}-r{n}");
            let mut rm = Map::new();
            rm.insert("role".into(), "toolResult".into());
            rm.insert("toolCallId".into(), cid.into());
            rm.insert("toolName".into(), name.into());
            rm.insert("content".into(), json!([{"type": "text", "text": text}]));
            rm.insert("isError".into(), is_error.into());
            let mut r = Map::new();
            r.insert("type".into(), "message".into());
            r.insert("id".into(), rid.clone().into());
            r.insert("parentId".into(), prev.clone().map(Value::from).unwrap_or(Value::Null));
            r.insert("timestamp".into(), iso_ms(created).into());
            r.insert("message".into(), Value::Object(rm));
            lines.push(serde_json::to_string(&r).map_err(|e| e.to_string())?);
            prev = Some(rid);
            stats.messages += 1;
            stats.tool_results += 1;
        }
    }

    if stats.messages == 0 {
        return Err("export has no user or assistant text to convert".into());
    }
    Ok((lines, stats))
}

/// The model named on an OpenCode message, or "unknown".
fn model_name(minfo: &Map<String, Value>) -> String {
    minfo.get("modelID").and_then(Value::as_str).unwrap_or("unknown").to_string()
}

/// O-penCode names an untitled session "New session - <timestamp>". That is
/// not a title, so it is dropped rather than carried into the mirror name.
fn clean_title(t: &str) -> String {
    let t = t.trim();
    if t.starts_with("New session - ") && t.ends_with('Z') {
        return String::new();
    }
    t.to_string()
}

/// ISO-8601 UTC with milliseconds, from epoch milliseconds.
fn iso_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An export with one tool that completed and one that failed, as OpenCode writes
    /// them: a single `tool` part per call, holding the input and the output or error.
    fn tool_export() -> Value {
        serde_json::from_str(r#"{
          "info": {"id": "ses_t", "directory": "/tmp/p", "title": "Tools", "time": {"created": 1792000000000}},
          "messages": [
            {"info": {"id": "msg_u", "role": "user", "time": {"created": 1792000000001}},
             "parts": [{"type": "text", "text": "list and read"}]},
            {"info": {"id": "msg_a", "role": "assistant", "time": {"created": 1792000000002}},
             "parts": [
               {"type": "step-start"},
               {"type": "text", "text": "Running."},
               {"type": "tool", "callID": "call_ok", "tool": "bash",
                "state": {"status": "completed", "input": {"command": "ls"}, "output": "a.txt\nb.txt",
                          "title": "ls", "metadata": {}, "time": {"start": 1, "end": 2}}},
               {"type": "tool", "callID": "call_bad", "tool": "read",
                "state": {"status": "error", "input": {"filePath": "x"}, "error": "no such file",
                          "time": {"start": 3, "end": 4}}},
               {"type": "step-finish", "reason": "tool-calls"}
             ]},
            {"info": {"id": "msg_b", "role": "assistant", "time": {"created": 1792000000003}},
             "parts": [{"type": "text", "text": "One file is missing."}]}
          ]
        }"#).unwrap()
    }

    fn recs(export: &Value) -> (Vec<Value>, Stats) {
        let (lines, stats) = to_journal_lines(export).unwrap();
        (lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect(), stats)
    }

    #[test]
    fn a_tool_part_becomes_a_tool_call_in_the_assistant_message() {
        let (r, stats) = recs(&tool_export());
        let a = r.iter().find(|m| m["id"] == "msg_a").expect("assistant turn kept");
        let c = a["message"]["content"].as_array().unwrap();
        assert_eq!(c[0]["type"], "text");
        assert_eq!(c[1]["type"], "toolCall");
        assert_eq!(c[1]["id"], "call_ok");
        assert_eq!(c[1]["name"], "bash");
        assert_eq!(c[1]["arguments"]["command"], "ls");
        assert_eq!(c[2]["id"], "call_bad");
        assert_eq!(stats.tool_calls, 2);
        assert_eq!(stats.skipped_tool_parts, 0);
    }

    #[test]
    fn each_finished_tool_gets_a_tool_result_message_after_its_call() {
        let (r, stats) = recs(&tool_export());
        let ids: Vec<&str> = r.iter().skip(1).map(|m| m["id"].as_str().unwrap()).collect();
        let a = ids.iter().position(|i| *i == "msg_a").unwrap();
        let ok = r.iter().find(|m| m["message"]["toolCallId"] == "call_ok").expect("completed result");
        let bad = r.iter().find(|m| m["message"]["toolCallId"] == "call_bad").expect("error result");
        assert_eq!(ok["message"]["role"], "toolResult");
        assert_eq!(ok["message"]["toolName"], "bash");
        assert_eq!(ok["message"]["isError"], false);
        assert_eq!(ok["message"]["content"][0]["text"], "a.txt\nb.txt");
        assert_eq!(bad["message"]["isError"], true);
        assert_eq!(bad["message"]["content"][0]["text"], "no such file");
        assert_eq!(ok["parentId"], "msg_a", "first result follows the assistant message");
        assert_eq!(bad["parentId"], ok["id"], "results are chained in call order");
        assert!(ids.iter().position(|i| *i == ok["id"].as_str().unwrap()).unwrap() > a);
        assert_eq!(stats.tool_results, 2);
    }

    #[test]
    fn the_next_turn_links_to_the_last_tool_result() {
        let (r, _) = recs(&tool_export());
        let bad = r.iter().find(|m| m["message"]["toolCallId"] == "call_bad").unwrap();
        let b = r.iter().find(|m| m["id"] == "msg_b").unwrap();
        assert_eq!(b["parentId"], bad["id"], "the chain must run through the tool results");
    }

    #[test]
    fn a_tool_that_never_finished_keeps_its_call_but_gets_no_result() {
        // A pending or running tool has no output. Its call is kept so the turn reads
        // correctly, and the missing result is counted, not invented.
        let export: Value = serde_json::from_str(r#"{
          "info": {"id": "ses_r", "directory": "/tmp", "time": {"created": 1}},
          "messages": [
            {"info": {"id": "msg_u", "role": "user", "time": {"created": 2}}, "parts": [{"type": "text", "text": "go"}]},
            {"info": {"id": "msg_a", "role": "assistant", "time": {"created": 3}},
             "parts": [{"type": "tool", "callID": "call_run", "tool": "bash",
                        "state": {"status": "running", "input": {"command": "sleep 9"}, "time": {"start": 1}}}]}
          ]
        }"#).unwrap();
        let (r, stats) = recs(&export);
        assert!(r.iter().all(|m| m["message"]["role"] != "toolResult"));
        assert_eq!(stats.tool_calls, 1);
        assert_eq!(stats.unfinished_tool_parts, 1);
    }

    #[test]
    fn an_inline_image_attachment_is_kept_and_a_file_reference_is_counted() {
        let export: Value = serde_json::from_str(r#"{
          "info": {"id": "ses_img", "directory": "/tmp", "time": {"created": 1}},
          "messages": [
            {"info": {"id": "msg_u", "role": "user", "time": {"created": 2}},
             "parts": [{"type": "text", "text": "what is this"},
                       {"type": "file", "mime": "image/png", "url": "data:image/png;base64,iVBORw0KGgo="},
                       {"type": "file", "mime": "text/plain", "url": "file:///home/x/notes.txt"}]}
          ]
        }"#).unwrap();
        let (lines, stats) = to_journal_lines(&export).unwrap();
        let u: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(u["message"]["content"][1], json!({"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}));
        assert!(!lines.join("\n").contains("notes.txt"), "a local file path must not leak into the journal");
        assert_eq!(stats.images, 1);
        assert_eq!(stats.skipped_file_parts, 1);
    }

    #[test]
    fn a_tool_part_without_a_call_id_is_skipped_and_counted() {
        let export: Value = serde_json::from_str(r#"{
          "info": {"id": "ses_n", "directory": "/tmp", "time": {"created": 1}},
          "messages": [
            {"info": {"id": "msg_u", "role": "user", "time": {"created": 2}}, "parts": [{"type": "text", "text": "go"}]},
            {"info": {"id": "msg_a", "role": "assistant", "time": {"created": 3}},
             "parts": [{"type": "text", "text": "ok"},
                       {"type": "tool", "tool": "bash", "state": {"status": "completed", "input": {}, "output": "x"}}]}
          ]
        }"#).unwrap();
        let (r, stats) = recs(&export);
        assert!(r.iter().all(|m| m["message"]["role"] != "toolResult"));
        assert_eq!(stats.tool_calls, 0);
        assert_eq!(stats.skipped_tool_parts, 1);
    }

    fn export(text_user: &str, text_asst: &str) -> Value {
        json!({
            "info": {"id": "ses_test", "directory": "/tmp/x", "title": "New session - 2026-10-14T18:31:12.640Z",
                     "time": {"created": 1791000000000i64}},
            "messages": [
                {"info": {"id": "msg_u", "role": "user", "time": {"created": 1791000000000i64}},
                 "parts": [{"type": "text", "text": text_user}]},
                {"info": {"id": "msg_a", "role": "assistant", "time": {"created": 1791000001000i64}},
                 "parts": [{"type": "step-start"}, {"type": "reasoning", "text": "hmm"},
                           {"type": "text", "text": text_asst}, {"type": "step-finish"}]}
            ]
        })
    }

    #[test]
    fn keeps_prompt_and_answer_in_order() {
        let (lines, stats) = to_journal_lines(&export("ping", "pong")).unwrap();
        assert_eq!(lines.len(), 3, "header + 2 messages");
        let u: Value = serde_json::from_str(&lines[1]).unwrap();
        let a: Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(u["message"]["role"], "user");
        assert_eq!(u["message"]["content"][0]["text"], "ping");
        assert_eq!(a["message"]["role"], "assistant");
        // The reasoning comes first as a marker, then the answer.
        let marker = a["message"]["content"][0]["text"].as_str().unwrap();
        assert!(marker.starts_with("<prior_reasoning harness=\"opencode\""), "{marker}");
        assert!(marker.contains("hmm"));
        assert_eq!(a["message"]["content"][1]["text"], "pong");
        assert_eq!(a["parentId"], u["id"], "answer must link to the prompt");
        assert_eq!(stats.messages, 2);
        assert_eq!(stats.reasoning_kept, 1);
    }

    #[test]
    fn default_title_is_dropped_not_copied() {
        let (lines, _) = to_journal_lines(&export("a", "b")).unwrap();
        let h: Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(h.get("title").is_none(), "date-named default must not become a title");
    }

    #[test]
    fn timestamps_are_real_not_epoch() {
        let (lines, _) = to_journal_lines(&export("a", "b")).unwrap();
        let u: Value = serde_json::from_str(&lines[1]).unwrap();
        assert!(u["timestamp"].as_str().unwrap().starts_with("2026-"), "{}", u["timestamp"]);
    }

    #[test]
    fn banner_before_json_is_accepted() {
        let raw = format!("Exporting session: ses_test\n{}", export("a", "b"));
        assert!(parse_export(&raw).is_ok());
    }

    #[test]
    fn empty_session_is_an_error() {
        let v = json!({"info": {"id": "ses_e", "directory": "/", "time": {"created": 0}}, "messages": []});
        assert!(to_journal_lines(&v).is_err());
    }
}
