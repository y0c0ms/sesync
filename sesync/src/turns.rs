//! Completed conversation turns: the unit the team compares a conversation by.
//!
//! A journal (the canonical omp/Pi form) is read as a sequence of turns. A turn is one user
//! message and everything the assistant did in answer to it: assistant text, tool calls, tool
//! results. It is COMPLETED when its last visible message is an assistant message with no tool
//! call (the final answer). A prompt without an answer, a reply still running tools, and an
//! aborted reply with no content are not turns yet, so they never count as a change.
//!
//! The turn hash is taken over normalized content only, so the same conversation hashes the same
//! whatever harness wrote the journal it was read from:
//!
//! * kept: the role sequence; text parts of user and assistant messages; tool calls by name and
//!   arguments (a non-object argument is compared as `{"input": v}`, as `sesync-conv` writes it);
//!   tool results by their text (parts joined by a newline).
//! * dropped: ids, parent ids, timestamps, usage, model, stop reasons, thinking blocks and their
//!   signatures, images, and any text that is empty after trimming or starts with `<`. A `<`
//!   prefix is how a Claude transcript marks wrappers and reasoning that `sesync-conv from-claude`
//!   drops or rewrites (`prior_reasoning`), so dropping it here on both sides keeps a copy that
//!   went through Claude equal to the original.
//! * a tool result is kept only when it answers a call seen earlier and not answered yet, which is
//!   the pairing `sesync-conv from-claude` applies.
//! * only the active branch counts: the chain from the last message record in the file, followed
//!   by `parentId`. It is NOT cut at a compaction: the archived turns stay in the file, so a
//!   copy made before the compaction still compares as a prefix.
//!
//! Records that are not messages (model changes, titles, compactions, session info) are ignored.

use serde_json::{Map, Value};

/// One completed turn, with what a reader needs to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// Hash of the normalized content. Equal turns have equal hashes across harnesses.
    pub hash: String,
    /// The prompt: the user message's text parts, joined by a newline.
    pub user: String,
    /// The reply, in order: assistant text, and `[tool: name]` for each call.
    pub reply: String,
    /// When the turn's last message was written, epoch milliseconds; 0 when the journal does not say.
    pub at_ms: i64,
}

/// How a local conversation relates to a team copy, by their turn lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relation {
    /// The same completed turns.
    Identical,
    /// Local has this many more turns than the team copy, which is a prefix of it.
    Ahead(usize),
    /// The team copy has this many more turns, and local is a prefix of it.
    Behind(usize),
    /// Neither list is a prefix of the other. `common` turns agree; the rest differ on each side.
    Diverged { common: usize, local_new: Vec<Turn>, team_new: Vec<Turn> },
}

/// The completed turns of a canonical journal. Errors name the line that is not JSON; the content
/// is never echoed.
pub fn turns_of(journal: &[u8]) -> Result<Vec<Turn>, String> {
    let text = std::str::from_utf8(journal).map_err(|_| "journal is not UTF-8".to_string())?;
    let mut records: Vec<Map<String, Value>> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(t) {
            Ok(Value::Object(m)) => records.push(m),
            Ok(_) => return Err(format!("line {}: not a record", n + 1)),
            Err(_) => return Err(format!("line {}: not JSON", n + 1)),
        }
    }
    let chain = active_chain(&records);
    let mut seen_calls: std::collections::HashSet<String> = Default::default();
    let mut answered: std::collections::HashSet<String> = Default::default();
    let mut msgs: Vec<Msg> = Vec::new();
    for r in chain {
        if r.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        if let Some(m) = normalize(r, &mut seen_calls, &mut answered) {
            msgs.push(m);
        }
    }
    Ok(segment(msgs))
}

/// A fingerprint of a turn list: equal lists have equal fingerprints. Stored in the state file to
/// tell whether the completed turns changed since the last push, without keeping them.
pub fn fingerprint(turns: &[Turn]) -> String {
    let mut all = String::new();
    for t in turns {
        all.push_str(&t.hash);
        all.push('\n');
    }
    crate::mirror::file_hash_bytes(all.as_bytes())
}

/// Compare a local turn list with a team copy's.
pub fn compare(local: &[Turn], team: &[Turn]) -> Relation {
    let common = local.iter().zip(team).take_while(|(a, b)| a.hash == b.hash).count();
    match (local.len() == common, team.len() == common) {
        (true, true) => Relation::Identical,
        (true, false) => Relation::Behind(team.len() - common),
        (false, true) => Relation::Ahead(local.len() - common),
        (false, false) => Relation::Diverged {
            common,
            local_new: local[common..].to_vec(),
            team_new: team[common..].to_vec(),
        },
    }
}

/// The active branch: the chain from the last message record in the file, followed by `parentId`
/// through any record, oldest first. The same walk `sesync-conv` uses, without its compaction cut.
fn active_chain(records: &[Map<String, Value>]) -> Vec<&Map<String, Value>> {
    let by_id: std::collections::HashMap<&str, &Map<String, Value>> = records.iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str).map(|id| (id, r)))
        .collect();
    let leaf = records.iter().rev()
        .find(|r| r.get("type").and_then(Value::as_str) == Some("message"))
        .and_then(|r| r.get("id").and_then(Value::as_str));
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cur = leaf;
    while let Some(id) = cur {
        if !seen.insert(id) {
            break;
        }
        let Some(r) = by_id.get(id) else { break };
        chain.push(*r);
        cur = r.get("parentId").and_then(Value::as_str);
    }
    chain.reverse();
    chain
}

