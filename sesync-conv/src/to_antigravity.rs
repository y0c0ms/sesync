//! omp/Pi journal -> Antigravity CLI conversation (`<store>/conversations/<id>.db`).
//!
//! Exports the LIVE window (the compaction summary first, as a user step), the
//! field set proven to list and resume on agy 1.4.4:
//!   user            -> 14 / payload 19 {2 text, 3 {1 text}, 9 {1 mime, 2 bytes} per image}
//!   assistant       -> 15 / payload 20 {1 text, 3 thinking, 7 ToolCall {1 id, 2 name, 3 JSON}..., 8 text, 12: 2}
//!   toolResult      -> 132 / payload 140 {1 {1 key, 2 value}..., 2 {1 text}}, status 3 (7 on isError)
//! Every step: payload {1 type, 4 status, 5 StepMetadata}, and the `metadata`
//! column = the same StepMetadata {1 Timestamp, 3 source, [4 ToolCall on results]}.
//! agy replays a tool call and its result only if the result's StepMetadata
//! carries the matching ToolCall in payload field 5; the column alone is not enough.
//!
//! Dropped and counted: tool-result images (a counted placeholder line instead:
//! agy has no slot), assistant images, other block types, non-conversation roles,
//! results whose call never appeared (agy would replay a functionResponse with no
//! functionCall) and second results for one call.

use crate::antigravity::{self as ag, with_suffix};
use crate::journal::{parse_iso_ms, Journal};
use crate::protobuf::{timestamp, Writer};
use base64::Engine;
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub user_steps: usize,
    pub planner_steps: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub images: usize,
    pub thinking_blocks: usize,
    pub generated_call_ids: usize,
    pub tool_result_images_dropped: usize,
    pub orphan_tool_results: usize,
    pub duplicate_tool_results: usize,
    pub dropped_blocks: usize,
    pub skipped_messages: usize,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub step_type: i64,
    pub status: i64,
    pub metadata: Vec<u8>,
    pub payload: Vec<u8>,
}

fn step(step_type: i64, status: i64, meta: &Writer, body_field: u32, body: &Writer) -> Step {
    let mut p = Writer::new();
    p.varint(1, step_type as u64).varint(4, status as u64).message(5, meta).message(body_field, body);
    Step { step_type, status, metadata: meta.finish(), payload: p.into_bytes() }
}

fn meta(ms: i64, source: u64, call: Option<&Writer>) -> Writer {
    let mut m = Writer::new();
    m.message(1, &timestamp(ms)).varint(3, source);
    if let Some(c) = call {
        m.message(4, c);
    }
    m
}

struct Call {
    id: String,
    name: String,
    args: Value,
}

impl Call {
    fn proto(&self) -> Writer {
        let mut w = Writer::new();
        w.string(1, &self.id).string(2, &self.name).string(3, &serde_json::to_string(&self.args).unwrap_or_else(|_| "{}".into()));
        w
    }
}

fn block_type(b: &Value) -> Option<&str> {
    b.get("type").and_then(Value::as_str)
}

fn user_step(text: &str, images: &[(String, Vec<u8>)], ms: i64) -> Step {
    let mut u = Writer::new();
    let mut item = Writer::new();
    item.string(1, text);
    u.string(2, text).message(3, &item);
    for (mime, data) in images {
        let mut m = Writer::new();
        m.string(1, mime).bytes(2, data);
        u.message(9, &m);
    }
    step(ag::STEP_USER_INPUT, ag::STATUS_DONE, &meta(ms, ag::SOURCE_USER, None), 19, &u)
}

