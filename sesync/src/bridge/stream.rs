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
//!
//! A terminal stream also carries the pane's history, the rows herdr keeps above the screen, so a client can scroll
//! back. Binary: each snapshot starts with the history (the phone resets its terminal for a snapshot, and the rows
//! above the screen land in its scrollback), and rows that scroll off the screen later are pushed into the
//! terminal's scrollback before the repaint (`push_history`). JSON: `history` events carry the rows, oldest first.

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

/// The history a terminal stream carries: all `pane.read` "recent" gives (herdr keeps 1000 rows of a pane).
pub const HISTORY_ROWS: u32 = 1000;
/// The history rows a stream remembers of what it sent: a later read is searched for them to find the rows that
/// came after. Many, so a run of repeated rows (blank ones, box borders) rarely matches in the wrong place.
const MARK_ROWS: usize = 12;

/// The rows of a herdr read, CR LF or LF separated, without the empty row after a final line break.
pub fn rows_of(text: &str) -> Vec<String> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n').map(|row| row.strip_suffix('\r').unwrap_or(row).to_string()).collect()
}

/// The pane's history with colour codes: its last `lines` rows without the `screen_rows` of its screen, oldest first.
pub fn read_history(h: &dyn Herdr, pane_id: &str, screen_rows: u32, lines: u32) -> Result<Vec<String>, String> {
    let mut rows = rows_of(&herdr::read_recent_ansi(h, pane_id, lines)?);
    rows.truncate(rows.len().saturating_sub(screen_rows as usize));
    Ok(rows)
}

/// Where the history a client has ends: the last rows of it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HistoryMark(Vec<String>);

impl HistoryMark {
    /// The mark at the end of `history`.
    pub fn after(history: &[String]) -> HistoryMark {
        HistoryMark(history[history.len().saturating_sub(MARK_ROWS)..].to_vec())
    }

    /// The rows of `history` after the mark: all of it when the client had no history, None when the mark is not in
    /// it (the read was too short for what scrolled, or herdr's history was cleared or rewrapped). The last place
    /// the mark fits is taken.
    pub fn new_rows<'a>(&self, history: &'a [String]) -> Option<&'a [String]> {
        if self.0.is_empty() {
            return Some(history);
        }
        history.windows(self.0.len()).rposition(|w| w == self.0.as_slice()).map(|at| &history[at + self.0.len()..])
    }
}

/// Bytes that add `history` to a terminal's scrollback, oldest first, and leave its screen blank: the phone's xterm
/// and the terminal `sesync machine attach` runs in. Each batch of up to `rows` rows is drawn from the top of a
/// cleared screen, then line feeds at the bottom row scroll it off into the scrollback. The screen is cleared from the
/// home position down (ED 0), never as a whole (ED 2), which some terminals copy into their scrollback.
pub fn push_history(history: &[String], rows: u32) -> String {
    let rows = rows.max(1);
    let mut out = String::new();
    for batch in history.chunks(rows as usize) {
        out.push_str("\x1b[H\x1b[J");
        for (i, row) in batch.iter().enumerate() {
            if i > 0 {
                out.push_str("\r\n");
            }
            out.push_str(row);
            out.push_str("\x1b[0m");
        }
        out.push_str(&format!("\x1b[{rows};1H"));
        out.push_str(&"\n".repeat(batch.len()));
    }
    out
}

/// A snapshot for a terminal the client has just reset: the history, then the screen. Writing it scrolls the
/// history off into the scrollback and leaves the screen in view.
pub fn snapshot_text(history: &[String], screen: &str) -> String {
    let mut out = String::new();
    for row in history {
        out.push_str(row);
        out.push_str("\x1b[0m\r\n");
    }
    out.push_str(screen);
    out
}

/// `history` as JSON `history` events, each under one frame chunk of text.
pub fn history_events(history: &[String]) -> Vec<Value> {
    let mut events = Vec::new();
    let mut batch: Vec<&str> = Vec::new();
    let mut size = 0;
    for row in history {
        if !batch.is_empty() && size + row.len() > frame::CHUNK_BYTES {
            events.push(json!({ "type": "history", "lines": batch }));
            batch = Vec::new();
            size = 0;
        }
        size += row.len();
        batch.push(row);
    }
    if !batch.is_empty() {
        events.push(json!({ "type": "history", "lines": batch }));
    }
    events
}

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
    /// The history read at subscribe, sent once at the start (empty when the pane has no layout).
    pub history: Vec<String>,
    /// Where the history the client has ends.
    pub mark: HistoryMark,
}

/// The first row of a screen. Rows scroll off the screen only when it changes, so history is looked for only then.
fn first_row(screen: &str) -> &str {
    screen.split("\r\n").next().unwrap_or("")
}

