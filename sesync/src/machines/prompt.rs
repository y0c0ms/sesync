//! `sesync machine prompt` and `sesync machine read`: another machine's agents, from this one. A prompt goes into
//! the conversation's pane as a prompt turn (`terminal.send` with `enter`, the phone composer's call), so the
//! other machine claims the conversation in sesync-lock for the turn as it does for the phone. `read` prints the
//! conversation's last messages from its transcript (`nativeChat.readSession`).
//!
//! A conversation not open on that machine is resumed there first only with `open`, the phone's Resume: the other
//! bridge opens it as "Open in herdr" does, under its own session id, and refuses to start a second agent on a
//! conversation already running there. With `wait`, the prompt returns once the agent has answered and stopped
//! working, and prints the answer.
//!
//! The other machine must have granted this one (the phone app's "can see" switch, `machines.grant`): the same
//! pairing `sesync machine attach` uses.

use super::attach::pick;
use super::client::{self, Reader, Writer};
use super::store::Machine;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// How often `wait` looks at the conversation again.
const POLL_EVERY: Duration = Duration::from_secs(2);
/// The most messages one `read` asks for.
pub const MOST_MESSAGES: usize = 200;

/// Where a prompt went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub title: String,
    pub worktree: String,
    pub terminal: String,
    /// The conversation was not open, and the other machine resumed it for this prompt.
    pub resumed: bool,
    /// The transcript offset of the newest answer before the prompt: `settle` waits for a newer one.
    pub before: Option<u64>,
}

/// What `wait` saw once the prompt's turn was over.
#[derive(Debug, Clone, PartialEq)]
pub enum Settled {
    /// The agent answered and stopped working; the newest assistant message.
    Answered(Value),
    /// The agent stopped on a question in its pane (a permission or a choice): it is answered there.
    Blocked,
    /// No answer within the time given.
    TimedOut,
}

/// Send `text` as a prompt to the conversation `query` names on `machine`. With `open`, a conversation that is
/// not open there is resumed first. Returns where the prompt went, and the connection for `settle`.
pub async fn send(machine: &Machine, query: &str, text: &str, open: bool) -> Result<(Sent, Writer, Reader), String> {
    if text.trim().is_empty() {
        return Err("the prompt is empty".into());
    }
    let (mut writer, mut reader) = client::connect(machine).await?;
    let list = client::call(&mut writer, &mut reader, "worktree.ps", json!({})).await?;
    let rows = list["worktrees"].as_array().cloned().unwrap_or_default();
    let row = pick(&rows, query)?;
    let title = row["displayName"].as_str().unwrap_or(query).to_string();
    let worktree = row["worktreeId"].as_str().unwrap_or("").to_string();
    let row = row.clone();
    // A journal that cannot be read yet has no answer to compare with.
    let before = messages(&mut writer, &mut reader, &row, 50).await.ok().and_then(|list| newest_answer(&list).map(|(offset, _)| offset));
    let (terminal, resumed) = match row["agents"][0]["paneKey"].as_str() {
        Some(handle) => (handle.to_string(), false),
        None if !open => {
            return Err(format!(
                "{title} is not open on {}; add --open to resume it there (it is refused if it runs there already)",
                machine.name
            ))
        }
        None => {
            let opened = client::call(
                &mut writer,
                &mut reader,
                "session.tabs.createTerminal",
                json!({ "worktree": format!("id:{worktree}") }),
            )
            .await
            .map_err(|e| format!("{title} is not open on {} and could not be resumed: {e}", machine.name))?;
            let terminal = opened["tab"]["terminal"].as_str().ok_or("the machine resumed the conversation but named no terminal")?;
            (terminal.to_string(), true)
        }
    };
    client::call(&mut writer, &mut reader, "terminal.send", json!({ "terminal": terminal, "text": text, "enter": true }))
        .await
        .map_err(|e| format!("{title} on {} did not take the prompt: {e}", machine.name))?;
    Ok((Sent { title, worktree, terminal, resumed, before }, writer, reader))
}

/// The journal session id of the conversation in `row`, the one `nativeChat.readSession` reads by. A bridge
/// that lists it gives it in the row; an older one only in an open pane's tab row.
async fn session_of(writer: &mut Writer, reader: &mut Reader, row: &Value) -> Result<String, String> {
    if let Some(id) = row["sessionId"].as_str().filter(|s| !s.is_empty()) {
        return Ok(id.to_string());
    }
    let worktree = row["worktreeId"].as_str().unwrap_or("");
    let tabs = client::call(writer, reader, "session.tabs.list", json!({ "worktree": format!("id:{worktree}") })).await?;
    tabs["tabs"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|tab| tab["agentStatus"]["providerSession"]["id"].as_str().filter(|s| !s.is_empty()))
        .map(str::to_string)
        .ok_or_else(|| "that machine's sesync is too old to name a closed conversation's session; update it, or open the conversation there".into())
}