/// The steps for the journal's live window, plus the trajectory start time (ms).
pub fn build_steps(j: &Journal) -> Result<(Vec<Step>, Stats, i64), String> {
    let msgs = j.messages();
    if !msgs.iter().any(|m| m.role == "user" || m.role == "assistant") {
        return Err("empty conversation: the journal has no user or assistant message".into());
    }
    let start_ms = j
        .timestamp()
        .and_then(parse_iso_ms)
        .or_else(|| msgs.first().map(|m| m.timestamp_ms))
        .unwrap_or(0);
    let mut st = Stats::default();
    let mut steps = Vec::new();
    if let Some(note) = j.compaction_note() {
        steps.push(user_step(&note, &[], start_ms));
        st.user_steps += 1;
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut calls: HashMap<String, Call> = HashMap::new();
    let mut pending: Vec<String> = Vec::new();
    let mut answered: HashSet<String> = HashSet::new();
    let mut call_seq = 0usize;

    for m in &msgs {
        let ms = m.timestamp_ms;
        match m.role {
            "user" => {
                let mut texts: Vec<&str> = Vec::new();
                let mut images = Vec::new();
                for b in &m.content {
                    match b {
                        Value::String(s) => texts.push(s),
                        _ => match block_type(b) {
                            Some("text") => texts.push(b.get("text").and_then(Value::as_str).unwrap_or("")),
                            Some("image") => {
                                let data = b.get("data").and_then(Value::as_str).unwrap_or("");
                                match b64.decode(data) {
                                    Ok(bytes) if !bytes.is_empty() => {
                                        let mime = b.get("mimeType").and_then(Value::as_str).unwrap_or("image/png");
                                        images.push((mime.to_string(), bytes));
                                        st.images += 1;
                                    }
                                    _ => st.dropped_blocks += 1,
                                }
                            }
                            _ => st.dropped_blocks += 1,
                        },
                    }
                }
                let text = texts.join("\n");
                if text.is_empty() && images.is_empty() {
                    st.skipped_messages += 1;
                    continue;
                }
                steps.push(user_step(&text, &images, ms));
                st.user_steps += 1;
            }
            "assistant" => {
                let mut texts: Vec<&str> = Vec::new();
                let mut thinking: Vec<&str> = Vec::new();
                let mut call_protos = Vec::new();
                for b in &m.content {
                    match b {
                        Value::String(s) => texts.push(s),
                        _ => match block_type(b) {
                            Some("text") => texts.push(b.get("text").and_then(Value::as_str).unwrap_or("")),
                            Some("thinking") => {
                                let t = b.get("thinking").and_then(Value::as_str).unwrap_or("");
                                if !t.is_empty() {
                                    thinking.push(t);
                                    st.thinking_blocks += 1;
                                }
                            }
                            Some("toolCall") => {
                                call_seq += 1;
                                let mut id = b.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                                if id.is_empty() || calls.contains_key(&id) {
                                    id = format!("call-{call_seq}");
                                    st.generated_call_ids += 1;
                                }
                                let name = b.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("unknown_tool");
                                let args = b.get("arguments").cloned().filter(|v| !v.is_null()).unwrap_or_else(|| serde_json::json!({}));
                                let c = Call { id: id.clone(), name: name.to_string(), args };
                                call_protos.push(c.proto());
                                calls.insert(id.clone(), c);
                                pending.push(id);
                                st.tool_calls += 1;
                            }
                            _ => st.dropped_blocks += 1,
                        },
                    }
                }
                let text = texts.into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n");
                let think = thinking.join("\n\n");
                if text.is_empty() && think.is_empty() && call_protos.is_empty() {
                    st.skipped_messages += 1;
                    continue;
                }
                let mut p = Writer::new();
                if !text.is_empty() {
                    p.string(1, &text);
                }
                if !think.is_empty() {
                    p.string(3, &think);
                }
                for c in &call_protos {
                    p.message(7, c);
                }
                if !text.is_empty() {
                    p.string(8, &text);
                }
                p.varint(12, 2);
                steps.push(step(ag::STEP_PLANNER_RESPONSE, ag::STATUS_DONE, &meta(ms, ag::SOURCE_MODEL, None), 20, &p));
                st.planner_steps += 1;
            }
            "toolResult" => {
                let given = m.message.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                let id = if given.is_empty() {
                    match pending.first() {
                        Some(id) => id.clone(),
                        None => {
                            st.orphan_tool_results += 1;
                            continue;
                        }
                    }
                } else if calls.contains_key(given) {
                    given.to_string()
                } else {
                    st.orphan_tool_results += 1;
                    continue;
                };
                if !answered.insert(id.clone()) {
                    st.duplicate_tool_results += 1;
                    continue;
                }
                pending.retain(|p| p != &id);
                let call = &calls[&id];
                let mut texts: Vec<String> = Vec::new();
                let mut imgs = 0usize;
                for b in &m.content {
                    match b {
                        Value::String(s) => texts.push(s.clone()),
                        _ => match block_type(b) {
                            Some("text") => texts.push(b.get("text").and_then(Value::as_str).unwrap_or("").to_string()),
                            Some("image") => imgs += 1,
                            _ => st.dropped_blocks += 1,
                        },
                    }
                }
                if imgs > 0 {
                    texts.push(format!("[{imgs} image(s) omitted: Antigravity has no slot for tool-result images]"));
                    st.tool_result_images_dropped += imgs;
                }
                let is_error = m.message.get("isError").and_then(Value::as_bool).unwrap_or(false);
                let mut g = Writer::new();
                if let Value::Object(o) = &call.args {
                    for (k, v) in o {
                        let mut kv = Writer::new();
                        let vs = match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        kv.string(1, k).string(2, &vs);
                        g.message(1, &kv);
                    }
                }
                let mut r = Writer::new();
                r.string(1, &texts.join("\n"));
                g.message(2, &r);
                let status = if is_error { ag::STATUS_ERROR } else { ag::STATUS_DONE };
                steps.push(step(ag::STEP_GENERIC, status, &meta(ms, ag::SOURCE_MODEL, Some(&call.proto())), 140, &g));
                st.tool_results += 1;
            }
            _ => st.skipped_messages += 1,
        }
    }
    Ok((steps, st, start_ms))
}

/// Trajectory id: derived from the conversation id, never equal to it.
pub fn trajectory_id(cid: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, format!("sesync-antigravity-trajectory:{cid}").as_bytes()).to_string()
}

fn trajectory_metadata(cid: &str, uri: &str, start_ms: i64) -> Vec<u8> {
    let mut ws = Writer::new();
    ws.string(1, uri);
    let mut w = Writer::new();
    w.message(1, &ws).message(2, &timestamp(start_ms)).string(6, cid).string(7, uri).string(18, ag::CLI_PROJECT);
    w.into_bytes()
}

