//! Live streams to the phone: one terminal's screen (`terminal.subscribe`), one conversation's tabs
//! (`session.tabs.subscribe`) and the agent statuses (`agentSession.subscribeStatus`).
//!
//! Each stream is a thread that polls herdr on a bounded interval and sends only when something changed.
//! Polling, not events: herdr's `pane.updated` does not fire on output, and `pane.output_matched` fires
//! only on a matching line and takes its pane set when the connection opens (both checked against herdr
//! 0.9.1). A screen is polled every 250 ms and the lists every second.
//!
//! A terminal stream speaks Orca's binary terminal stream when the phone asks for it with
//! `capabilities.terminalBinaryStream`: a JSON `subscribed` reply carrying the stream id, then binary
//! frames tagged with that id (`mobile/src/transport/rpc-client-terminal-binary-frame.ts`). Otherwise it
//! sends the legacy JSON `scrollback` and `data` events. Both carry the same screen.

use super::catalog::{Catalog, Conversation};
use super::e2ee::Channel;
use super::herdr::{self, Herdr};
use super::terminals::{self, Running};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use parking_lot::Mutex;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

/// How often a terminal screen is read.
pub const TERMINAL_POLL: Duration = Duration::from_millis(250);
/// How often the tab and status lists are read.
pub const LIST_POLL: Duration = Duration::from_secs(1);
/// How often the conversation list behind a stream is re-read from disk.
pub const CATALOG_REFRESH: Duration = Duration::from_secs(5);

/// Clear the screen and home the cursor, then the screen: a full repaint, so a change never leaves
/// rows from the previous frame behind.
pub const REPAINT: &str = "\x1b[H\x1b[2J";

/// The binary terminal stream's wire format (`src/shared/terminal-stream-protocol.ts`): a 16-byte
/// little-endian header, then the payload.
pub mod frame {
    /// The first byte of every frame.
    pub const KIND: u8 = 0x74;
    pub const VERSION: u8 = 1;
    pub const HEADER_BYTES: usize = 16;
    /// The largest text payload per frame; Orca's `TERMINAL_STREAM_CHUNK_BYTES`.
    pub const CHUNK_BYTES: usize = 48 * 1024;

    pub const OUTPUT: u8 = 1;
    pub const SNAPSHOT_START: u8 = 2;
    pub const SNAPSHOT_CHUNK: u8 = 3;
    pub const SNAPSHOT_END: u8 = 4;

