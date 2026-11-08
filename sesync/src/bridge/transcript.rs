//! The phone's chat view of a conversation: `nativeChat.readSession` (one page of the live window) and
//! `nativeChat.subscribe` (that window, kept current).
//!
//! The source is the parent session's journal (`Conversation::key`). omp and Pi journals are the file itself
//! (`Harness::native_journal`). Every other harness is converted to the canonical journal by its own converter
//! (`sesync-conv from-claude`; OpenCode's export through `sesync-conv from-opencode`), and the chat reads the
//! converted copy. Copies live under the bridge's state directory, one per source state (see
//! `converted_journal`), so a read converts only when the source changed. A harness whose source cannot be
//! stamped is refused with `unavailable`.
//!
//! The window is the live one: the active branch cut at the last compaction, as `sesync-conv` reads it
//! (`Journal::live_records`). Rows are decoded the way the desktop host decodes omp
//! (`src/main/native-chat/transcript-line-decoders-omp.ts`), with the mobile clips of
//! `src/main/runtime/rpc/methods/native-chat-rpc-block-sanitize.ts`. Images are labels only: the phone
//! shows a placeholder, never bytes.
//!
//! Wire: the phone's `sessionId` is `session_id(conv)`, the parent's identifier (`Conversation::idents[0]`).
//! Shapes follow the app's wire contract ("App wire requirements for phase 2",
//! items 2-4), `mobile/src/session/use-mobile-native-chat-session.ts` and `src/shared/native-chat-types.ts`.

use super::catalog::{self, Catalog, Conversation};
use super::rpc::{Ctx, Failure};
use super::stream::{self, Conn, Sink};
use crate::harness::{self, journal::record_time};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Rows per read when the phone names no `limit`: the desktop host's mobile default.
const DEFAULT_WINDOW: u64 = 40;
/// The most rows one read returns (`MOBILE_NATIVE_CHAT_MAX_WINDOW`).
const MAX_WINDOW: u64 = 2000;
/// Text blocks keep this many characters (`MOBILE_TEXT_BLOCK_CHAR_CAP`).
const TEXT_CAP: usize = 64_000;
/// Tool output and tool arguments keep this many characters (`MOBILE_BLOCK_CHAR_CAP`).
const TOOL_CAP: usize = 4_000;
const CLIP_MARK: &str = "\n… (truncated)";
/// The line a reply the agent aborted shows (`NATIVE_CHAT_INTERRUPTED_STATUS_TEXT`).
const INTERRUPTED: &str = "Conversation interrupted";
/// How often a subscribed chat's source is checked.
const POLL: Duration = Duration::from_secs(1);
/// The longest a converted copy waits for its source to hold still before it is rebuilt anyway.
const MAX_STALE: Duration = Duration::from_secs(5);

/// The phone's `sessionId` for a conversation: the parent session's identifier.
pub fn session_id(conv: &Conversation) -> String {
    conv.idents.first().cloned().unwrap_or_default()
}

/// `nativeChat.readSession`: one page of the live window, older than `beforeOffset` when given.
/// A conversation the bridge does not know is answered in-band as `{error}`, as the app's reader expects.
pub fn read_session(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let conversation = match resolve(ctx, params) {
        Ok(conversation) => conversation,
        Err((code, message)) if code == "worktree_not_found" => return Ok(json!({ "error": message })),
        Err(failure) => return Err(failure),
    };
    let stamp = stamp_of(ctx.catalog.as_ref(), &conversation);
    let path = journal_for(ctx.catalog.as_ref(), &ctx.dir, &conversation, stamp.as_deref()).map_err(|m| ("unavailable", m))?;
    page(&path, page_limit(params), params["beforeOffset"].as_u64())
        .map(page_json)
        .map_err(|m| ("unavailable", m))
}

/// `nativeChat.subscribe`: a `snapshot` of the live window, then `appended` or `replacement` frames as the
/// journal changes. Problems with the conversation or its journal are sent as an `error` frame on the
/// stream, so the view shows them instead of loading forever.
pub fn subscribe(params: &Value, ctx: &Ctx, conn: &Conn, id: &Value) -> Result<(), Failure> {
    let session = session_param(params)?;
    let conversation = match resolve(ctx, params) {
        Ok(conversation) => conversation,
        Err((_, message)) => {
            conn.sink.json(&stream::streamed(id, json!({ "type": "error", "message": message })));
            return Ok(());
        }
    };
    // Stamp before reading, so a write that lands during the read is caught by the next poll.
    let stamp = stamp_of(ctx.catalog.as_ref(), &conversation);
    let path = match journal_for(ctx.catalog.as_ref(), &ctx.dir, &conversation, stamp.as_deref()) {
        Ok(path) => path,
        Err(message) => {
            conn.sink.json(&stream::streamed(id, json!({ "type": "error", "message": message })));
            return Ok(());
        }
    };
    // The cleanup token the phone unsubscribes by. Legacy callers send none, so it is `agent:sessionId`.
    let key = match params["subscriptionId"].as_str().filter(|k| !k.is_empty()) {
        Some(token) => token.to_string(),
        None => format!("{}:{session}", params["agent"].as_str().unwrap_or("")),
    };
    let limit = page_limit(params);
    let pending_ok = params["capabilities"]["transcriptPending"].as_u64() == Some(1);
    let (first, shown, failed) = first_frame(&path, limit, pending_ok);
    let stop = conn.register(key, id.clone());
    let _ = conn.sink.json(&stream::streamed(id, first));
    let native = native_journal(&conversation);
    let feed = Feed {
        sink: conn.sink.clone(),
        request_id: id.clone(),
        catalog: Arc::clone(&ctx.catalog),
        dir: ctx.dir.clone(),
        conversation,
        native,
        limit,
        stop,
        stamp,
        pending: None,
        converted_at: Instant::now(),
        shown,
        failed,
    };
    thread::spawn(move || run_feed(feed));
    Ok(())
}