/// The conversation `worktree` among `worktree.ps` rows.
async fn row_of(writer: &mut Writer, reader: &mut Reader, worktree: &str) -> Result<Value, String> {
    let list = client::call(writer, reader, "worktree.ps", json!({})).await?;
    list["worktrees"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["worktreeId"] == worktree)
        .cloned()
        .ok_or_else(|| "the conversation is no longer listed on that machine".into())
}

/// The last `limit` messages of the conversation in `row`, oldest first.
async fn messages(writer: &mut Writer, reader: &mut Reader, row: &Value, limit: usize) -> Result<Vec<Value>, String> {
    let session = session_of(writer, reader, row).await?;
    let agent = row["conversationAgent"].as_str().unwrap_or("");
    let page = client::call(
        writer,
        reader,
        "nativeChat.readSession",
        json!({ "agent": agent, "sessionId": session, "limit": limit.clamp(1, MOST_MESSAGES) }),
    )
    .await?;
    if let Some(error) = page["error"].as_str() {
        return Err(error.to_string());
    }
    Ok(page["messages"].as_array().cloned().unwrap_or_default())
}

/// The transcript offset of the newest assistant message among `messages`, if any.
fn newest_answer(messages: &[Value]) -> Option<(u64, &Value)> {
    messages
        .iter()
        .filter(|m| m["role"] == "assistant" && !text_of(m).is_empty())
        .filter_map(|m| m["transcriptOffset"].as_u64().map(|offset| (offset, m)))
        .max_by_key(|(offset, _)| *offset)
}

/// Wait until the conversation `sent` names has an answer newer than the one it had before the prompt and its
/// agent has stopped working, or until `limit` passes.
pub async fn settle(writer: &mut Writer, reader: &mut Reader, sent: &Sent, limit: Duration) -> Result<Settled, String> {
    let start = Instant::now();
    loop {
        let row = row_of(writer, reader, &sent.worktree).await?;
        let state = row["agents"][0]["state"].as_str().unwrap_or("closed");
        if state == "blocked" {
            return Ok(Settled::Blocked);
        }
        if state != "working" {
            if let Ok(list) = messages(writer, reader, &row, 50).await {
                if let Some((offset, answer)) = newest_answer(&list) {
                    if sent.before.map_or(true, |b| offset > b) {
                        return Ok(Settled::Answered(answer.clone()));
                    }
                }
            }
        }
        if start.elapsed() >= limit {
            return Ok(Settled::TimedOut);
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// The last `count` messages of the conversation `query` names on `machine`, oldest first.
pub async fn read(machine: &Machine, query: &str, count: usize) -> Result<(String, Vec<Value>), String> {
    let (mut writer, mut reader) = client::connect(machine).await?;
    let list = client::call(&mut writer, &mut reader, "worktree.ps", json!({})).await?;
    let rows = list["worktrees"].as_array().cloned().unwrap_or_default();
    let row = pick(&rows, query)?.clone();
    let title = row["displayName"].as_str().unwrap_or(query).to_string();
    let list = messages(&mut writer, &mut reader, &row, count).await?;
    Ok((title, list))
}

/// A message's text: its text blocks, joined.
pub fn text_of(message: &Value) -> String {
    message["blocks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// One message as `read` prints it: its role, then its text, with each tool call named on its own line.
pub fn render(message: &Value) -> String {
    let role = message["role"].as_str().unwrap_or("?");
    let mut lines = vec![format!("[{role}]")];
    for block in message["blocks"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => lines.extend(block["text"].as_str().map(str::to_string)),
            Some("tool-call") => lines.push(format!("(tool call: {})", block["name"].as_str().unwrap_or("?"))),
            _ => {}
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, offset: u64, blocks: Value) -> Value {
        json!({ "role": role, "blocks": blocks, "transcriptOffset": offset })
    }

    #[test]
    fn the_newest_answer_is_the_last_assistant_message_with_text() {
        let list = vec![
            message("user", 10, json!([{ "type": "text", "text": "q" }])),
            message("assistant", 20, json!([{ "type": "text", "text": "first" }])),
            message("assistant", 30, json!([{ "type": "tool-call", "name": "read", "input": "" }])),
            message("user", 40, json!([{ "type": "text", "text": "q2" }])),
        ];
        let (offset, answer) = newest_answer(&list).unwrap();
        assert_eq!(offset, 20, "a message with only a tool call is not an answer");
        assert_eq!(text_of(answer), "first");
        assert!(newest_answer(&list[..1]).is_none());
    }

    #[test]
    fn a_message_renders_its_role_its_text_and_its_tool_calls() {
        let m = message(
            "assistant",
            1,
            json!([
                { "type": "text", "text": "looking" },
                { "type": "tool-call", "name": "bash", "input": "ls" },
                { "type": "tool-result", "output": "secret output" },
                { "type": "text", "text": "done" },
            ]),
        );
        assert_eq!(render(&m), "[assistant]\nlooking\n(tool call: bash)\ndone");
    }
}
