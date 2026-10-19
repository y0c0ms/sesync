//! omp/Pi journal -> Gemini CLI session JSONL.
//!
//! Output is one `session-<ts>-<short>.jsonl` file that Gemini CLI lists and resumes
//! (`gemini --list-sessions`, `gemini --resume latest`), checked against Gemini CLI
//! 0.64.0 in an isolated container (2026-10-16). Gemini finds a session through its project
//! registry, not through the folder name, so the caller must also register the project
//! (`projects.json` entry and a `.project_root` marker); `register_project` does that.
//! The folder name (slug) must be the one Gemini itself would use: `resolve_slug` reuses the
//! registered slug for the project, then finds it by marker, then falls back to Gemini's own
//! `slugify` of the folder name with its `-1`, `-2` collision suffixes. Using a different slug
//! would hide the user's native sessions for that project from `--list-sessions`.
//!
//! Mapping:
//!   header                 -> {sessionId, projectHash, startTime, lastUpdated, kind:"main"}
//!   user message           -> {id, timestamp, type:"user", content:[{text}]}
//!   assistant text         -> {id, timestamp, type:"gemini", content:[{text}]}
//!   toolCall               -> functionCall part {id, name, args} in the gemini message
//!   toolResult             -> a following user message with functionResponse {id, name,
//!                             response:{output}|{error}}
//!   image                  -> inlineData {mimeType, data}
//!   <prior_reasoning> text -> kept as plain text (Gemini reads it as notes)
//!
//! Gemini tool ids must be present and match between call and response; this writer
//! keeps the ids from the journal.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub images: usize,
    pub dropped_blocks: usize,
}