/// The conversation a phone chat names: the one whose parent identifier is `session`, else the one whose
/// parent journal is `transcript`.
fn pick(list: Vec<Conversation>, session: &str, transcript: Option<&str>) -> Option<Conversation> {
    let index = list
        .iter()
        .position(|c| session_id(c) == session)
        .or_else(|| transcript.and_then(|path| list.iter().position(|c| c.key == path)))?;
    list.into_iter().nth(index)
}

fn resolve(ctx: &Ctx, params: &Value) -> Result<Conversation, Failure> {
    let session = session_param(params)?;
    let transcript = params["transcriptPath"].as_str().filter(|p| !p.is_empty());
    let list = ctx.catalog.conversations().map_err(|m| ("unavailable", m))?;
    pick(list, session, transcript)
        .ok_or_else(|| ("worktree_not_found", format!("no conversation runs session {session}")))
}

fn session_param(params: &Value) -> Result<&str, Failure> {
    params["sessionId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ("bad_request", "sessionId is required".to_string()))
}

/// Whether a conversation's parent file is already the canonical journal (omp and Pi).
fn native_journal(conv: &Conversation) -> bool {
    harness::get(&conv.harness).is_some_and(|h| h.native_journal())
}

/// The stamp of a conversation's source as it stands: for omp and Pi, the file's size and change time; for
/// every other harness, what the catalog reads (`Catalog::source_stamp`). None when the source is gone.
fn stamp_of(catalog: &dyn Catalog, conv: &Conversation) -> Option<String> {
    if native_journal(conv) {
        return harness::stat(Path::new(&conv.key)).map(|(len, nanos)| format!("{len}:{nanos}"));
    }
    catalog.source_stamp(conv)
}

/// The journal a chat reads for the source as `stamp` describes it: the parent's own file for omp and Pi,
/// else the converted copy of that source state.
fn journal_for(catalog: &dyn Catalog, dir: &Path, conv: &Conversation, stamp: Option<&str>) -> Result<PathBuf, String> {
    if native_journal(conv) {
        return Ok(PathBuf::from(&conv.key));
    }
    let stamp = stamp.ok_or_else(|| format!("{} chats cannot be read: their source is not available", conv.harness))?;
    converted_journal(catalog, dir, conv, stamp)
}

/// Serialises the converters the bridge runs: each takes seconds on a long session, and two at once would only
/// compete for the same disk and CPU.
static CONVERTING: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// The converted journal of a conversation for the source state `stamp`, kept under `<dir>/transcripts/`. A copy
/// made from that state is reused; otherwise the converter runs and its copy replaces the older copies of the
/// conversation. The copy is renamed into place, so a reader never sees half of one, and it is owner-only.
fn converted_journal(catalog: &dyn Catalog, dir: &Path, conv: &Conversation, stamp: &str) -> Result<PathBuf, String> {
    let _converting = CONVERTING.lock();
    let transcripts = dir.join("transcripts");
    let stem = catalog::conversation_id(&conv.harness, &conv.key);
    let digest: String = Sha256::digest(stamp.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let file = transcripts.join(format!("{stem}-{digest}.jsonl"));
    if file.is_file() {
        return Ok(file);
    }
    super::ensure_owner_dir(&transcripts)?;
    let scratch = transcripts.join(format!("{stem}.scratch"));
    let _ = fs::remove_dir_all(&scratch);
    super::ensure_owner_dir(&scratch)?;
    let placed = catalog
        .canonical_journal(conv, &scratch)
        .and_then(|journal| fs::rename(&journal, &file).map_err(|e| format!("cannot place the {} journal: {e}", conv.harness)))
        .and_then(|()| super::restrict(&file, 0o600));
    let _ = fs::remove_dir_all(&scratch);
    placed?;
    remove_older_copies(&transcripts, &stem, &file);
    Ok(file)
}

/// Removes the copies of one conversation that were made from other source states.
fn remove_older_copies(transcripts: &Path, stem: &str, keep: &Path) {
    let Ok(entries) = fs::read_dir(transcripts) else { return };
    let prefix = format!("{stem}-");
    for entry in entries.flatten() {
        let path = entry.path();
        let copy = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".jsonl"));
        if copy && path != keep {
            let _ = fs::remove_file(&path);
        }
    }
}

fn page_limit(params: &Value) -> usize {
    params["limit"].as_u64().unwrap_or(DEFAULT_WINDOW).clamp(1, MAX_WINDOW) as usize
}

/// One page of the live window.
struct Page {
    rows: Vec<Value>,
    has_more: bool,
    /// The cursor of the oldest row shown: the next page is the rows older than it.
    before: u64,
}

