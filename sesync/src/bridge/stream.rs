//! Live streams to the phone: one terminal's screen (`terminal.subscribe`), one conversation's tabs
//! (`session.tabs.subscribe`) and the agent statuses (`agentSession.subscribeStatus`).
//!
//! Each stream is a thread that polls herdr on a bounded interval and sends only when something changed.
//! Polling, not events: herdr's `pane.updated` does not fire on output, and `pane.output_matched` fires
//! only on a matching line and takes its pane set when the connection opens (both checked against herdr
//! 0.9.1). A screen is polled every 250 ms, a terminal's pane size every second, and the lists every second.
//!
//! A terminal stream speaks Orca's binary terminal stream when the phone asks for it with
//! `capabilities.terminalBinaryStream`: a JSON `subscribed` reply carrying the stream id, then binary
//! frames tagged with that id (`mobile/src/transport/rpc-client-terminal-binary-frame.ts`). Otherwise it
//! sends the legacy JSON `scrollback` and `data` events. Both carry the same screen.

use super::catalog::{Catalog, Conversation};
use super::e2ee::Channel;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use super::herdr::{self, Herdr};
use super::terminals::{self, Running};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
/// How often a terminal's pane size is read. A resize reaches the phone within about this long.
pub const SIZE_POLL: Duration = Duration::from_secs(1);
/// The display mode of a terminal at its pane's real size.
pub const DESKTOP: &str = "desktop";
/// The display mode of a terminal the bridge could not size: the phone's viewport is announced, as before.
pub const AUTO: &str = "auto";
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

    /// Seal one JSON value into an encrypted text frame. False once the connection has gone. The frame is queued
    /// while the channel's counter is held, so frames from several streams leave in the order they were sealed.
    pub fn json(&self, value: &Value) -> bool {
        self.channel
            .seal_then(value.to_string().as_bytes(), |frame| self.tx.send(Message::Text(STANDARD.encode(frame).into())).is_ok())
            .unwrap_or(false)
    }

    /// Seal raw bytes into an encrypted binary frame. False once the connection has gone.
    pub fn binary(&self, bytes: &[u8]) -> bool {
        self.channel.seal_then(bytes, |frame| self.tx.send(Message::Binary(frame.into())).is_ok()).unwrap_or(false)
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
/// that asked for it. Its id is unique in the process, so a request can say which phone sent it.
pub struct Conn {
    pub sink: Sink,
    id: u64,
    streams: Mutex<Vec<Open>>,
    next_stream: AtomicU32,
}

/// Connections opened in this process, numbered from 1.
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

impl Conn {
    pub fn new(sink: Sink) -> Conn {
        let id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);
        Conn { sink, id, streams: Mutex::new(Vec::new()), next_stream: AtomicU32::new(0) }
    }

    /// This connection's id, unique in the process.
    pub fn id(&self) -> u64 {
        self.id
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
    /// The display mode announced with the size: `desktop` for the pane's real size, `auto` for the phone's viewport.
    pub display_mode: &'static str,
    pub stop: Arc<AtomicBool>,
    /// The screen already sent (in the subscribe reply, or as the binary snapshot).
    pub shown: String,
}

/// Send `screen` as a binary snapshot of `kind` (`scrollback` for a subscribe, `resized` for a size change):
/// the start with its meta, the chunks, the end. False once the connection has gone.
fn send_snapshot(t: &Terminal, kind: &str, seq: u64, screen: &str) -> bool {
    let meta = json!({
        "kind": kind, "cols": t.cols, "rows": t.rows, "displayMode": t.display_mode,
        "truncated": false, "truncatedByByteBudget": false, "seq": seq,
    });
    let mut ok = t.sink.binary(&frame::encode(frame::SNAPSHOT_START, t.stream_id, seq, meta.to_string().as_bytes()));
    for piece in frame::chunks(screen) {
        ok = ok && t.sink.binary(&frame::encode(frame::SNAPSHOT_CHUNK, t.stream_id, seq, piece.as_bytes()));
    }
    ok && t.sink.binary(&frame::encode(frame::SNAPSHOT_END, t.stream_id, seq, b""))
}

/// Send `screen` as the terminal's new size: a resized snapshot in binary mode, a `resized` event in JSON mode.
fn send_resized(t: &Terminal, seq: u64, screen: &str) -> bool {
    if t.binary {
        send_snapshot(t, "resized", seq, screen)
    } else {
        t.sink.json(&streamed(
            &t.request_id,
            json!({ "type": "resized", "cols": t.cols, "rows": t.rows, "displayMode": t.display_mode, "serialized": screen }),
        ))
    }
}

/// Send the binary snapshot of `shown`, then poll the screen until the stream stops, sending a full
/// repaint whenever it changes. The pane's size is read every [`SIZE_POLL`]. A change of size sends the
/// tick's screen as a resize and nothing else for that tick, so a size and a screen change never both go out.
pub fn run_terminal(mut t: Terminal) {
    let mut seq = 0u64;
    if t.binary && !send_snapshot(&t, "scrollback", 0, &t.shown) {
        return;
    }
    let mut size_checked = Instant::now();
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
        if size_checked.elapsed() >= SIZE_POLL {
            size_checked = Instant::now();
            if let Some((cols, rows)) = herdr::pane_size(t.herdr.as_ref(), &t.pane_id) {
                if (cols, rows) != (t.cols, t.rows) || t.display_mode != DESKTOP {
                    t.cols = cols;
                    t.rows = rows;
                    t.display_mode = DESKTOP;
                    t.shown = screen.clone();
                    seq += 1;
                    if !send_resized(&t, seq, &screen) {
                        return;
                    }
                    continue;
                }
            }
        }
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
    use crate::bridge::e2ee::Channel;
    use crate::bridge::testkit::FakeHerdr;
    use crypto_box::aead::Aead;
    use crypto_box::{Nonce, SalsaBox, SecretKey};
    use tokio::sync::mpsc::UnboundedReceiver;

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

    /// The channel a stream seals with, and the box that opens its binary frames: one shared key, both ways.
    fn keys() -> (Arc<Channel>, SalsaBox) {
        let client = SecretKey::from_slice(&[1u8; 32]).unwrap();
        let server = SecretKey::from_slice(&[2u8; 32]).unwrap();
        (Arc::new(Channel::new(&client.public_key(), &server)), SalsaBox::new(&server.public_key(), &client))
    }

    /// One message a stream sent, opened: a JSON event's `result`, or a binary frame's opcode, seq and payload.
    #[derive(Debug)]
    enum Sent {
        Event(Value),
        Frame { opcode: u8, seq: u64, payload: Vec<u8> },
    }

    fn open(rx: &mut UnboundedReceiver<Message>, channel: &Channel, boxed: &SalsaBox) -> Vec<Sent> {
        let mut sent = Vec::new();
        while let Ok(message) = rx.try_recv() {
            match message {
                Message::Text(text) => {
                    let plain = channel.open(text.as_str()).expect("a sealed JSON frame");
                    let event: Value = serde_json::from_str(&plain).unwrap();
                    sent.push(Sent::Event(event["result"].clone()));
                }
                Message::Binary(bytes) => {
                    let (nonce, body) = bytes.split_at(24);
                    let frame = boxed.decrypt(Nonce::from_slice(nonce), body).expect("a sealed binary frame");
                    let word = |at: usize| u32::from_le_bytes(frame[at..at + 4].try_into().unwrap());
                    let seq = (u64::from(word(8)) << 32) | u64::from(word(12));
                    sent.push(Sent::Frame { opcode: frame[2], seq, payload: frame[frame::HEADER_BYTES..].to_vec() });
                }
                _ => {}
            }
        }
        sent
    }

    /// The meta of each snapshot start in `sent`.
    fn starts(sent: &[Sent]) -> Vec<Value> {
        sent.iter()
            .filter_map(|s| match s {
                Sent::Frame { opcode: frame::SNAPSHOT_START, payload, .. } => Some(serde_json::from_slice(payload).unwrap()),
                _ => None,
            })
            .collect()
    }

    /// The snapshot chunks in `sent`, joined.
    fn chunk_text(sent: &[Sent]) -> String {
        sent.iter()
            .filter_map(|s| match s {
                Sent::Frame { opcode: frame::SNAPSHOT_CHUNK, payload, .. } => Some(String::from_utf8(payload.clone()).unwrap()),
                _ => None,
            })
            .collect()
    }

    /// The output frames in `sent`, one text each.
    fn outputs(sent: &[Sent]) -> Vec<String> {
        sent.iter()
            .filter_map(|s| match s {
                Sent::Frame { opcode: frame::OUTPUT, payload, .. } => Some(String::from_utf8(payload.clone()).unwrap()),
                _ => None,
            })
            .collect()
    }

    /// A terminal stream running over a fake herdr: its pane, and the phone's end of the channel.
    struct Running {
        fake: Arc<FakeHerdr>,
        terminal: String,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
        rx: UnboundedReceiver<Message>,
        channel: Arc<Channel>,
        boxed: SalsaBox,
    }

    impl Running {
        fn start(binary: bool, screen: &str, size: (u32, u32), display_mode: &'static str) -> Running {
            let fake = FakeHerdr::new();
            let pane = fake.open_tab("w-main", "stream", "/work", &[]);
            fake.set_pane(&pane.terminal_id, screen, size);
            let (channel, boxed) = keys();
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let stop = Arc::new(AtomicBool::new(false));
            let t = Terminal {
                herdr: fake.clone(),
                sink: Sink::new(tx, Arc::clone(&channel)),
                request_id: json!("r1"),
                terminal_id: pane.terminal_id.clone(),
                pane_id: pane.pane_id.clone(),
                stream_id: 1,
                binary,
                cols: size.0,
                rows: size.1,
                display_mode,
                stop: Arc::clone(&stop),
                shown: screen_text(screen),
            };
            let thread = thread::spawn(move || run_terminal(t));
            Running { fake, terminal: pane.terminal_id, stop, thread: Some(thread), rx, channel, boxed }
        }

        /// Set the pane's screen and size together.
        fn set(&self, screen: &str, size: (u32, u32)) {
            self.fake.set_pane(&self.terminal, screen, size);
        }

        /// What the stream sent during the next `wait`.
        fn sent_within(&mut self, wait: Duration) -> Vec<Sent> {
            thread::sleep(wait);
            open(&mut self.rx, &self.channel, &self.boxed)
        }

        /// Stop the stream, wait for it, and return what it sent last (its end).
        fn finish(&mut self) -> Vec<Sent> {
            self.stop.store(true, Ordering::Relaxed);
            self.thread.take().unwrap().join().unwrap();
            open(&mut self.rx, &self.channel, &self.boxed)
        }
    }

    #[test]
    fn a_size_change_sends_one_resized_snapshot_carrying_the_current_screen() {
        let mut s = Running::start(true, "one\ntwo", (57, 27), DESKTOP);
        let initial = s.sent_within(Duration::from_millis(100));
        assert_eq!(starts(&initial).len(), 1, "the subscribe's snapshot: {initial:?}");
        assert_eq!(starts(&initial)[0]["kind"], "scrollback");
        assert_eq!(starts(&initial)[0]["cols"], 57);

        // Only the size changes: one resized snapshot carrying the screen, and no repaint.
        s.set("one\ntwo", (40, 20));
        let resized = s.sent_within(Duration::from_millis(1800));
        let metas = starts(&resized);
        assert_eq!(metas.len(), 1, "one resized snapshot: {resized:?}");
        assert_eq!(metas[0]["kind"], "resized");
        assert_eq!((metas[0]["cols"].as_u64(), metas[0]["rows"].as_u64()), (Some(40), Some(20)));
        assert_eq!(metas[0]["displayMode"], "desktop");
        assert_eq!(chunk_text(&resized), "one\r\ntwo");
        assert!(outputs(&resized).is_empty(), "a size change sends no repaint: {resized:?}");
        assert!(resized.iter().all(|m| matches!(m, Sent::Frame { seq: 1, .. })), "the resize takes the next sequence: {resized:?}");

        // Only the screen changes: a full repaint, and no resize.
        s.set("one\ntwo\nthree", (40, 20));
        let repaint = s.sent_within(Duration::from_millis(600));
        assert_eq!(outputs(&repaint), vec![format!("{REPAINT}one\r\ntwo\r\nthree")]);
        assert!(starts(&repaint).is_empty(), "a screen change alone is a repaint, not a resize");

        let quiet = s.sent_within(Duration::from_millis(1800));
        assert!(quiet.is_empty(), "an unchanged size and screen send nothing: {quiet:?}");

        // Both change: one resized snapshot with the new screen. The screen may have gone out as a repaint in
        // an earlier tick, when the stream read it before the size; the size is read once per second, so a tick
        // sends either the repaint or the resize, never both.
        s.set("one\ntwo\nthree\nfour", (30, 10));
        let both = s.sent_within(Duration::from_millis(1800));
        let metas = starts(&both);
        assert_eq!(metas.len(), 1, "one resized snapshot for the size change: {both:?}");
        assert_eq!((metas[0]["cols"].as_u64(), metas[0]["rows"].as_u64()), (Some(30), Some(10)));
        assert_eq!(chunk_text(&both), "one\r\ntwo\r\nthree\r\nfour", "the resize carries the current screen");
        assert!(outputs(&both).len() <= 1, "at most the repaint of the tick before the size was read: {both:?}");

        let end = s.finish();
        assert!(matches!(end.as_slice(), [Sent::Event(e)] if e["type"] == "end"), "{end:?}");
    }

    #[test]
    fn a_json_stream_sends_a_resized_event_with_the_new_size_and_screen() {
        let mut s = Running::start(false, "one\ntwo", (57, 27), DESKTOP);
        s.set("one\ntwo", (40, 20));
        let after = s.sent_within(Duration::from_millis(1800));
        assert_eq!(after.len(), 1, "one resized event and no repaint: {after:?}");
        match &after[0] {
            Sent::Event(e) => {
                assert_eq!(e["type"], "resized");
                assert_eq!((e["cols"].as_u64(), e["rows"].as_u64()), (Some(40), Some(20)));
                assert_eq!(e["displayMode"], "desktop");
                assert_eq!(e["serialized"], "one\r\ntwo");
            }
            other => panic!("a JSON stream sends events: {other:?}"),
        }
        s.finish();
    }

    #[test]
    fn a_pane_herdr_cannot_size_is_not_resized() {
        let mut s = Running::start(true, "one", (80, 24), AUTO);
        s.fake.hide_layout(&s.terminal);
        let initial = s.sent_within(Duration::from_millis(100));
        assert_eq!(starts(&initial)[0]["displayMode"], "auto", "{initial:?}");
        let later = s.sent_within(Duration::from_millis(1800));
        assert!(later.is_empty(), "no layout, no resize: {later:?}");
        s.finish();
    }
}