/// Write a complete conversation DB at `path` (which must not exist) and close it.
pub fn write_db(path: &Path, cid: &str, cwd: &str, start_ms: i64, steps: &[Step]) -> Result<(), String> {
    let e = |e: rusqlite::Error| format!("{}: {e}", path.display());
    let uri = ag::path_to_file_uri(cwd);
    let mut c = Connection::open(path).map_err(e)?;
    c.execute_batch("PRAGMA page_size=32768; PRAGMA journal_mode=DELETE;").map_err(e)?;
    c.execute_batch(ag::SCHEMA).map_err(e)?;
    c.pragma_update(None, "user_version", 1).map_err(e)?;
    let tx = c.transaction().map_err(e)?;
    tx.execute(
        "INSERT INTO trajectory_meta (trajectory_id, cascade_id, trajectory_type, source) VALUES (?1, ?2, ?3, ?4)",
        params![trajectory_id(cid), cid, ag::TRAJECTORY_TYPE, ag::CLI_SOURCE],
    )
    .map_err(e)?;
    tx.execute(
        "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
        params![trajectory_metadata(cid, &uri, start_ms)],
    )
    .map_err(e)?;
    {
        let mut ins = tx
            .prepare(
                "INSERT INTO steps (idx, step_type, status, has_subtrajectory, metadata, step_payload, step_format) \
                 VALUES (?1, ?2, ?3, 0, ?4, ?5, 0)",
            )
            .map_err(e)?;
        for (i, s) in steps.iter().enumerate() {
            ins.execute(params![i as i64, s.step_type, s.status, s.metadata, s.payload]).map_err(e)?;
        }
    }
    tx.commit().map_err(e)?;
    c.close().map_err(|(_, err)| e(err))?;
    // FlushFileBuffers needs write access on Windows.
    fs::OpenOptions::new().write(true).open(path).and_then(|f| f.sync_all()).map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(())
}

fn in_use(p: &Path, err: &std::io::Error) -> String {
    format!(
        "{}: in use (Antigravity has this conversation open: {err}); the existing conversation was left untouched",
        p.display()
    )
}

/// Move `tmp` to `db`, replacing an existing conversation and its `-wal`/`-shm`.
/// The old files are first renamed aside; if any of them is held open (Windows
/// refuses to rename an open SQLite file), every rename is undone and nothing changes.
/// An old WAL is never left next to the new file, where SQLite would replay it.
pub fn publish(tmp: &Path, db: &Path) -> Result<(), String> {
    let olds: Vec<PathBuf> = [db.to_path_buf(), with_suffix(db, "-wal"), with_suffix(db, "-shm")]
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let undo = |moved: &[(PathBuf, PathBuf)]| {
        for (orig, aside) in moved.iter().rev() {
            let _ = fs::rename(aside, orig);
        }
    };
    for p in &olds {
        let aside = with_suffix(p, ".sesync-old");
        let _ = fs::remove_file(&aside);
        if let Err(err) = fs::rename(p, &aside) {
            undo(&moved);
            let _ = fs::remove_file(tmp);
            return Err(in_use(p, &err));
        }
        moved.push((p.clone(), aside));
    }
    if let Err(err) = fs::rename(tmp, db) {
        undo(&moved);
        let _ = fs::remove_file(tmp);
        return Err(in_use(db, &err));
    }
    for (_, aside) in &moved {
        let _ = fs::remove_file(aside);
    }
    Ok(())
}

/// Write `text` to `path` atomically (temp file + rename).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = with_suffix(path, ".sesync-tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// The title agy cached for `id` in `jetbox_summaries_proto.pb`, and the file
/// rewritten without that entry. None if the file has no entry for `id`.
pub fn jetbox_without(bytes: &[u8], id: &str) -> Result<Option<(String, Vec<u8>)>, String> {
    use crate::protobuf::{parse, Msg, Value as PV};
    let fields = parse(bytes)?;
    let mut found = None;
    let mut out = Vec::with_capacity(bytes.len());
    for f in &fields {
        if let (1, PV::Bytes(b)) = (f.num, f.value) {
            let entry = Msg::lenient(b);
            if entry.string(1).as_deref() == Some(id) {
                let title = entry.message(2).and_then(|s| s.string(1)).unwrap_or_default();
                found = Some(title);
                continue;
            }
        }
        out.extend_from_slice(f.raw);
    }
    Ok(found.map(|t| (t, out)))
}

/// Write or replace `annotations/<id>.pbtxt` (agy's source of truth for the title),
/// keeping an existing `last_user_view_time:` line verbatim.
pub fn write_annotation(store: &Path, id: &str, title: &str) -> Result<(), String> {
    let ann = ag::annotation_path(store, id);
    if let Some(dir) = ann.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let existing = fs::read_to_string(&ann).ok();
    let text = ag::annotation_text(title, existing.as_deref());
    if existing.as_deref() != Some(text.as_str()) {
        write_atomic(&ann, text.as_bytes()).map_err(|e| format!("{}: {e}", ann.display()))?;
    }
    Ok(())
}