    /// One frame: kind, version, opcode, a zero byte, the stream id (u32 LE), the sequence number as
    /// two u32 LE halves (high, then low), then the payload.
    pub fn encode(opcode: u8, stream_id: u32, seq: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
        out.push(KIND);
        out.push(VERSION);
        out.push(opcode);
        out.push(0);
        out.extend_from_slice(&stream_id.to_le_bytes());
        out.extend_from_slice(&((seq >> 32) as u32).to_le_bytes());
        out.extend_from_slice(&(seq as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// `text` split into pieces of at most [`CHUNK_BYTES`], each cut on a character boundary.
    pub fn chunks(text: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let mut end = rest.len().min(CHUNK_BYTES);
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            let (head, tail) = rest.split_at(end);
            out.push(head);
            rest = tail;
        }
        out
    }
}

/// A screen as the phone's terminal shows it: every line break is CR LF, as a terminal expects.
pub fn screen_text(raw: &str) -> String {
    raw.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// The sending half of one connection: every frame goes through here, sealed, to the writer task.
#[derive(Clone)]
pub struct Sink {
    tx: UnboundedSender<Message>,
    channel: Arc<Channel>,
}

impl Sink {
    pub fn new(tx: UnboundedSender<Message>, channel: Arc<Channel>) -> Sink {
        Sink { tx, channel }
    }

    /// Seal one JSON value into an encrypted text frame. False once the connection has gone.
    pub fn json(&self, value: &Value) -> bool {
        match self.channel.seal(&value.to_string()) {
            Ok(frame) => self.tx.send(Message::Text(frame.into())).is_ok(),
            Err(_) => false,
        }
    }

    /// Seal raw bytes into an encrypted binary frame. False once the connection has gone.
    pub fn binary(&self, bytes: &[u8]) -> bool {
        match self.channel.seal_bytes(bytes) {
            Ok(frame) => self.tx.send(Message::Binary(frame.into())).is_ok(),
            Err(_) => false,
        }
    }

    /// Send a close frame with `code` and `reason`. The writer stops after it.
    pub fn close(&self, code: u16, reason: &str) -> bool {
        let frame = CloseFrame { code: CloseCode::from(code), reason: reason.into() };
        self.tx.send(Message::Close(Some(frame))).is_ok()
    }
}

/// A streamed reply: the request's `id` with `streaming:true`, and the event as `result`. The phone
/// matches later events to the request by `id`.
pub fn streamed(id: &Value, result: Value) -> Value {
    json!({ "id": id, "ok": true, "streaming": true, "result": result, "_meta": { "runtimeId": super::rpc::RUNTIME_ID } })
}

/// The event that ends a stream.
pub fn end(id: &Value) -> Value {
    streamed(id, json!({ "type": "end" }))
}

struct Open {
    key: String,
    request_id: Value,
    stop: Arc<AtomicBool>,
}

/// One connection's open streams. Dropping it stops them all: a stream never outlives the phone
/// that asked for it.
pub struct Conn {
    pub sink: Sink,
    streams: Mutex<Vec<Open>>,
    next_stream: AtomicU32,
}

impl Conn {
    pub fn new(sink: Sink) -> Conn {
        Conn { sink, streams: Mutex::new(Vec::new()), next_stream: AtomicU32::new(0) }
    }

    /// The next binary stream id on this connection, starting at 1.
    pub fn next_stream_id(&self) -> u32 {
        self.next_stream.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Record a stream under `key` and return its stop flag. A stream already open under `key` is
    /// stopped, so a repeated subscribe replaces the old one.
    pub fn register(&self, key: String, request_id: Value) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut list = self.streams.lock();
        list.retain(|o| {
            if o.key == key {
                o.stop.store(true, Ordering::Relaxed);
                false
            } else {
                true
            }
        });
        list.push(Open { key, request_id, stop: Arc::clone(&stop) });
        stop
    }

    /// Stop the streams named by `key` or started by `request_id`. False when none matched.
    pub fn stop(&self, key: Option<&str>, request_id: Option<&Value>) -> bool {
        let mut found = false;
        self.streams.lock().retain(|o| {
            let hit = key.is_some_and(|k| o.key == k) || request_id.is_some_and(|r| &o.request_id == r);
            if hit {
                o.stop.store(true, Ordering::Relaxed);
                found = true;
            }
            !hit
        });
        found
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        for o in self.streams.lock().iter() {
            o.stop.store(true, Ordering::Relaxed);
        }
    }
}


/// The terminal stream's state: the pane it reads and the screen the phone last received.
pub struct Terminal {
    pub herdr: Arc<dyn Herdr>,
    pub sink: Sink,
    pub request_id: Value,
    pub terminal_id: String,
    pub pane_id: String,
    /// The binary stream id the subscribe reply announced; unused in JSON mode.
    pub stream_id: u32,
    pub binary: bool,
    pub cols: u32,
    pub rows: u32,
    pub stop: Arc<AtomicBool>,
    /// The screen already sent (in the subscribe reply, or as the binary snapshot).
    pub shown: String,
}

/// Send the binary snapshot of `shown`, then poll the screen until the stream stops, sending a full
/// repaint whenever it changes.
pub fn run_terminal(mut t: Terminal) {
    let mut seq = 0u64;
    if t.binary {
        let meta = json!({
            "kind": "scrollback", "cols": t.cols, "rows": t.rows, "displayMode": "auto",
            "truncated": false, "truncatedByByteBudget": false, "seq": 0,
        });
        let mut ok = t.sink.binary(&frame::encode(frame::SNAPSHOT_START, t.stream_id, 0, meta.to_string().as_bytes()));
        for piece in frame::chunks(&t.shown) {
            ok = ok && t.sink.binary(&frame::encode(frame::SNAPSHOT_CHUNK, t.stream_id, 0, piece.as_bytes()));
        }
        if !ok || !t.sink.binary(&frame::encode(frame::SNAPSHOT_END, t.stream_id, 0, b"")) {
            return;
        }
    }
    loop {
        thread::sleep(TERMINAL_POLL);
        if t.stop.load(Ordering::Relaxed) {
            break;
        }
        let raw = match herdr::read_visible(t.herdr.as_ref(), &t.pane_id) {
            Ok(raw) => raw,
            Err(_) => match find_pane(t.herdr.as_ref(), &t.terminal_id) {
                Some(pane_id) => {
                    t.pane_id = pane_id;
                    continue;
                }
                // The terminal is gone: the stream ends, as Orca's does.
                None => break,
            },
        };
        let screen = screen_text(&raw);
        if screen == t.shown {
            continue;
        }
        let body = format!("{REPAINT}{screen}");
        t.shown = screen;
        let delivered = if t.binary {
            let mut ok = true;
            for piece in frame::chunks(&body) {
                seq += 1;
                ok = ok && t.sink.binary(&frame::encode(frame::OUTPUT, t.stream_id, seq, piece.as_bytes()));
            }
            ok
        } else {
            let mut ok = true;
            for piece in frame::chunks(&body) {
                ok = ok && t.sink.json(&streamed(&t.request_id, json!({ "type": "data", "chunk": piece })));
            }
            ok
        };
        if !delivered {
            return;
        }
    }
    t.sink.json(&end(&t.request_id));
}

/// The pane herdr now has for `terminal_id`, if any.
fn find_pane(h: &dyn Herdr, terminal_id: &str) -> Option<String> {
    herdr::list_panes(h).ok()?.into_iter().find(|p| p.terminal_id == terminal_id).map(|p| p.pane_id)
}

/// A conversation's tab stream. `last` is the tab list the phone already has.
pub struct Tabs {
    pub herdr: Arc<dyn Herdr>,
    pub catalog: Arc<dyn Catalog>,
    pub sink: Sink,
    pub request_id: Value,
    /// The worktree id, without the `id:` prefix.
    pub worktree: String,
    pub stop: Arc<AtomicBool>,
    pub last: Value,
}

/// The tab list's comparable part: everything that changes what the phone shows. The version and
/// the epoch are left out, so an unchanged list is not resent.
pub fn tabs_key(snapshot: &Value) -> Value {
    json!([snapshot["tabs"], snapshot["activeTabId"], snapshot["activeTabType"]])
}

pub fn run_tabs(mut t: Tabs) {
    let mut convs: Vec<Conversation> = Vec::new();
    let mut refreshed = Instant::now();
    loop {
        thread::sleep(LIST_POLL);
        if t.stop.load(Ordering::Relaxed) {
            break;
        }
        if convs.is_empty() || refreshed.elapsed() >= CATALOG_REFRESH {
            if let Ok(list) = t.catalog.conversations() {
                convs = list;
            }
            refreshed = Instant::now();
        }
        let Some(c) = convs.iter().find(|c| c.worktree_id() == t.worktree) else { break };
        let Ok(panes) = terminals::running_panes(t.herdr.as_ref(), &convs) else { continue };
        let mine = terminals::panes_of(&panes, c);
        let mut snapshot = terminals::tabs_result(
            &t.worktree,
            c,
            &mine,
            terminals::publication_epoch(),
            terminals::next_version(),
        );
        let key = tabs_key(&snapshot);
        if key == tabs_key(&t.last) {
            continue;
        }
        t.last = snapshot.clone();
        snapshot["type"] = json!("updated");
        if !t.sink.json(&streamed(&t.request_id, snapshot)) {
            return;
        }
    }
    t.sink.json(&end(&t.request_id));
}

/// The agent-session summaries for the panes that run a conversation. Each carries its pane's id,
/// so a stream can tell sessions apart.
pub fn sessions(panes: &[Running], conversations: &[Conversation], now: f64) -> Vec<Value> {
    panes
        .iter()
        .filter_map(|r| terminals::conversation_of(r, conversations).map(|c| terminals::agent_session(r, c, now)))
        .collect()
}

/// A session summary without its clock, the part that decides whether a status event is due.
pub fn without_clock(mut session: Value) -> Value {
    if let Some(object) = session.as_object_mut() {
        object.remove("updatedAt");
    }
    session
}

/// The agent-status stream. `last` maps each session id to the summary the phone already has.
pub struct Status {
    pub herdr: Arc<dyn Herdr>,
    pub catalog: Arc<dyn Catalog>,
    pub sink: Sink,
    pub request_id: Value,
    pub stop: Arc<AtomicBool>,
    pub last: HashMap<String, Value>,
}

pub fn run_status(mut t: Status) {
    let mut convs: Vec<Conversation> = Vec::new();
    let mut refreshed = Instant::now();
    loop {
        thread::sleep(LIST_POLL);
        if t.stop.load(Ordering::Relaxed) {
            break;
        }
        if convs.is_empty() || refreshed.elapsed() >= CATALOG_REFRESH {
            if let Ok(list) = t.catalog.conversations() {
                convs = list;
            }
            refreshed = Instant::now();
        }
        let Ok(panes) = terminals::running_panes(t.herdr.as_ref(), &convs) else { continue };
        for session in sessions(&panes, &convs, terminals::now_ms()) {
            let id = session["sessionId"].as_str().unwrap_or("").to_string();
            let key = without_clock(session.clone());
            if t.last.get(&id).map(|prev| without_clock(prev.clone())) == Some(key.clone()) {
                continue;
            }
            t.last.insert(id, session.clone());
            if !t.sink.json(&streamed(&t.request_id, json!({ "type": "status", "session": session }))) {
                return;
            }
        }
    }
    t.sink.json(&end(&t.request_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_has_the_header_orca_reads() {
        let bytes = frame::encode(frame::OUTPUT, 1, 1, b"hello");
        assert_eq!(bytes.len(), 21);
        assert_eq!(&bytes[..4], &[0x74, 0x01, 0x01, 0x00]);
        assert_eq!(&bytes[4..8], &1u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &0u32.to_le_bytes(), "high half of the sequence");
        assert_eq!(&bytes[12..16], &1u32.to_le_bytes(), "low half of the sequence");
        assert_eq!(&bytes[16..], b"hello");
    }

    #[test]
    fn a_sequence_number_splits_into_two_halves() {
        let bytes = frame::encode(frame::OUTPUT, 9, (3u64 << 32) | 7, b"");
        assert_eq!(&bytes[8..12], &3u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &7u32.to_le_bytes());
    }

    #[test]
    fn text_is_chunked_on_character_boundaries_within_the_limit() {
        assert!(frame::chunks("").is_empty());
        assert_eq!(frame::chunks("abc"), vec!["abc"]);
        let text = "é".repeat(frame::CHUNK_BYTES);
        let pieces = frame::chunks(&text);
        assert!(pieces.len() >= 2);
        assert!(pieces.iter().all(|p| p.len() <= frame::CHUNK_BYTES));
        assert_eq!(pieces.concat(), text, "no byte is lost or split");
    }

    #[test]
    fn a_screen_uses_crlf_line_endings_once() {
        assert_eq!(screen_text("a\nb"), "a\r\nb");
        assert_eq!(screen_text("a\r\nb\n"), "a\r\nb\r\n");
        assert_eq!(screen_text(""), "");
    }

    #[test]
    fn a_status_summary_compares_without_its_clock() {
        let a = json!({"sessionId": "t", "status": "idle", "updatedAt": 1.0});
        let b = json!({"sessionId": "t", "status": "idle", "updatedAt": 2.0});
        assert_eq!(without_clock(a), without_clock(b));
    }
    #[test]
    fn a_tab_list_compares_without_its_version_so_an_unchanged_list_is_not_resent() {
        let a = json!({"tabs": [{"id": "t"}], "activeTabId": "t", "activeTabType": "terminal", "snapshotVersion": 1});
        let b = json!({"tabs": [{"id": "t"}], "activeTabId": "t", "activeTabType": "terminal", "snapshotVersion": 2});
        assert_eq!(tabs_key(&a), tabs_key(&b));
        let gone = json!({"tabs": [], "activeTabId": null, "activeTabType": null, "snapshotVersion": 3});
        assert_ne!(tabs_key(&a), tabs_key(&gone));
    }

    #[test]
    fn a_stream_is_found_by_its_key_or_its_request_and_stops_when_dropped() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let channel = Arc::new(crate::bridge::e2ee::Channel::new(
            &crypto_box::SecretKey::from_slice(&[1u8; 32]).unwrap().public_key(),
            &crypto_box::SecretKey::from_slice(&[2u8; 32]).unwrap(),
        ));
        let conn = Conn::new(Sink::new(tx, channel));
        assert_eq!(conn.next_stream_id(), 1);
        assert_eq!(conn.next_stream_id(), 2);
        let first = conn.register("term_a".into(), json!("r1"));
        let second = conn.register("term_a".into(), json!("r2"));
        assert!(first.load(Ordering::Relaxed), "a repeated key replaces the old stream");
        assert!(!second.load(Ordering::Relaxed));
        assert!(conn.stop(None, Some(&json!("r2"))));
        assert!(second.load(Ordering::Relaxed));
        assert!(!conn.stop(Some("missing"), None));
        let kept = conn.register("status".into(), json!("r3"));
        drop(conn);
        assert!(kept.load(Ordering::Relaxed), "dropping the connection stops its streams");
    }
}