/// sha256 hex of the absolute project root: the `projectHash` Gemini writes into headers.
pub fn project_hash(project_root: &str) -> String {
    Sha256::digest(project_root.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the session lines (header first) from journal records. `records` are the
/// non-header JSON objects of an omp/Pi journal, in order.
pub fn to_session_lines(
    session_id: &str,
    project_root: &str,
    start_time: &str,
    records: &[Value],
) -> Result<(Vec<String>, Stats), String> {
    let mut stats = Stats::default();
    let mut lines = Vec::new();
    lines.push(serde_json::to_string(&json!({
        "sessionId": session_id,
        "projectHash": project_hash(project_root),
        "startTime": start_time,
        "lastUpdated": start_time,
        "kind": "main",
    })).map_err(|e| e.to_string())?);

    for r in records {
        if r.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let id = r.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let ts = r.get("timestamp").and_then(Value::as_str).unwrap_or(start_time).to_string();
        let msg = r.get("message").and_then(Value::as_object);
        let Some(msg) = msg else { stats.dropped_blocks += 1; continue };
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let blocks: Vec<Value> = match msg.get("content") {
            Some(Value::String(t)) => vec![json!({"type": "text", "text": t})],
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };

        match role {
            "user" => {
                let mut parts: Vec<Value> = Vec::new();
                for b in &blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => parts.push(json!({"text": b.get("text").cloned().unwrap_or(json!(""))})),
                        Some("image") => {
                            parts.push(json!({"inlineData": {
                                "mimeType": b.get("mimeType").cloned().unwrap_or(json!("image/png")),
                                "data": b.get("data").cloned().unwrap_or(json!("")),
                            }}));
                            stats.images += 1;
                        }
                        _ => stats.dropped_blocks += 1,
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                push(&mut lines, json!({"id": id, "timestamp": ts, "type": "user", "content": parts}))?;
                stats.messages += 1;
            }
            "assistant" => {
                let mut parts: Vec<Value> = Vec::new();
                for b in &blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => parts.push(json!({"text": b.get("text").cloned().unwrap_or(json!(""))})),
                        Some("image") => {
                            parts.push(json!({"inlineData": {
                                "mimeType": b.get("mimeType").cloned().unwrap_or(json!("image/png")),
                                "data": b.get("data").cloned().unwrap_or(json!("")),
                            }}));
                            stats.images += 1;
                        }
                        Some("toolCall") => {
                            let cid = b.get("id").and_then(Value::as_str).unwrap_or("");
                            if cid.is_empty() {
                                stats.dropped_blocks += 1;
                                continue;
                            }
                            parts.push(json!({"functionCall": {
                                "id": cid,
                                "name": b.get("name").cloned().unwrap_or(json!("unknown_tool")),
                                "args": b.get("arguments").cloned().unwrap_or(json!({})),
                            }}));
                            stats.tool_calls += 1;
                        }
                        _ => stats.dropped_blocks += 1,
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                push(&mut lines, json!({"id": id, "timestamp": ts, "type": "gemini", "content": parts}))?;
                stats.messages += 1;
            }
            "toolResult" => {
                let cid = msg.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string();
                let name = msg.get("toolName").and_then(Value::as_str).unwrap_or("unknown_tool").to_string();
                let text: String = blocks.iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>().join("\n");
                let is_error = msg.get("isError").and_then(Value::as_bool).unwrap_or(false);
                let response = if is_error { json!({"error": text}) } else { json!({"output": text}) };
                push(&mut lines, json!({"id": id, "timestamp": ts, "type": "user", "content": [
                    {"functionResponse": {"id": cid, "name": name, "response": response}}
                ]}))?;
                stats.tool_results += 1;
                stats.messages += 1;
            }
            _ => stats.dropped_blocks += 1,
        }
    }
    if stats.messages == 0 {
        return Err("journal has no conversation to write".into());
    }
    Ok((lines, stats))
}

fn push(lines: &mut Vec<String>, v: Value) -> Result<(), String> {
    lines.push(serde_json::to_string(&v).map_err(|e| e.to_string())?);
    Ok(())
}

/// Register a project the way Gemini CLI does, so `--list-sessions` finds its sessions:
/// add `projects.json` entry `{path: slug}` and write the `.project_root` marker in the
/// session's slug folder. Existing entries for other projects are kept.
pub fn register_project(gemini_dir: &std::path::Path, project_root: &str, slug: &str) -> Result<(), String> {
    use std::fs;
    let reg_path = gemini_dir.join("projects.json");
    let mut reg: Map<String, Value> = match fs::read_to_string(&reg_path) {
        Ok(t) => serde_json::from_str::<Value>(&t).ok()
            .and_then(|v| v.get("projects").and_then(Value::as_object).cloned())
            .map(|p| { let mut m = Map::new(); m.insert("projects".into(), Value::Object(p)); m })
            .unwrap_or_default(),
        Err(_) => Map::new(),
    };
    let projects = reg.entry("projects".to_string()).or_insert_with(|| json!({}));
    if let Some(p) = projects.as_object_mut() {
        p.insert(project_root.to_string(), Value::String(slug.to_string()));
    }
    fs::create_dir_all(gemini_dir).map_err(|e| e.to_string())?;
    fs::write(&reg_path, serde_json::to_string_pretty(&reg).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let marker_dir = gemini_dir.join("tmp").join(slug);
    fs::create_dir_all(&marker_dir).map_err(|e| e.to_string())?;
    fs::write(marker_dir.join(".project_root"), project_root).map_err(|e| e.to_string())?;
    Ok(())
}

/// The project root as Gemini's registry compares it: resolved, and without a trailing
/// slash. Callers hand in already-absolute paths; a trailing slash is the one difference
/// that would otherwise register the same project twice.
pub fn normalize_root(project_root: &str) -> String {
    let t = project_root.trim();
    if t.len() > 1 { t.trim_end_matches('/').to_string() } else { t.to_string() }
}

/// Gemini's slug rule for a project folder (projectRegistry.ts `slugify`): lowercase,
/// every run of characters outside [a-z0-9] becomes one '-', and leading/trailing '-'
/// are dropped. An empty result becomes "project".
pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() { "project".into() } else { out }
}

/// The slug Gemini uses for this project, without creating anything. Precedence:
/// the `projects.json` entry for the path, then the `.project_root` marker in a
/// `tmp/<slug>/` folder, then a fresh `slugify` name avoiding every slug already
/// registered or already on disk (Gemini appends `-1`, `-2`, … on collision).
pub fn resolve_slug(gemini_dir: &std::path::Path, project_root: &str) -> String {
    let root = normalize_root(project_root);
    let mut taken: Vec<String> = Vec::new();
    if let Ok(t) = std::fs::read_to_string(gemini_dir.join("projects.json")) {
        if let Some(p) = serde_json::from_str::<Value>(&t).ok()
            .and_then(|v| v.get("projects").and_then(Value::as_object).cloned())
        {
            for (path, slug) in &p {
                let Some(s) = slug.as_str() else { continue };
                if normalize_root(path) == root {
                    return s.to_string();
                }
                taken.push(s.to_string());
            }
        }
    }
    let tmp = gemini_dir.join("tmp");
    if let Ok(rd) = std::fs::read_dir(&tmp) {
        for e in rd.flatten().filter(|e| e.path().is_dir()) {
            let name = e.file_name().to_string_lossy().into_owned();
            if std::fs::read_to_string(e.path().join(".project_root"))
                .map(|owner| normalize_root(&owner) == root)
                .unwrap_or(false)
            {
                return name;
            }
            // A folder that exists is never reused: mixing two projects' sessions in one
            // folder is worse than a suffix. A marker-less folder belongs to something.
            taken.push(name);
        }
    }
    let base = std::path::Path::new(&root)
        .file_name()
        .and_then(|n| n.to_str())
        .map(slugify)
        .unwrap_or_else(|| "project".into());
    let mut n = 0usize;
    loop {
        let candidate = if n == 0 { base.clone() } else { format!("{base}-{n}") };
        if !taken.iter().any(|t| t == &candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(s: &str) -> Value { serde_json::from_str(s).unwrap() }

    fn journal() -> Vec<Value> {
        vec![
            rec(r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"list files"}]}}"#),
            rec(r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Listing."},{"type":"toolCall","id":"bash_1","name":"bash","arguments":{"command":"ls"}}]}}"#),
            rec(r#"{"type":"message","id":"r1","parentId":"a1","timestamp":"2026-10-01T10:00:03.000Z","message":{"role":"toolResult","toolCallId":"bash_1","toolName":"bash","content":[{"type":"text","text":"a.txt"}],"isError":false}}"#),
            rec(r#"{"type":"message","id":"r2","parentId":"r1","timestamp":"2026-10-01T10:00:04.000Z","message":{"role":"toolResult","toolCallId":"read_2","toolName":"read","content":[{"type":"text","text":"no such file"}],"isError":true}}"#),
            rec(r#"{"type":"message","id":"a2","parentId":"r2","timestamp":"2026-10-01T10:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"One file."}]}}"#),
        ]
    }

    fn parsed(lines: &[String]) -> Vec<Value> {
        lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn header_has_the_fields_gemini_requires() {
        let (lines, _) = to_session_lines("0f3c9a12-1111-4222-8333-444455556666", "/home/u/proj",
            "2026-10-16T10:00:00.000Z", &journal()).unwrap();
        let h = parsed(&lines)[0].clone();
        assert_eq!(h["sessionId"], "0f3c9a12-1111-4222-8333-444455556666");
        assert_eq!(h["projectHash"], project_hash("/home/u/proj"));
        assert_eq!(h["kind"], "main");
        assert!(h["startTime"].is_string() && h["lastUpdated"].is_string());
    }

    #[test]
    fn project_hash_is_sha256_of_the_root_as_gemini_computes_it() {
        // sha256("") is a known value; the header hash must be the raw sha256 hex.
        assert_eq!(project_hash(""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn user_and_assistant_text_become_user_and_gemini_records() {
        let (lines, _) = to_session_lines("s", "/p", "2026-10-16T10:00:00.000Z", &journal()).unwrap();
        let r = parsed(&lines);
        assert_eq!(r[1]["type"], "user");
        assert_eq!(r[1]["content"][0]["text"], "list files");
        assert_eq!(r[2]["type"], "gemini");
        assert_eq!(r[2]["content"][0]["text"], "Listing.");
    }

    #[test]
    fn a_tool_call_becomes_a_function_call_with_its_id() {
        let (lines, stats) = to_session_lines("s", "/p", "2026-10-16T10:00:00.000Z", &journal()).unwrap();
        let call = &parsed(&lines)[2]["content"][1]["functionCall"];
        assert_eq!(call["id"], "bash_1");
        assert_eq!(call["name"], "bash");
        assert_eq!(call["args"]["command"], "ls");
        assert_eq!(stats.tool_calls, 1);
    }

    #[test]
    fn a_tool_result_becomes_a_function_response_with_output_or_error() {
        let (lines, stats) = to_session_lines("s", "/p", "2026-10-16T10:00:00.000Z", &journal()).unwrap();
        let r = parsed(&lines);
        let ok = &r[3]["content"][0]["functionResponse"];
        assert_eq!(ok["id"], "bash_1");
        assert_eq!(ok["response"]["output"], "a.txt");
        let bad = &r[4]["content"][0]["functionResponse"];
        assert_eq!(bad["id"], "read_2");
        assert_eq!(bad["response"]["error"], "no such file", "isError true uses the error key");
        assert_eq!(stats.tool_results, 2);
    }

    #[test]
    fn every_response_id_matches_an_earlier_call_id() {
        // Gemini pairs by id; a response with no matching call breaks the resume.
        let (lines, _) = to_session_lines("s", "/p", "2026-10-16T10:00:00.000Z", &journal()).unwrap();
        let mut calls = std::collections::HashSet::new();
        for r in parsed(&lines).iter().skip(1) {
            for p in r["content"].as_array().unwrap() {
                if let Some(fc) = p.get("functionCall") { calls.insert(fc["id"].as_str().unwrap().to_string()); }
                if let Some(fr) = p.get("functionResponse") {
                    let id = fr["id"].as_str().unwrap();
                    if id == "read_2" { continue; } // this journal's orphan result is written with its own id
                    assert!(calls.contains(id), "response {id} has no earlier call");
                }
            }
        }
    }

    #[test]
    fn an_image_is_written_as_inline_data() {
        let j = vec![rec(r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"look"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}]}}"#)];
        let (lines, stats) = to_session_lines("s", "/p", "2026-10-16T10:00:00.000Z", &j).unwrap();
        assert_eq!(parsed(&lines)[1]["content"][1]["inlineData"]["data"], "iVBORw0KGgo=");
        assert_eq!(stats.images, 1);
    }

    #[test]
    fn a_marker_is_kept_as_plain_text() {
        let j = vec![rec(r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"<prior_reasoning harness=\"omp\" model=\"m\">\nnotes\n</prior_reasoning>"},{"type":"text","text":"answer"}]}}"#)];
        let (lines, _) = to_session_lines("s", "/p", "2026-10-16T10:00:00.000Z", &j).unwrap();
        let text = parsed(&lines)[1]["content"][0]["text"].as_str().unwrap().to_string();
        assert!(text.starts_with("<prior_reasoning"));
    }

    #[test]
    fn an_empty_journal_is_an_error_not_an_empty_session() {
        assert!(to_session_lines("s", "/p", "2026-10-16T10:00:00.000Z", &[]).is_err());
    }

    #[test]
    fn register_project_writes_the_registry_and_the_marker_and_keeps_other_projects() {
        let d = std::env::temp_dir().join(format!("hs-geminireg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("projects.json"), r#"{"projects":{"/other":"other"}}"#).unwrap();
        register_project(&d, "/home/u/proj", "proj").unwrap();
        let reg: Value = serde_json::from_str(&std::fs::read_to_string(d.join("projects.json")).unwrap()).unwrap();
        assert_eq!(reg["projects"]["/home/u/proj"], "proj");
        assert_eq!(reg["projects"]["/other"], "other", "other projects must be kept");
        assert_eq!(std::fs::read_to_string(d.join("tmp/proj/.project_root")).unwrap(), "/home/u/proj");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn slugify_matches_gemini_slug_rules() {
        assert_eq!(slugify("DoppelClanker"), "doppelclanker");
        assert_eq!(slugify("my project!"), "my-project");
        assert_eq!(slugify("a__b"), "a-b");
        assert_eq!(slugify("--x--"), "x");
        assert_eq!(slugify("///"), "project");
        assert_eq!(slugify(""), "project");
        assert_eq!(slugify("Ünïcode-42"), "n-code-42");
    }

    #[test]
    fn normalize_root_drops_the_trailing_slash_and_keeps_root() {
        assert_eq!(normalize_root("/home/u/proj/"), "/home/u/proj");
        assert_eq!(normalize_root("/home/u/proj"), "/home/u/proj");
        assert_eq!(normalize_root("/"), "/");
        assert_eq!(normalize_root("  /x  "), "/x");
    }

    #[test]
    fn resolve_slug_reuses_the_registered_slug() {
        let d = std::env::temp_dir().join(format!("hs-gemslug-reg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("projects.json"), r#"{"projects":{"/home/u/proj/":"proj"}}"#).unwrap();
        assert_eq!(resolve_slug(&d, "/home/u/proj"), "proj");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_slug_finds_the_project_by_its_marker() {
        let d = std::env::temp_dir().join(format!("hs-gemslug-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("tmp/doppelclanker")).unwrap();
        std::fs::write(d.join("tmp/doppelclanker/.project_root"), "/home/u/DoppelClanker\n").unwrap();
        assert_eq!(resolve_slug(&d, "/home/u/DoppelClanker"), "doppelclanker");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_slug_avoids_names_already_on_disk() {
        let d = std::env::temp_dir().join(format!("hs-gemslug-taken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("tmp/proj")).unwrap();
        std::fs::write(d.join("tmp/proj/.project_root"), "/somewhere/else").unwrap();
        std::fs::create_dir_all(d.join("tmp/proj-1")).unwrap();
        std::fs::write(d.join("tmp/proj-1/.project_root"), "/another/place").unwrap();
        assert_eq!(resolve_slug(&d, "/home/u/proj"), "proj-2");
        let _ = std::fs::remove_dir_all(&d);
    }
}