fn io_locked(e: &std::io::Error) -> bool {
    // 5 access denied, 32 sharing violation, 33 lock violation (Windows)
    e.kind() == std::io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

fn sql_locked(e: &rusqlite::Error) -> bool {
    matches!(e.sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

/// The titles agy cached for `id`: (jetbox entry, conversation_summaries row).
fn cached_titles(store: &Path, id: &str) -> Result<(Option<String>, Option<String>), String> {
    let jet = store.join("jetbox_summaries_proto.pb");
    let jet_title = match fs::read(&jet) {
        Ok(b) => jetbox_without(&b, id).map_err(|e| format!("{}: not parsed ({e})", jet.display()))?.map(|(t, _)| t),
        Err(_) => None,
    };
    let sum_db = store.join("conversation_summaries.db");
    let row_title = if sum_db.is_file() {
        Connection::open_with_flags(&sum_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|c| {
                c.busy_timeout(Duration::from_secs(2))?;
                c.query_row("SELECT title FROM conversation_summaries WHERE conversation_id = ?1", [id], |r| r.get(0))
            })
            .ok()
    } else {
        None
    };
    Ok((jet_title, row_title))
}

/// Drop `id`'s cached summary so agy rebuilds it (with the annotations title) at its next
/// start: its entry in `jetbox_summaries_proto.pb` (atomic rewrite, other entries
/// byte-identical) and its `conversation_summaries` row. Ok(notes) lists the cache files
/// skipped because agy holds them (empty = all done); Err is any other failure.
pub fn drop_cached_summary(store: &Path, id: &str) -> Result<Vec<String>, String> {
    let mut locked = Vec::new();
    let jet = store.join("jetbox_summaries_proto.pb");
    match fs::read(&jet) {
        Ok(b) => {
            if let Some((_, rest)) = jetbox_without(&b, id).map_err(|e| format!("{}: not parsed ({e})", jet.display()))? {
                if let Err(e) = write_atomic(&jet, &rest) {
                    if !io_locked(&e) {
                        return Err(format!("{}: {e}", jet.display()));
                    }
                    locked.push(format!("{}: locked by a running agy, cached title not dropped ({e})", jet.display()));
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) if io_locked(&e) => locked.push(format!("{}: locked by a running agy ({e})", jet.display())),
        Err(e) => return Err(format!("{}: {e}", jet.display())),
    }
    let sum_db = store.join("conversation_summaries.db");
    if sum_db.is_file() {
        let r = Connection::open(&sum_db).and_then(|c| {
            c.busy_timeout(Duration::from_secs(2))?;
            c.execute("DELETE FROM conversation_summaries WHERE conversation_id = ?1", [id])?;
            c.close().map_err(|(_, e)| e)
        });
        match r {
            Ok(()) => {}
            Err(e) if sql_locked(&e) => locked.push(format!("{}: locked by a running agy, summary row not dropped ({e})", sum_db.display())),
            Err(e) => return Err(format!("{}: {e}", sum_db.display())),
        }
    }
    Ok(locked)
}

/// Title a freshly installed conversation: annotations, then drop the cached summary
/// only if agy cached a different title. Cache problems are notes, never errors: the
/// conversation is already installed.
pub fn apply_title(store: &Path, id: &str, title: &str) -> Result<Vec<String>, String> {
    write_annotation(store, id, title)?;
    let (jet, row) = match cached_titles(store, id) {
        Ok(x) => x,
        Err(e) => return Ok(vec![format!("{e}; cache left as is")]),
    };
    let stale = jet.is_some_and(|t| t != title) || row.is_some_and(|t| t != title);
    if !stale {
        return Ok(Vec::new());
    }
    Ok(drop_cached_summary(store, id).unwrap_or_else(|e| vec![format!("{e}; cache left as is")]))
}

#[derive(Debug)]
pub enum RetitleError {
    /// A cache file is held by a running agy; the annotations change is in place.
    Locked(Vec<String>),
    Other(String),
}

/// `antigravity-retitle`: rename an existing conversation.
pub fn retitle(store: &Path, id: &str, title: &str) -> Result<(), RetitleError> {
    check_id(id).map_err(RetitleError::Other)?;
    let db = store.join("conversations").join(format!("{id}.db"));
    if !db.is_file() {
        return Err(RetitleError::Other(format!("unknown conversation {id}: {} does not exist", db.display())));
    }
    if title.trim().is_empty() {
        return Err(RetitleError::Other("empty title".into()));
    }
    write_annotation(store, id, title).map_err(RetitleError::Other)?;
    let locked = drop_cached_summary(store, id).map_err(RetitleError::Other)?;
    if locked.is_empty() { Ok(()) } else { Err(RetitleError::Locked(locked)) }
}

fn check_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.contains(['/', '\\', ':']) || id.starts_with('.') {
        return Err(format!("invalid conversation id {id:?}"));
    }
    Ok(())
}

/// Convert and install. Returns the installed `.db` path, stats and stderr notes.
pub fn install(j: &Journal, store: &Path, id: &str, title: &str) -> Result<(PathBuf, Stats, Vec<String>), String> {
    check_id(id)?;
    let cwd = j.cwd().unwrap_or("").trim();
    if cwd.is_empty() {
        return Err("the journal header has no cwd: Antigravity lists conversations by workspace".into());
    }
    let (steps, stats, start_ms) = build_steps(j)?;
    let dir = store.join("conversations");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let db = dir.join(format!("{id}.db"));
    let tmp = dir.join(format!("{id}.db.tmp"));
    for p in [tmp.clone(), with_suffix(&tmp, "-journal")] {
        if p.exists() {
            fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
    }
    if let Err(e) = write_db(&tmp, id, cwd, start_ms, &steps) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    publish(&tmp, &db)?;
    let notes = if title.trim().is_empty() { Vec::new() } else { apply_title(store, id, title)? };
    Ok((db, stats, notes))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::from_antigravity;
    use crate::protobuf::{timestamp_ms, Msg};

    /// A scratch directory whose path has a space and an accent, like real Windows profiles.
    pub fn scratch(name: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("sesync agy é {name}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn journal_from(dir: &Path, lines: &[Value]) -> Journal {
        let p = dir.join("source.jsonl");
        let text: String = lines.iter().map(|l| serde_json::to_string(l).unwrap() + "\n").collect();
        fs::write(&p, text).unwrap();
        Journal::read(&p).unwrap()
    }

    pub const CWD: &str = r"C:\Users\Zé Silva\proj one";
    pub const TITLE: &str = "Fix \"quoted\" bug\nsecond line \\ end";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-image-bytes";

    fn msg(id: &str, parent: Option<&str>, ts: &str, message: Value) -> Value {
        serde_json::json!({"type": "message", "id": id, "parentId": parent, "timestamp": ts, "message": message})
    }

    /// User text + image, assistant thinking/text/two calls (one without id), an ok and an
    /// error result (the error one with an image), then a closing assistant reply.
    pub fn sample(dir: &Path) -> Journal {
        let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);
        journal_from(
            dir,
            &[
                serde_json::json!({"type": "session", "version": 3, "id": "src-1", "timestamp": "2026-10-01T10:00:00.250Z", "cwd": CWD}),
                msg("u1", None, "2026-10-01T10:00:01.123Z", serde_json::json!({"role": "user", "content": [
                    {"type": "text", "text": "Read \"a.txt\" please\nand <b>"}, {"type": "image", "data": b64, "mimeType": "image/png"}]})),
                msg("a1", Some("u1"), "2026-10-01T10:00:02.456Z", serde_json::json!({"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "I should read it"},
                    {"type": "text", "text": "Reading."},
                    {"type": "toolCall", "id": "call_A", "name": "read", "arguments": {"path": "C:\\a b\\a.txt", "limit": 5}},
                    {"type": "toolCall", "name": "bash", "arguments": {"command": "ls"}}]})),
                msg("r1", Some("a1"), "2026-10-01T10:00:03.001Z", serde_json::json!({"role": "toolResult", "toolCallId": "call_A", "toolName": "read",
                    "content": [{"type": "text", "text": "line \"one\"\nline two"}], "isError": false})),
                msg("r2", Some("r1"), "2026-10-01T10:00:03.999Z", serde_json::json!({"role": "toolResult", "toolName": "bash",
                    "content": [{"type": "text", "text": "ls: denied"}, {"type": "image", "data": b64, "mimeType": "image/png"}], "isError": true})),
                msg("a2", Some("r2"), "2026-10-01T10:00:04.500Z", serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "Done reading."}]})),
            ],
        )
    }

    fn read_back(db: &Path) -> (Vec<Value>, from_antigravity::Stats) {
        let (lines, st) = from_antigravity::to_journal_lines(db).unwrap();
        (lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect(), st)
    }

    fn rows(db: &Path) -> Vec<(i64, i64, Option<Vec<u8>>, Vec<u8>)> {
        let c = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut st = c.prepare("SELECT step_type, status, metadata, step_payload FROM steps ORDER BY idx").unwrap();
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().map(Result::unwrap).collect();
        v
    }

    #[test]
    fn journal_db_journal_round_trip_keeps_the_conversation() {
        let dir = scratch("roundtrip");
        let store = dir.join("antigravity-cli");
        let j = sample(&dir);
        let (db, st, _) = install(&j, &store, "conv-1", TITLE).unwrap();
        assert_eq!(db, store.join("conversations").join("conv-1.db"));
        assert_eq!((st.user_steps, st.planner_steps, st.tool_calls, st.tool_results), (1, 2, 2, 2));
        assert_eq!((st.images, st.thinking_blocks, st.generated_call_ids, st.tool_result_images_dropped), (1, 1, 1, 1));

        let (v, rst) = read_back(&db);
        let h = &v[0];
        assert_eq!(h["id"], "conv-1");
        assert_eq!(h["cwd"], CWD, "cwd with a space and an accent survives the file URI");
        assert_eq!(h["timestamp"], "2026-10-01T10:00:00.250Z");
        assert_eq!(h["title"], TITLE, "title with quotes, newline and backslash");
        let m: Vec<&Value> = v[1..].iter().collect();
        assert_eq!(m.len(), 5);
        let ts: Vec<&str> = m.iter().map(|r| r["timestamp"].as_str().unwrap()).collect();
        assert_eq!(ts, ["2026-10-01T10:00:01.123Z", "2026-10-01T10:00:02.456Z", "2026-10-01T10:00:03.001Z", "2026-10-01T10:00:03.999Z", "2026-10-01T10:00:04.500Z"]);
        for w in m.windows(2) {
            assert_eq!(w[1]["parentId"], w[0]["id"], "messages form one chain");
        }

        let u = &m[0]["message"];
        assert_eq!(u["role"], "user");
        assert_eq!(u["content"][0]["text"], "Read \"a.txt\" please\nand <b>");
        assert_eq!(u["content"][1]["mimeType"], "image/png");
        let img = base64::engine::general_purpose::STANDARD.decode(u["content"][1]["data"].as_str().unwrap()).unwrap();
        assert_eq!(img, PNG, "image bytes are byte-exact");

        let a = &m[1]["message"]["content"];
        assert_eq!(a[0], serde_json::json!({"type": "thinking", "thinking": "I should read it"}));
        assert_eq!(a[1]["text"], "Reading.");
        assert_eq!(a[2]["id"], "call_A");
        assert_eq!(a[2]["name"], "read");
        assert_eq!(a[2]["arguments"], serde_json::json!({"path": "C:\\a b\\a.txt", "limit": 5}));
        assert_eq!(a[3]["id"], "call-2", "a missing call id becomes deterministic call-<n>");
        assert_eq!(a[3]["arguments"], serde_json::json!({"command": "ls"}));

        let r1 = &m[2]["message"];
        assert_eq!((r1["toolCallId"].as_str(), r1["toolName"].as_str(), r1["isError"].as_bool()), (Some("call_A"), Some("read"), Some(false)));
        assert_eq!(r1["content"][0]["text"], "line \"one\"\nline two");
        let r2 = &m[3]["message"];
        assert_eq!((r2["toolCallId"].as_str(), r2["isError"].as_bool()), (Some("call-2"), Some(true)), "result without toolCallId pairs with the pending call");
        assert_eq!(r2["content"][0]["text"], "ls: denied\n[1 image(s) omitted: Antigravity has no slot for tool-result images]");
        assert_eq!(m[4]["message"]["content"][0]["text"], "Done reading.");
        assert_eq!((rst.tool_calls, rst.tool_results, rst.images, rst.thinking_blocks, rst.opaque_steps), (2, 2, 1, 1, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn db_has_the_fields_agy_needs_to_list_and_resume() {
        let dir = scratch("fields");
        let store = dir.join("s");
        let (db, _, _) = install(&sample(&dir), &store, "conv-2", "").unwrap();
        let c = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let uv: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(uv, 1, "user_version 0 makes agy wipe the workspace");
        let (tid, cid, ty, src): (String, String, i64, i64) =
            c.query_row("SELECT * FROM trajectory_meta", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
        assert_eq!((cid.as_str(), ty, src), ("conv-2", 4, 17));
        assert_ne!(tid, cid);
        let tm: Vec<u8> = c.query_row("SELECT data FROM trajectory_metadata_blob WHERE id='main'", [], |r| r.get(0)).unwrap();
        let tm = Msg::decode(&tm).unwrap();
        let uri = "file:///C:/Users/Z%C3%A9%20Silva/proj%20one";
        assert_eq!(tm.message(1).unwrap().string(1).as_deref(), Some(uri));
        assert_eq!(tm.string(7).as_deref(), Some(uri));
        assert_eq!(tm.string(6).as_deref(), Some("conv-2"));
        assert_eq!(tm.string(18).as_deref(), Some("default-cli-project"));
        assert_eq!(timestamp_ms(&tm.message(2).unwrap()), parse_iso_ms("2026-10-01T10:00:00.250Z"));
        drop(c);
        assert!(!store.join("annotations").exists(), "no title, no annotations file");
        assert!(!store.join("conversations").join("conv-2.db.tmp").exists());

        let mut calls: HashMap<String, Vec<u8>> = HashMap::new();
        for (ty, status, md, payload) in rows(&db) {
            let p = Msg::decode(&payload).unwrap();
            assert_eq!(p.varint(1), Some(ty as u64), "payload type equals the column");
            assert_eq!(p.varint(4), Some(status as u64), "payload status equals the column");
            assert_eq!(md.as_deref(), p.bytes(5), "metadata column is the payload's field 5");
            if ty == ag::STEP_PLANNER_RESPONSE {
                for c in p.message(20).unwrap().all_bytes(7) {
                    calls.insert(Msg::decode(c).unwrap().string(1).unwrap(), c.to_vec());
                }
            }
            if ty == ag::STEP_GENERIC {
                let link = p.message(5).unwrap().bytes(4).expect("result carries its ToolCall in payload 5.4");
                let id = Msg::decode(link).unwrap().string(1).unwrap();
                assert_eq!(calls.get(&id).map(Vec::as_slice), Some(link), "the link is the exact ToolCall of the planner step");
                let col = Msg::decode(md.as_deref().unwrap()).unwrap();
                assert_eq!(col.bytes(4), Some(link), "and on the metadata column");
            }
        }
        assert_eq!(calls.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_summary_comes_first_and_the_archive_is_dropped() {
        use crate::journal::fixture::*;
        let j = compacted_journal("agy");
        let (steps, st, _) = build_steps(&j).unwrap();
        let texts: Vec<String> = steps
            .iter()
            .map(|s| Msg::decode(&s.payload).unwrap().message(19).unwrap().string(2).unwrap())
            .collect();
        assert_eq!(texts.len(), 4);
        assert!(texts[0].contains(SUMMARY), "the summary is the first user step");
        assert_eq!(&texts[1..], [KEPT_ONE, KEPT_TWO, NEWEST]);
        assert!(!texts.iter().any(|t| t.contains(ARCHIVED_ONE) || t.contains(ARCHIVED_TWO)));
        assert_eq!(st.user_steps, 4);
    }

    #[test]
    fn refuses_empty_or_workspaceless_journals() {
        let dir = scratch("refuse");
        let store = dir.join("s");
        let only_result = journal_from(&dir, &[
            serde_json::json!({"type": "session", "id": "x", "timestamp": "2026-10-01T10:00:00.000Z", "cwd": CWD}),
            msg("r", None, "2026-10-01T10:00:01.000Z", serde_json::json!({"role": "toolResult", "toolCallId": "c", "content": []})),
        ]);
        let e = install(&only_result, &store, "e1", "t").unwrap_err();
        assert!(e.contains("empty conversation"), "{e}");
        let no_cwd = journal_from(&dir, &[
            serde_json::json!({"type": "session", "id": "x", "timestamp": "2026-10-01T10:00:00.000Z"}),
            msg("u", None, "2026-10-01T10:00:01.000Z", serde_json::json!({"role": "user", "content": "hi"})),
        ]);
        let e = install(&no_cwd, &store, "e2", "t").unwrap_err();
        assert!(e.contains("no cwd"), "{e}");
        assert!(install(&sample(&dir), &store, "..\\evil", "t").is_err(), "an id cannot escape the store");
        let conv = store.join("conversations");
        assert!(!conv.join("e1.db").exists() && !conv.join("e2.db").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_replaces_the_db_and_removes_a_stale_wal() {
        let dir = scratch("refresh");
        let store = dir.join("s");
        install(&sample(&dir), &store, "conv-3", "").unwrap();
        let db = store.join("conversations").join("conv-3.db");
        fs::write(with_suffix(&db, "-wal"), b"stale wal from an earlier agy run").unwrap();
        fs::write(with_suffix(&db, "-shm"), b"stale shm").unwrap();
        let small = journal_from(&dir, &[
            serde_json::json!({"type": "session", "id": "x", "timestamp": "2026-10-02T10:00:00.000Z", "cwd": CWD}),
            msg("u", None, "2026-10-02T10:00:01.000Z", serde_json::json!({"role": "user", "content": [{"type": "text", "text": "new content"}]})),
        ]);
        let (db2, _, _) = install(&small, &store, "conv-3", "").unwrap();
        assert_eq!(db2, db);
        assert!(!with_suffix(&db, "-wal").exists(), "an old WAL must never sit next to the new file");
        assert!(!with_suffix(&db, "-shm").exists());
        let left: Vec<_> = fs::read_dir(db.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("conv-3.db")], "no temp or aside files left");
        let (v, _) = read_back(&db);
        assert_eq!(v.len(), 2);
        assert_eq!(v[1]["message"]["content"][0]["text"], "new content");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Windows refuses to rename a file SQLite holds open; that is how a conversation agy
    /// has open shows up. The refresh must fail and leave the old conversation intact.
    #[cfg(windows)]
    #[test]
    fn refuses_to_replace_a_conversation_in_use() {
        let dir = scratch("inuse");
        let store = dir.join("s");
        install(&sample(&dir), &store, "conv-4", "Old").unwrap();
        let db = store.join("conversations").join("conv-4.db");
        let holder = Connection::open(&db).unwrap();
        holder.execute_batch("PRAGMA journal_mode=WAL; SELECT count(*) FROM steps;").unwrap();
        assert!(with_suffix(&db, "-wal").exists());
        let small = journal_from(&dir, &[
            serde_json::json!({"type": "session", "id": "x", "timestamp": "2026-10-02T10:00:00.000Z", "cwd": CWD}),
            msg("u", None, "2026-10-02T10:00:01.000Z", serde_json::json!({"role": "user", "content": "replacement"})),
        ]);
        let e = install(&small, &store, "conv-4", "New").unwrap_err();
        assert!(e.contains("in use"), "{e}");
        let n: i64 = holder.query_row("SELECT count(*) FROM steps", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 5, "the open conversation still has its own steps");
        drop(holder);
        assert!(!store.join("conversations").join("conv-4.db.tmp").exists());
        assert_eq!(rows(&db).len(), 5, "the file on disk is the old conversation");
        let ann = fs::read_to_string(ag::annotation_path(&store, "conv-4")).unwrap();
        assert_eq!(ann, "title:\"Old\"", "a refused refresh does not retitle");
        let _ = fs::remove_dir_all(&dir);
    }

    fn jetbox(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut w = Writer::new();
        for (id, title) in entries {
            let mut s = Writer::new();
            s.string(1, title).varint(2, 5);
            let mut e = Writer::new();
            e.string(1, id).message(2, &s);
            w.message(1, &e);
        }
        w.into_bytes()
    }

    fn summaries(store: &Path, rows: &[(&str, &str)]) {
        let c = Connection::open(store.join("conversation_summaries.db")).unwrap();
        c.execute_batch("CREATE TABLE IF NOT EXISTS `conversation_summaries` (`conversation_id` text,`title` text NOT NULL DEFAULT \"\",`preview` text NOT NULL DEFAULT \"\",PRIMARY KEY (`conversation_id`))").unwrap();
        for (id, t) in rows {
            c.execute("INSERT OR REPLACE INTO conversation_summaries (conversation_id, title) VALUES (?1, ?2)", params![id, t]).unwrap();
        }
    }

    fn summary_ids(store: &Path) -> Vec<String> {
        let c = Connection::open(store.join("conversation_summaries.db")).unwrap();
        let mut st = c.prepare("SELECT conversation_id FROM conversation_summaries ORDER BY 1").unwrap();
        let v = st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        v
    }

    #[test]
    fn a_new_title_drops_agys_stale_cached_summary_only() {
        let dir = scratch("title");
        let store = dir.join("s");
        fs::create_dir_all(store.join("annotations")).unwrap();
        fs::write(ag::annotation_path(&store, "conv-5"), "title:\"Old\"\nlast_user_view_time:{seconds:1792500000 nanos:0}").unwrap();
        fs::write(store.join("jetbox_summaries_proto.pb"), jetbox(&[("other", "Other"), ("conv-5", "Old"), ("third", "T")])).unwrap();
        summaries(&store, &[("other", "Other"), ("conv-5", "Old")]);

        install(&sample(&dir), &store, "conv-5", "Same title").unwrap();
        let ann = fs::read_to_string(ag::annotation_path(&store, "conv-5")).unwrap();
        assert_eq!(ann, "title:\"Same title\"\nlast_user_view_time:{seconds:1792500000 nanos:0}");
        let jet = fs::read(store.join("jetbox_summaries_proto.pb")).unwrap();
        assert_eq!(jet, jetbox(&[("other", "Other"), ("third", "T")]), "only this conversation's entry is removed, the rest byte-exact");
        assert_eq!(summary_ids(&store), vec!["other"]);

        // agy rebuilt the caches with the title we wrote: a refresh with the same title keeps them.
        fs::write(store.join("jetbox_summaries_proto.pb"), jetbox(&[("conv-5", "Same title"), ("other", "Other")])).unwrap();
        summaries(&store, &[("conv-5", "Same title")]);
        install(&sample(&dir), &store, "conv-5", "Same title").unwrap();
        assert_eq!(fs::read(store.join("jetbox_summaries_proto.pb")).unwrap(), jetbox(&[("conv-5", "Same title"), ("other", "Other")]));
        assert_eq!(summary_ids(&store), vec!["conv-5", "other"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retitle_escapes_keeps_view_time_and_drops_only_this_cached_summary() {
        let dir = scratch("retitle");
        let store = dir.join("s");
        install(&sample(&dir), &store, "conv-6", "Old").unwrap();
        fs::write(ag::annotation_path(&store, "conv-6"), "title:\"Old\"\nlast_user_view_time:{seconds:1792500000 nanos:7}").unwrap();
        let others = [("a", "A title"), ("b", "B \"quoted\"")];
        fs::write(store.join("jetbox_summaries_proto.pb"), jetbox(&[others[0], ("conv-6", "Old"), others[1]])).unwrap();
        summaries(&store, &[("a", "A title"), ("conv-6", "Old")]);

        let t = "New \"name\"\nline 2 \\ ação";
        retitle(&store, "conv-6", t).unwrap();
        let ann = fs::read_to_string(ag::annotation_path(&store, "conv-6")).unwrap();
        assert_eq!(ann, "title:\"New \\\"name\\\"\\nline 2 \\\\ ação\"\nlast_user_view_time:{seconds:1792500000 nanos:7}");
        assert_eq!(ag::parse_annotation_title(&ann).as_deref(), Some(t));
        assert_eq!(fs::read(store.join("jetbox_summaries_proto.pb")).unwrap(), jetbox(&others), "other entries byte-identical");
        assert_eq!(summary_ids(&store), vec!["a"]);
        let db = store.join("conversations").join("conv-6.db");
        assert_eq!(read_back(&db).0[0]["title"], t, "from-antigravity reads the new title");

        match retitle(&store, "nope", "x") {
            Err(RetitleError::Other(e)) => assert!(e.contains("unknown conversation"), "{e}"),
            other => panic!("unknown id must be an error, got {other:?}"),
        }
        assert!(!ag::annotation_path(&store, "nope").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A running agy holds its caches open: the rename-over fails, which is "locked"
    /// (exit 2), and the annotations title is already in place.
    #[cfg(windows)]
    #[test]
    fn retitle_reports_a_locked_cache_and_keeps_the_annotation() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = scratch("retitle-locked");
        let store = dir.join("s");
        install(&sample(&dir), &store, "conv-7", "").unwrap();
        let jet = store.join("jetbox_summaries_proto.pb");
        fs::write(&jet, jetbox(&[("conv-7", "Old"), ("x", "X")])).unwrap();
        let holder = fs::OpenOptions::new().read(true).share_mode(1).open(&jet).unwrap();
        match retitle(&store, "conv-7", "Fresh") {
            Err(RetitleError::Locked(n)) => assert!(n[0].contains("locked"), "{n:?}"),
            other => panic!("expected Locked, got {other:?}"),
        }
        drop(holder);
        assert_eq!(fs::read(&jet).unwrap(), jetbox(&[("conv-7", "Old"), ("x", "X")]), "the locked cache is untouched");
        assert_eq!(fs::read_to_string(ag::annotation_path(&store, "conv-7")).unwrap(), "title:\"Fresh\"");
        assert!(!with_suffix(&jet, ".sesync-tmp").exists(), "no temp file left");
        let _ = fs::remove_dir_all(&dir);
    }
}