fn page_json(page: Page) -> Value {
    json!({ "messages": page.rows, "hasMore": page.has_more, "beforeOffset": page.before })
}

/// The `limit` rows of the live window older than `before` (the newest rows when `before` is None).
fn page(path: &Path, limit: usize, before: Option<u64>) -> Result<Page, String> {
    let older: Vec<(u64, Value)> = live_rows(path)?
        .into_iter()
        .filter(|(offset, _)| before.map_or(true, |b| *offset < b))
        .collect();
    let split = older.len().saturating_sub(limit);
    let shown = &older[split..];
    let cursor = shown.first().map(|(offset, _)| *offset).or(before).unwrap_or(0);
    Ok(Page {
        rows: shown.iter().map(|(_, row)| row.clone()).collect(),
        has_more: split > 0,
        before: cursor,
    })
}

/// The first frame of a subscription: the window, a `pending` snapshot when the client accepts pending
/// journals and the file is not there yet, or an `error` frame. Returns the frame, the rows the client now
/// shows, and whether the feed is in its failed state.
fn first_frame(path: &Path, limit: usize, pending_ok: bool) -> (Value, Vec<Value>, bool) {
    match page(path, limit, None) {
        Ok(p) => {
            let frame = json!({ "type": "snapshot", "messages": p.rows.clone(), "hasMore": p.has_more, "beforeOffset": p.before });
            (frame, p.rows, false)
        }
        Err(_) if pending_ok && harness::stat(path).is_none() => (
            json!({ "type": "snapshot", "messages": [], "hasMore": false, "pending": true }),
            Vec::new(),
            true,
        ),
        Err(message) => (json!({ "type": "error", "message": message }), Vec::new(), true),
    }
}

/// Whether a source's stamp moved since the last read. A source that appears or vanishes moves it too.
pub fn stamp_changed(last: &Option<String>, now: &Option<String>) -> bool {
    last != now
}

/// The frame that takes a client showing `old` to `new`. `appended` carries the rows that are new or changed
/// when `old`'s last row is still in the window (the tail moved on). Otherwise the whole window is sent as
/// `replacement`. None when no row changed.
pub fn delta(old: &[Value], new: &[Value], has_more: bool, before: u64) -> Option<Value> {
    if old == new {
        return None;
    }
    let anchored = old.last().and_then(row_id).is_some_and(|id| new.iter().any(|r| row_id(r) == Some(id)));
    if anchored {
        let changed: Vec<&Value> = new.iter().filter(|r| !old.contains(r)).collect();
        if changed.is_empty() {
            return None;
        }
        return Some(json!({ "type": "appended", "messages": changed }));
    }
    Some(json!({ "type": "replacement", "messages": new, "hasMore": has_more, "beforeOffset": before }))
}

fn row_id(row: &Value) -> Option<&str> {
    row.get("id")?.as_str()
}

/// A subscribed chat: sends the frames that take the client's window from one state of its source to the next.
struct Feed {
    sink: Sink,
    request_id: Value,
    catalog: Arc<dyn Catalog>,
    /// The bridge's state directory, where converted copies are kept.
    dir: PathBuf,
    conversation: Conversation,
    /// Whether the parent file is read as it is (omp, Pi), so a change is applied on the next poll.
    native: bool,
    limit: usize,
    stop: Arc<AtomicBool>,
    /// The source's stamp when the rows the client holds were read.
    stamp: Option<String>,
    /// The stamp the last poll saw, while a change is still settling (see `run_feed`).
    pending: Option<String>,
    /// When the window was last rebuilt from the source.
    converted_at: Instant,
    /// The rows the client holds.
    shown: Vec<Value>,
    /// Whether the last read failed, so the client holds no window to extend.
    failed: bool,
}

fn run_feed(mut f: Feed) {
    loop {
        thread::sleep(POLL);
        if f.stop.load(Ordering::Relaxed) {
            break;
        }
        let now = stamp_of(f.catalog.as_ref(), &f.conversation);
        if !stamp_changed(&f.stamp, &now) {
            f.pending = None;
            continue;
        }
        // A converted copy is rebuilt once its source has held still for a poll, so a source written in bursts is
        // converted once per burst; a source that never holds still is rebuilt every `MAX_STALE` regardless.
        let settled = f.pending == now;
        f.pending = now.clone();
        if !f.native && !settled && f.converted_at.elapsed() < MAX_STALE {
            continue;
        }
        f.stamp = now.clone();
        f.converted_at = Instant::now();
        let path = journal_for(f.catalog.as_ref(), &f.dir, &f.conversation, now.as_deref());
        let frame = match path.and_then(|path| page(&path, f.limit, None)) {
            Ok(p) => {
                let frame = if f.failed {
                    Some(json!({ "type": "replacement", "messages": p.rows.clone(), "hasMore": p.has_more, "beforeOffset": p.before }))
                } else {
                    delta(&f.shown, &p.rows, p.has_more, p.before)
                };
                f.shown = p.rows;
                f.failed = false;
                frame
            }
            Err(message) => {
                if f.failed {
                    None
                } else {
                    f.shown.clear();
                    f.failed = true;
                    Some(json!({ "type": "error", "message": message }))
                }
            }
        };
        if let Some(frame) = frame {
            if !f.sink.json(&stream::streamed(&f.request_id, frame)) {
                return;
            }
        }
    }
    f.sink.json(&stream::end(&f.request_id));
}