/// One normalized message, before segmentation into turns.
#[derive(Debug, Clone)]
struct Msg {
    role: Role,
    items: Vec<Item>,
    at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
    Result,
}

#[derive(Debug, Clone)]
enum Item {
    Text(String),
    Call { name: String, args: Value },
    Result(String),
}

/// Normalize one message record. `None` when it has no visible content, or when it is a result
/// that does not answer an open call.
fn normalize(r: &Map<String, Value>, seen_calls: &mut std::collections::HashSet<String>,
             answered: &mut std::collections::HashSet<String>) -> Option<Msg> {
    let message = r.get("message")?.as_object()?;
    let at_ms = message.get("timestamp").and_then(Value::as_i64)
        .or_else(|| r.get("timestamp").and_then(Value::as_str).and_then(parse_iso_ms))
        .unwrap_or(0);
    let role = match message.get("role").and_then(Value::as_str)? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "toolResult" => Role::Result,
        _ => return None,
    };
    let blocks: Vec<&Value> = match message.get("content") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v @ Value::String(_)) => vec![v],
        _ => Vec::new(),
    };
    let mut items = Vec::new();
    match role {
        Role::Result => {
            let call_id = message.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string();
            if !seen_calls.contains(&call_id) || !answered.insert(call_id) {
                return None;
            }
            let text = blocks.iter().filter_map(|b| part_text(b)).collect::<Vec<_>>().join("\n");
            items.push(Item::Result(text));
        }
        Role::User | Role::Assistant => {
            for b in blocks {
                match b {
                    Value::String(s) => push_text(&mut items, s),
                    Value::Object(o) => match o.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(s) = o.get("text").and_then(Value::as_str) {
                                push_text(&mut items, s);
                            }
                        }
                        Some("toolCall") if role == Role::Assistant => {
                            let id = o.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                            seen_calls.insert(id);
                            let name = o.get("name").and_then(Value::as_str).unwrap_or("tool").to_string();
                            let args = match o.get("arguments") {
                                None | Some(Value::Null) => Value::Object(Map::new()),
                                Some(v @ Value::Object(_)) => v.clone(),
                                Some(v) => serde_json::json!({"input": v}),
                            };
                            items.push(Item::Call { name, args });
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            if items.is_empty() {
                return None;
            }
        }
    }
    Some(Msg { role, items, at_ms })
}

/// A text part counts when it is not blank and not a wrapper (`<`). See the module note.
fn push_text(items: &mut Vec<Item>, s: &str) {
    let t = s.trim();
    if !t.is_empty() && !t.starts_with('<') {
        items.push(Item::Text(s.to_string()));
    }
}

/// The text of a tool-result part, as `sesync-conv to-claude` reads it: a string, or an object with
/// a `text` string. Any other part (an image) carries none and is left out of the join.
fn part_text(b: &Value) -> Option<String> {
    match b {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("text").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// Group messages into turns. A turn opens at a user message; it is kept when it ends with a
/// final assistant message. A turn that a new prompt interrupts, or that ends mid-run, is dropped.
fn segment(msgs: Vec<Msg>) -> Vec<Turn> {
    let mut out = Vec::new();
    let mut open: Vec<Msg> = Vec::new();
    for m in msgs {
        if m.role == Role::User {
            if let Some(t) = close(std::mem::take(&mut open)) {
                out.push(t);
            }
        }
        // Replies before any prompt belong to no turn.
        if m.role == Role::User || !open.is_empty() {
            open.push(m);
        }
    }
    if let Some(t) = close(open) {
        out.push(t);
    }
    out
}

fn close(turn: Vec<Msg>) -> Option<Turn> {
    let last = turn.last()?;
    let final_answer = last.role == Role::Assistant
        && !last.items.iter().any(|i| matches!(i, Item::Call { .. }));
    if !final_answer {
        return None;
    }
    let mut user = Vec::new();
    let mut reply = Vec::new();
    for m in &turn {
        for item in &m.items {
            match (m.role, item) {
                (Role::User, Item::Text(s)) => user.push(s.clone()),
                (Role::Assistant, Item::Text(s)) => reply.push(s.clone()),
                (Role::Assistant, Item::Call { name, .. }) => reply.push(format!("[tool: {name}]")),
                _ => {}
            }
        }
    }
    let canon: Vec<Value> = turn.iter().map(|m| {
        let items: Vec<Value> = m.items.iter().map(|i| match i {
            Item::Text(s) => serde_json::json!({"text": s}),
            Item::Call { name, args } => serde_json::json!({"tool": name, "args": args}),
            Item::Result(s) => serde_json::json!({"result": s}),
        }).collect();
        serde_json::json!({"role": role_name(m.role), "items": items})
    }).collect();
    let mut buf = String::new();
    canonical(&Value::Array(canon), &mut buf);
    Some(Turn {
        hash: crate::mirror::file_hash_bytes(buf.as_bytes()),
        user: user.join("\n"),
        reply: reply.join("\n"),
        at_ms: turn.last().map_or(0, |m| m.at_ms),
    })
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Result => "result",
    }
}

/// JSON with object keys in sorted order, so equal values always serialize equal.
fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                canonical(&o[*k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.mmm]Z` to epoch milliseconds, or `None`.
fn parse_iso_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, l: usize| s.get(a..a + l)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (num(0, 4)?, num(5, 2)?, num(8, 2)?, num(11, 2)?, num(14, 2)?, num(17, 2)?);
    let ms = if b.len() >= 23 && b[19] == b'.' { num(20, 3).unwrap_or(0) } else { 0 };
    let (y2, m2) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((((days * 24 + h) * 60 + mi) * 60 + sec) * 1000 + ms)
}