/// The pane's whole history, for a snapshot after a size change: the mark moves to its end. None without a layout.
fn fresh_history(t: &mut Terminal) -> Vec<String> {
    let history = if t.display_mode == DESKTOP {
        read_history(t.herdr.as_ref(), &t.pane_id, t.rows, HISTORY_ROWS).unwrap_or_default()
    } else {
        Vec::new()
    };
    t.mark = HistoryMark::after(&history);
    history
}

/// The history rows that scrolled off the screen since the client's mark, and the mark moved past them. A short read
/// first (three screens); the whole history when the mark is not in it. When the whole history lacks the mark (herdr
/// cleared it, or more scrolled than herdr keeps), all of it is new.
fn scrolled_history(t: &mut Terminal) -> Vec<String> {
    if t.display_mode != DESKTOP {
        return Vec::new();
    }
    for lines in [(t.rows * 3).min(HISTORY_ROWS), HISTORY_ROWS] {
        let Ok(history) = read_history(t.herdr.as_ref(), &t.pane_id, t.rows, lines) else { return Vec::new() };
        // A read shorter than asked holds all herdr keeps: a longer one would find nothing more.
        let whole = history.len() + (t.rows as usize) < lines as usize;
        let fresh = match t.mark.new_rows(&history) {
            Some(fresh) => fresh.to_vec(),
            None if whole || lines == HISTORY_ROWS => history.clone(),
            None => continue,
        };
        t.mark = HistoryMark::after(&history);
        return fresh;
    }
    Vec::new()
}

/// Send `history` as JSON `history` events. False once the connection has gone.
fn send_history(t: &Terminal, history: &[String]) -> bool {
    history_events(history).into_iter().all(|event| t.sink.json(&streamed(&t.request_id, event)))
}