/// The rows of a journal's live window, oldest first, each with its cursor: the record's position in the
/// file. Lines that do not parse are skipped, so a record still being written is not counted until it is
/// complete. The position of a complete record never changes as the file grows.
fn live_rows(path: &Path) -> Result<Vec<(u64, Value)>, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let records: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(Value::is_object)
        .collect();
    let by_id: HashMap<&str, usize> = records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| Some((r.get("id")?.as_str()?, i)))
        .collect();
    // The active leaf is the last message record: the engines append in order and a branch switch re-roots later records.
    let leaf = records
        .iter()
        .rev()
        .find(|r| kind(r) == Some("message"))
        .and_then(|r| r.get("id")?.as_str());
    let mut chain: Vec<usize> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut cur = leaf;
    while let Some(id) = cur {
        if !seen.insert(id) {
            break;
        }
        let Some(&i) = by_id.get(id) else { break };
        chain.push(i);
        cur = records[i].get("parentId").and_then(Value::as_str);
    }
    chain.reverse();
    // The live window starts at the first kept entry of the last compaction; the compaction itself stays in it.
    if let Some(cut) = chain.iter().rposition(|&i| kind(&records[i]) == Some("compaction")) {
        let kept = records[chain[cut]].get("firstKeptEntryId").and_then(Value::as_str);
        let start = kept
            .and_then(|k| chain[..cut].iter().rposition(|&i| records[i].get("id").and_then(Value::as_str) == Some(k)))
            .unwrap_or(cut);
        chain.drain(..start);
    }
    Ok(chain
        .into_iter()
        .filter_map(|i| Some((i as u64, row_of(&records[i], i as u64)?)))
        .collect())
}

fn kind(record: &Value) -> Option<&str> {
    record.get("type")?.as_str()
}

fn is_true(v: Option<&Value>) -> bool {
    v.and_then(Value::as_bool) == Some(true)
}

/// One record as a row, or None for a record the phone does not show.
fn row_of(r: &Value, offset: u64) -> Option<Value> {
    let id = r.get("id")?.as_str()?;
    let at = record_time(r)
        .map(|secs| (secs * 1000.0).round() as i64)
        .or_else(|| r.get("message")?.get("timestamp")?.as_i64());
    match kind(r)? {
        "compaction" => Some(message(
            id,
            "system",
            vec![json!({ "type": "text", "text": "Context compacted", "presentation": "compaction" })],
            at,
            offset,
        )),
        // Custom rows show only when marked for display; the rest are hidden extension state.
        "custom_message" => {
            let blocks = if is_true(r.get("display")) { content_blocks(r.get("content")) } else { Vec::new() };
            (!blocks.is_empty()).then(|| message(id, "system", blocks, at, offset))
        }
        "message" => message_row(r.get("message")?, id, at, offset),
        _ => None,
    }
}

fn message_row(m: &Value, id: &str, at: Option<i64>, offset: u64) -> Option<Value> {
    let role = m.get("role")?.as_str()?;
    if role == "toolResult" {
        return Some(message(id, "tool", tool_result_blocks(m), at, offset));
    }
    if role == "bashExecution" || role == "pythonExecution" {
        return Some(message(id, "tool", execution_blocks(role == "bashExecution", id, m), at, offset));
    }
    if role == "fileMention" {
        // Mentioned paths only: the auto-read file contents would bury the conversation.
        let lines: Vec<String> = m
            .get("files")
            .and_then(Value::as_array)?
            .iter()
            .filter_map(|f| f.get("path")?.as_str())
            .map(|p| format!("@{p}"))
            .collect();
        return (!lines.is_empty())
            .then(|| message(id, "system", vec![json!({ "type": "text", "text": lines.join("\n") })], at, offset));
    }
    if (role == "custom" || role == "hookMessage") && !is_true(m.get("display")) {
        return None;
    }
    let mut blocks = content_blocks(m.get("content"));
    let failure = (role == "assistant").then(|| failure_text(m)).flatten();
    if blocks.is_empty() {
        // An aborted reply with no content still shows that it was interrupted, and a failed one why it failed.
        let aborted = role == "assistant" && m.get("stopReason").and_then(Value::as_str) == Some("aborted");
        let line = failure.or_else(|| aborted.then(|| INTERRUPTED.to_string()))?;
        return Some(message(id, "system", vec![json!({ "type": "text", "text": line })], at, offset));
    }
    let row_role = match role {
        "user" => "user",
        "assistant" => "assistant",
        _ => "system",
    };
    if let Some(line) = failure {
        blocks.push(json!({ "type": "text", "text": line }));
    }
    let mut row = message(id, row_role, blocks, at, offset);
    if row_role == "assistant" {
        serving(m, &mut row);
    }
    Some(row)
}