/// The screen a JSON client starts from: a `scrollback` event carrying the whole screen, as `resized` does.
fn send_first_screen(t: &Terminal, screen: &str) -> bool {
    t.sink.json(&streamed(
        &t.request_id,
        json!({ "type": "scrollback", "cols": t.cols, "rows": t.rows, "displayMode": t.display_mode, "serialized": screen }),
    ))
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

/// Send `screen` as the terminal's new size: a resized snapshot in binary mode, led by the pane's `history`, a
/// `resized` event in JSON mode (a JSON client keeps the history it has).
fn send_resized(t: &Terminal, seq: u64, screen: &str, history: &[String]) -> bool {
    if t.binary {
        send_snapshot(t, "resized", seq, &snapshot_text(history, screen))
    } else {
        t.sink.json(&streamed(
            &t.request_id,
            json!({ "type": "resized", "cols": t.cols, "rows": t.rows, "displayMode": t.display_mode, "serialized": screen }),
        ))
    }
}

/// Send the history and the screen the client starts from, then poll the screen until the stream stops, sending a
/// full repaint whenever it changes, led by the rows that scrolled into the pane's history. The pane's size is read
/// every [`SIZE_POLL`]. A change of size sends the tick's screen as a resize and nothing else for that tick, so a
/// size and a screen change never both go out.
pub fn run_terminal(mut t: Terminal) {
    let mut seq = 0u64;
    let history = std::mem::take(&mut t.history);
    let opened = if t.binary {
        send_snapshot(&t, "scrollback", 0, &snapshot_text(&history, &t.shown))
    } else {
        // A JSON client gets the history, then the screen once: without it the client draws nothing until the pane
        // changes, so a new attach stays blank until the next key.
        send_history(&t, &history) && send_first_screen(&t, &t.shown)
    };
    drop(history);
    if !opened {
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
                    let history = fresh_history(&mut t);
                    seq += 1;
                    if !send_resized(&t, seq, &screen, &history) {
                        return;
                    }
                    continue;
                }
            }
        }
        if screen == t.shown {
            continue;
        }
        let scrolled = if first_row(&screen) != first_row(&t.shown) { scrolled_history(&mut t) } else { Vec::new() };
        t.shown = screen;
        let delivered = if t.binary {
            let body = format!("{}{REPAINT}{}", push_history(&scrolled, t.rows), t.shown);
            let mut ok = true;
            for piece in frame::chunks(&body) {
                seq += 1;
                ok = ok && t.sink.binary(&frame::encode(frame::OUTPUT, t.stream_id, seq, piece.as_bytes()));
            }
            ok
        } else {
            let body = format!("{REPAINT}{}", t.shown);
            let mut ok = send_history(&t, &scrolled);
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
            Running::start_with(binary, screen, size, display_mode, &[])
        }

        /// `start`, with `history` above the pane's screen, read as `terminal.subscribe` reads it.
        fn start_with(binary: bool, screen: &str, size: (u32, u32), display_mode: &'static str, history: &[&str]) -> Running {
            let fake = FakeHerdr::new();
            let pane = fake.open_tab("w-main", "stream", "/work", &[]);
            fake.set_pane(&pane.terminal_id, screen, size);
            fake.set_history(&pane.terminal_id, history);
            let history = read_history(fake.as_ref(), &pane.pane_id, size.1, HISTORY_ROWS).unwrap();
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
                mark: HistoryMark::after(&history),
                history,
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
        let resized: Vec<&Sent> = after.iter().filter(|e| matches!(e, Sent::Event(v) if v["type"] == "resized")).collect();
        assert_eq!(resized.len(), 1, "one resized event: {after:?}");
        assert_eq!(after.len(), 2, "the start screen, then the resize; no repaint: {after:?}");
        match resized[0] {
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

    #[test]
    fn a_json_client_gets_the_screen_on_subscribe_without_waiting_for_a_change() {
        let mut s = Running::start(false, "one\ntwo", (57, 27), DESKTOP);
        let initial = s.sent_within(Duration::from_millis(150));
        let screens: Vec<&Value> = initial.iter().filter_map(|m| match m {
            Sent::Event(v) if v["type"] == "scrollback" => Some(v),
            _ => None,
        }).collect();
        assert_eq!(screens.len(), 1, "the screen is sent once on subscribe: {initial:?}");
        assert_eq!(screens[0]["serialized"], "one\r\ntwo", "{initial:?}");
        s.finish();
    }

    fn rows(r: &[&str]) -> Vec<String> {
        r.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_mark_finds_the_rows_after_it_and_none_when_the_history_lost_it() {
        let had = rows(&["a", "b", "c"]);
        let mark = HistoryMark::after(&had);
        assert_eq!(mark.new_rows(&rows(&["a", "b", "c", "d", "e"])), Some(&rows(&["d", "e"])[..]));
        assert_eq!(mark.new_rows(&had), Some(&[][..]), "nothing scrolled");
        assert_eq!(mark.new_rows(&rows(&["b", "c", "d"])), None, "the read began after the mark's first row");
        assert_eq!(mark.new_rows(&rows(&["x"])), None, "cleared");
        assert_eq!(HistoryMark::default().new_rows(&rows(&["x", "y"])), Some(&rows(&["x", "y"])[..]), "a client with no history takes all");
        let long: Vec<String> = (0..30).map(|n| n.to_string()).collect();
        assert_eq!(HistoryMark::after(&long).new_rows(&long[5..]), Some(&[][..]), "only the last rows are the mark");
    }

    #[test]
    fn the_history_leads_the_snapshot_and_rows_that_scroll_off_lead_the_repaint_once() {
        let mut s = Running::start_with(true, "c\nd\ne", (10, 3), DESKTOP, &["a", "b"]);
        let initial = s.sent_within(Duration::from_millis(150));
        assert_eq!(chunk_text(&initial), "a\x1b[0m\r\nb\x1b[0m\r\nc\r\nd\r\ne", "history above the screen: {initial:?}");

        // Two rows scroll off the top.
        s.fake.set_history(&s.terminal, &["a", "b", "c", "d"]);
        s.set("e\nf\ng", (10, 3));
        let out = outputs(&s.sent_within(Duration::from_millis(600))).concat();
        assert_eq!(out, format!("{}{REPAINT}e\r\nf\r\ng", push_history(&rows(&["c", "d"]), 3)));

        // Only the bottom row changes: nothing new in the history.
        s.set("e\nf\nh", (10, 3));
        let out = outputs(&s.sent_within(Duration::from_millis(600))).concat();
        assert_eq!(out, format!("{REPAINT}e\r\nf\r\nh"));
        s.finish();
    }

    #[test]
    fn a_json_stream_sends_its_history_as_events_before_the_screen_they_left() {
        let mut s = Running::start_with(false, "c\nd\ne", (10, 3), DESKTOP, &["a", "b"]);
        let initial = s.sent_within(Duration::from_millis(150));
        let events: Vec<Value> = initial.iter().filter_map(|e| if let Sent::Event(v) = e { Some(v.clone()) } else { None }).collect();
        assert_eq!(events[0], json!({ "type": "history", "lines": ["a", "b"] }), "the history comes first");
        assert_eq!(events[1]["type"], "scrollback", "then the screen, once: {events:?}");

        s.fake.set_history(&s.terminal, &["a", "b", "c"]);
        s.set("d\ne\nf", (10, 3));
        let later = s.sent_within(Duration::from_millis(600));
        let events: Vec<Value> = later.iter().filter_map(|e| if let Sent::Event(v) = e { Some(v.clone()) } else { None }).collect();
        assert_eq!(events[0], json!({ "type": "history", "lines": ["c"] }));
        assert_eq!(events[1]["type"], "data");
        s.finish();
    }
}