/// What a reply that ended in a provider error shows in the chat: the provider's own message when the error
/// carries one (`{"error":{"message":…}}` after an HTTP status), else the error as recorded, with the model that
/// failed. Without it a turn the provider refused (an expired login, an empty balance) shows nothing in the chat,
/// and the phone cannot tell a dead session from a slow one. None for any other reply.
fn failure_text(m: &Value) -> Option<String> {
    if m.get("stopReason").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let raw = m.get("errorMessage").and_then(Value::as_str).unwrap_or("").trim();
    let status = raw.split_whitespace().next().filter(|s| s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit()));
    // The first JSON value after the status: omp appends lines after it (a `raw-http-request=` log path).
    let provider_message = raw
        .find('{')
        .and_then(|at| serde_json::Deserializer::from_str(&raw[at..]).into_iter::<Value>().next()?.ok())
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string));
    let reason = match (provider_message, status) {
        (Some(text), Some(code)) => format!("{text} (HTTP {code})"),
        (Some(text), None) => text,
        (None, _) if raw.is_empty() => "no reason given".to_string(),
        (None, _) => clip(raw, 500),
    };
    let model = [m.get("provider"), m.get("model")]
        .iter()
        .filter_map(|v| v.and_then(Value::as_str).filter(|s| !s.is_empty()))
        .collect::<Vec<_>>()
        .join("/");
    Some(if model.is_empty() { format!("Model call failed: {reason}") } else { format!("Model call failed ({model}): {reason}") })
}

fn message(id: &str, role: &str, blocks: Vec<Value>, at: Option<i64>, offset: u64) -> Value {
    json!({
        "id": id,
        "role": role,
        "blocks": blocks,
        "timestamp": at,
        "source": "transcript",
        "transcriptOffset": offset,
    })
}

/// The model, provider and token accounting of an assistant reply. A failed or aborted reply carries no usage.
fn serving(m: &Value, row: &mut Value) {
    for key in ["model", "provider"] {
        if let Some(name) = m.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            row[key] = json!(name);
        }
    }
    let failed = matches!(m.get("stopReason").and_then(Value::as_str), Some("aborted" | "error"));
    if let Some(usage) = m.get("usage").filter(|u| !failed && u.is_object()) {
        row["usage"] = json!({
            "inputTokens": usage.get("input").and_then(Value::as_u64).unwrap_or(0),
            "cacheCreationInputTokens": usage.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0),
            "cacheReadInputTokens": usage.get("cacheRead").and_then(Value::as_u64).unwrap_or(0),
            "outputTokens": usage.get("output").and_then(Value::as_u64).unwrap_or(0),
        });
    }
}

fn tool_result_blocks(m: &Value) -> Vec<Value> {
    let mut result = json!({ "type": "tool-result", "output": clip(&tool_output(m.get("content")), TOOL_CAP) });
    if is_true(m.get("isError")) {
        result["isError"] = json!(true);
    }
    // The call this result answers, by the provider's id.
    if let Some(call_id) = m.get("toolCallId").and_then(Value::as_str) {
        result["callId"] = json!(call_id);
    }
    let mut blocks = vec![result];
    blocks.extend(image_blocks(m.get("content")));
    blocks
}

/// A bash or python execution record as a call and its result, linked by the record's id.
fn execution_blocks(bash: bool, id: &str, m: &Value) -> Vec<Value> {
    let (source_key, name) = if bash { ("command", "bash") } else { ("code", "python") };
    let input = m.get(source_key).and_then(Value::as_str).unwrap_or("");
    let output = m.get("output").and_then(Value::as_str).unwrap_or("");
    let failed = is_true(m.get("cancelled")) || m.get("exitCode").and_then(Value::as_f64).is_some_and(|c| c != 0.0);
    let mut result = json!({ "type": "tool-result", "output": clip(output, TOOL_CAP), "callId": id });
    if failed {
        result["isError"] = json!(true);
    }
    vec![
        json!({ "type": "tool-call", "name": name, "input": clip(input, TOOL_CAP), "callId": id }),
        result,
    ]
}

fn content_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => text_block(s).into_iter().collect(),
        Some(Value::Array(items)) => items.iter().filter_map(block).collect(),
        _ => Vec::new(),
    }
}

fn block(item: &Value) -> Option<Value> {
    match kind(item)? {
        "text" => text_block(item.get("text")?.as_str()?),
        // Thinking is shown as ordinary text, as the desktop host shows omp's.
        "thinking" => text_block(item.get("thinking").or_else(|| item.get("text"))?.as_str()?),
        "toolCall" => {
            let name = item.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("tool");
            let mut call = json!({
                "type": "tool-call",
                "name": name,
                "input": tool_input(item.get("arguments").cloned().unwrap_or(Value::Null)),
            });
            if let Some(call_id) = item.get("id").and_then(Value::as_str) {
                call["callId"] = json!(call_id);
            }
            Some(call)
        }
        "image" => image_block(item),
        _ => None,
    }
}

fn text_block(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| json!({ "type": "text", "text": clip(trimmed, TEXT_CAP) }))
}

/// An image is a label: its bytes stay in the journal. A block the phone cannot load shows as a placeholder.
fn image_block(item: &Value) -> Option<Value> {
    let mime = item.get("mimeType").and_then(Value::as_str).unwrap_or("");
    let data = item.get("data").and_then(Value::as_str).unwrap_or("");
    (mime.starts_with("image/") && !data.trim().is_empty()).then(|| json!({ "type": "image-ref" }))
}

fn image_blocks(content: Option<&Value>) -> Vec<Value> {
    content.and_then(Value::as_array).map_or_else(Vec::new, |items| {
        items.iter().filter(|i| kind(i) == Some("image")).filter_map(image_block).collect()
    })
}

/// Tool arguments are sent as JSON when they fit the tool cap, else as their clipped text.
fn tool_input(args: Value) -> Value {
    let text = args.to_string();
    if text.len() <= TOOL_CAP {
        args
    } else {
        Value::String(clip(&text, TOOL_CAP))
    }
}

/// A tool result's text: string content as it is, a list of blocks joined by line breaks.
fn tool_output(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(s) => Some(s.clone()),
                _ => item
                    .get("text")
                    .or_else(|| item.get("content"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `text` cut to `cap` characters, with the truncation mark when it was cut.
fn clip(text: &str, cap: usize) -> String {
    match text.char_indices().nth(cap) {
        Some((end, _)) => format!("{}{CLIP_MARK}", &text[..end]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    /// A scratch directory, removed when dropped.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("sesync-transcript-{}-{n}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    /// An omp journal in the order omp writes it: the title slot, then one record per line. Compaction
    /// archives a1 and a2 (its firstKeptEntryId is k1). Record positions: title 0, session 1, a1 2, a2 3,
    /// k1 4, k2 5, t1 6, c1 7, n1 8, n2 9.
    const OMP: &[&str] = &[
        r#"{"type":"title","v":1,"title":"fixture","source":"auto","updatedAt":"2026-01-01T00:00:09.000Z"}"#,
        r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/home/u/proj"}"#,
        r#"{"type":"message","id":"a1","timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"archived prompt"}]}}"#,
        r#"{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"archived answer"}],"model":"m1","provider":"p1","stopReason":"stop","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0}}}"#,
        r#"{"type":"message","id":"k1","parentId":"a2","timestamp":"2026-01-01T00:00:03.000Z","message":{"role":"user","content":[{"type":"text","text":"kept prompt"}]}}"#,
        r#"{"type":"message","id":"k2","parentId":"k1","timestamp":"2026-01-01T00:00:04.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"plan"},{"type":"text","text":"kept answer"},{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"ls"}}]}}"#,
        r#"{"type":"message","id":"t1","parentId":"k2","timestamp":"2026-01-01T00:00:05.000Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","isError":false,"content":[{"type":"text","text":"file.txt"}]}}"#,
        r#"{"type":"compaction","id":"c1","parentId":"t1","timestamp":"2026-01-01T00:00:06.000Z","summary":"earlier work","firstKeptEntryId":"k1"}"#,
        r#"{"type":"message","id":"n1","parentId":"c1","timestamp":"2026-01-01T00:00:07.000Z","message":{"role":"user","content":[{"type":"text","text":"newest prompt"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}]}}"#,
        r#"{"type":"message","id":"n2","parentId":"n1","timestamp":"2026-01-01T00:00:08.000Z","message":{"role":"assistant","content":[{"type":"text","text":"newest answer"}],"model":"m1","provider":"p1","stopReason":"stop","usage":{"input":10,"output":4,"cacheRead":2,"cacheWrite":3}}}"#,
    ];

    fn journal(dir: &Scratch, lines: &[&str]) -> PathBuf {
        let path = dir.0.join("2026-01-01T00-00-00-000Z_s1.jsonl");
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    fn ids(view: &Value) -> Vec<&str> {
        view["messages"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect()
    }

    fn conv(harness: &str, key: &str, idents: &[&str]) -> Conversation {
        Conversation {
            harness: harness.into(),
            key: key.into(),
            title: String::new(),
            dir: String::new(),
            updated: 0.0,
            idents: idents.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn the_window_starts_at_the_last_compaction_and_keeps_its_marker() {
        let dir = scratch("window");
        let view = page_json(page(&journal(&dir, OMP), 40, None).unwrap());
        assert_eq!(ids(&view), ["k1", "k2", "t1", "c1", "n1", "n2"]);
        assert_eq!(view["hasMore"], false);
        assert_eq!(view["beforeOffset"], 4);
        assert!(!view.to_string().contains("archived"));
        let marker = &view["messages"][3];
        assert_eq!(marker["role"], "system");
        assert_eq!(marker["blocks"][0]["text"], "Context compacted");
        assert_eq!(marker["blocks"][0]["presentation"], "compaction");
        // Thinking is shown as text ahead of the answer.
        assert_eq!(view["messages"][1]["blocks"][0], json!({ "type": "text", "text": "plan" }));
        let newest = &view["messages"][5];
        assert_eq!(newest["model"], "m1");
        assert_eq!(newest["usage"]["inputTokens"], 10);
        assert_eq!(newest["usage"]["outputTokens"], 4);
    }

    #[test]
    fn a_tool_result_names_the_call_it_answers() {
        let dir = scratch("linkage");
        let view = page_json(page(&journal(&dir, OMP), 40, None).unwrap());
        let call = &view["messages"][1]["blocks"][2];
        assert_eq!(call["type"], "tool-call");
        assert_eq!(call["name"], "bash");
        assert_eq!(call["callId"], "call_1");
        assert_eq!(call["input"], json!({ "command": "ls" }));
        let result = &view["messages"][2];
        assert_eq!(result["role"], "tool");
        assert_eq!(result["blocks"][0]["type"], "tool-result");
        assert_eq!(result["blocks"][0]["callId"], "call_1");
        assert_eq!(result["blocks"][0]["output"], "file.txt");
        assert!(result["blocks"][0].get("isError").is_none());
    }

    #[test]
    fn older_history_is_paged_by_row_cursor() {
        let dir = scratch("paging");
        let path = journal(&dir, OMP);
        let newest = page_json(page(&path, 2, None).unwrap());
        assert_eq!(ids(&newest), ["n1", "n2"]);
        assert_eq!(newest["hasMore"], true);
        assert_eq!(newest["beforeOffset"], 8);
        let older = page_json(page(&path, 2, Some(8)).unwrap());
        assert_eq!(ids(&older), ["t1", "c1"]);
        assert_eq!(older["hasMore"], true);
        assert_eq!(older["beforeOffset"], 6);
        let oldest = page_json(page(&path, 2, Some(6)).unwrap());
        assert_eq!(ids(&oldest), ["k1", "k2"]);
        assert_eq!(oldest["hasMore"], false);
        assert_eq!(oldest["beforeOffset"], 4);
    }

    #[test]
    fn an_image_is_a_label_and_its_bytes_stay_off_the_wire() {
        let dir = scratch("image");
        let view = page_json(page(&journal(&dir, OMP), 40, None).unwrap());
        let blocks = &view["messages"][4]["blocks"];
        assert_eq!(blocks[0], json!({ "type": "text", "text": "newest prompt" }));
        assert_eq!(blocks[1], json!({ "type": "image-ref" }));
        assert!(!view.to_string().contains("iVBOR"));
    }

    #[test]
    fn a_line_still_being_written_is_not_a_record_yet() {
        let dir = scratch("partial");
        let path = journal(&dir, OMP);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"type":"message","id":"n3","parentId":"n2","message":{"role":"user","content":"part"#)
            .unwrap();
        drop(file);
        let partial = page_json(page(&path, 40, None).unwrap());
        assert_eq!(ids(&partial), ["k1", "k2", "t1", "c1", "n1", "n2"]);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"\"}}\n").unwrap();
        drop(file);
        let whole = page_json(page(&path, 40, None).unwrap());
        assert_eq!(ids(&whole), ["k1", "k2", "t1", "c1", "n1", "n2", "n3"]);
    }

    #[test]
    fn a_missing_journal_is_an_error_and_a_pending_one_is_said_so_only_to_a_client_that_asks() {
        let dir = scratch("pending");
        let missing = dir.0.join("none.jsonl");
        assert!(page(&missing, 40, None).is_err());
        let (frame, rows, failed) = first_frame(&missing, 40, true);
        assert_eq!(frame["type"], "snapshot");
        assert_eq!(frame["pending"], true);
        assert!(rows.is_empty() && failed);
        let (frame, _, failed) = first_frame(&missing, 40, false);
        assert_eq!(frame["type"], "error");
        assert!(failed);
        let (frame, rows, failed) = first_frame(&journal(&dir, OMP), 40, false);
        assert_eq!(frame["type"], "snapshot");
        assert_eq!(rows.len(), 6);
        assert!(!failed);
    }

    #[test]
    fn a_source_is_checked_by_its_stamp() {
        let s = |v: &str| Some(v.to_string());
        assert!(!stamp_changed(&None, &None));
        assert!(stamp_changed(&None, &s("10:5")));
        assert!(!stamp_changed(&s("10:5"), &s("10:5")));
        assert!(stamp_changed(&s("10:5"), &s("11:5")));
        assert!(stamp_changed(&s("10:5"), &s("10:6")));
        assert!(stamp_changed(&s("10:5"), &None));
    }

    #[test]
    fn a_grown_window_is_appended_and_a_moved_one_replaced() {
        let row = |id: &str| json!({ "id": id, "role": "user", "blocks": [], "transcriptOffset": 0 });
        let (a, b, c, d) = (row("a"), row("b"), row("c"), row("d"));
        let old = vec![a.clone(), b.clone(), c.clone()];
        // The tail moved on: `a` leaves the window and `d` joins it.
        let frame = delta(&old, &[b.clone(), c.clone(), d.clone()], true, 5).unwrap();
        assert_eq!(frame["type"], "appended");
        assert_eq!(frame["messages"], json!([d]));
        // Nothing new in the window: no frame.
        assert!(delta(&old, &old, false, 0).is_none());
        assert!(delta(&old, &[b.clone(), c.clone()], false, 0).is_none());
        // A row changed in place is resent, so the client's copy follows.
        let c2 = json!({ "id": "c", "role": "assistant", "blocks": [{ "type": "text", "text": "more" }], "transcriptOffset": 0 });
        let frame = delta(&old, &[a.clone(), b.clone(), c2.clone()], false, 0).unwrap();
        assert_eq!(frame["type"], "appended");
        assert_eq!(frame["messages"], json!([c2]));
        // The window no longer holds the client's last row (a rewritten branch): the whole window replaces.
        let rewritten = vec![row("x")];
        let frame = delta(&old, &rewritten, false, 3).unwrap();
        assert_eq!(frame["type"], "replacement");
        assert_eq!(frame["messages"], json!(rewritten));
        assert_eq!(frame["hasMore"], false);
        assert_eq!(frame["beforeOffset"], 3);
    }

    #[test]
    fn a_phone_session_names_its_conversation_by_the_parent_identifier() {
        let list = vec![conv("omp", "/a.jsonl", &["s-a", "mirror-a"]), conv("omp", "/b.jsonl", &["s-b"])];
        assert_eq!(session_id(&list[0]), "s-a");
        assert_eq!(pick(list.clone(), "s-b", None).unwrap().key, "/b.jsonl");
        // A mirror's identifier is not the parent's.
        assert!(pick(list.clone(), "mirror-a", None).is_none());
        assert_eq!(pick(list.clone(), "zzz", Some("/a.jsonl")).unwrap().key, "/a.jsonl");
        assert!(pick(list, "zzz", None).is_none());
        assert_eq!(session_id(&conv("omp", "/c.jsonl", &[])), "");
    }

    #[test]
    fn omp_and_pi_journals_are_read_as_they_are() {
        let catalog = crate::bridge::testkit::Fixed(vec![]);
        let dir = std::env::temp_dir();
        assert_eq!(journal_for(&catalog, &dir, &conv("omp", "/x.jsonl", &[]), None).unwrap(), PathBuf::from("/x.jsonl"));
        assert_eq!(journal_for(&catalog, &dir, &conv("pi", "/x.jsonl", &[]), None).unwrap(), PathBuf::from("/x.jsonl"));
    }

    /// A catalog whose converter writes `text` into the scratch directory and counts its runs.
    struct Converter {
        stamp: parking_lot::Mutex<String>,
        text: parking_lot::Mutex<String>,
        runs: std::sync::atomic::AtomicUsize,
    }

    impl Catalog for Converter {
        fn conversations(&self) -> Result<Vec<Conversation>, String> {
            Ok(Vec::new())
        }
        fn source_stamp(&self, _conv: &Conversation) -> Option<String> {
            Some(self.stamp.lock().clone())
        }
        fn canonical_journal(&self, _conv: &Conversation, scratch: &Path) -> Result<PathBuf, String> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            std::fs::create_dir_all(scratch).unwrap();
            let out = scratch.join("canonical.jsonl");
            std::fs::write(&out, self.text.lock().as_bytes()).unwrap();
            Ok(out)
        }
    }

    #[test]
    fn a_converted_journal_is_reused_for_its_source_state_and_replaced_when_the_source_moves() {
        let dir = std::env::temp_dir().join(format!("sesync-transcript-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let catalog = Converter {
            stamp: parking_lot::Mutex::new("1:1".into()),
            text: parking_lot::Mutex::new("{\"type\":\"session\"}\n".into()),
            runs: Default::default(),
        };
        let claude = conv("claude", "/src/0d42.jsonl", &[]);
        let first = journal_for(&catalog, &dir, &claude, Some("1:1")).unwrap();
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "{\"type\":\"session\"}\n");
        assert_eq!(journal_for(&catalog, &dir, &claude, Some("1:1")).unwrap(), first, "the same state is reused");
        assert_eq!(catalog.runs.load(Ordering::SeqCst), 1, "the converter ran once for that state");

        *catalog.text.lock() = "{\"type\":\"session\",\"more\":true}\n".into();
        let second = journal_for(&catalog, &dir, &claude, Some("2:2")).unwrap();
        assert_ne!(second, first);
        assert_eq!(catalog.runs.load(Ordering::SeqCst), 2, "a moved source converts again");
        assert!(!first.exists(), "the copy of the older state is removed");
        assert!(second.is_file());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&second).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a converted copy is owner-only");
        assert!(journal_for(&catalog, &dir, &claude, None).is_err(), "a source that is gone cannot be read");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clips_cut_on_characters_and_say_so() {
        assert_eq!(clip("ééé", 2), format!("éé{CLIP_MARK}"));
        assert_eq!(clip("ééé", 3), "ééé");
    }

    #[test]
    fn a_reply_the_provider_refused_shows_why_in_the_chat() {
        // As omp journalled it on 2026-10-31 after its Anthropic login was lost: no content, only the error.
        let refused = json!({
            "role": "assistant", "content": [], "provider": "anthropic", "model": "claude-opus-5-5", "stopReason": "error",
            "errorMessage": "400 {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"Your credit balance is too low to access the Anthropic API.\"},\"request_id\":\"req_1\"}\nraw-http-request=/home/u/.omp/logs/http-400-requests/1.json",
        });
        let row = message_row(&refused, "e1", Some(1), 7).expect("a failed turn is not dropped");
        assert_eq!(row["role"], "system");
        assert_eq!(
            row["blocks"][0]["text"],
            "Model call failed (anthropic/claude-opus-5-5): Your credit balance is too low to access the Anthropic API. (HTTP 400)"
        );

        // A reply cut off by an error after some text keeps the text, then says why it stopped.
        let partial = json!({ "role": "assistant", "content": [{ "type": "text", "text": "Half an answer" }], "stopReason": "error", "errorMessage": "socket hang up" });
        let row = message_row(&partial, "e2", None, 8).unwrap();
        assert_eq!(row["role"], "assistant");
        assert_eq!(row["blocks"][1]["text"], "Model call failed: socket hang up");

        let finished = json!({ "role": "assistant", "content": [{ "type": "text", "text": "done" }], "stopReason": "stop" });
        assert_eq!(message_row(&finished, "e3", None, 9).unwrap()["blocks"].as_array().unwrap().len(), 1);
        let aborted = json!({ "role": "assistant", "content": [], "stopReason": "aborted" });
        assert_eq!(message_row(&aborted, "e4", None, 10).unwrap()["blocks"][0]["text"], INTERRUPTED);
    }
}
