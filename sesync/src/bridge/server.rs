//! The WebSocket server. A connection is a fixed sequence:
//!   1. plaintext `e2ee_hello` → plaintext `e2ee_ready` (a v2 hello is closed with 4001 instead; a v3 hello from a
//!      sesync client gets the hybrid handshake, `hybrid.rs`);
//!   2. encrypted `e2ee_auth` with a paired device token → encrypted `e2ee_authenticated`;
//!   3. encrypted RPC requests, each carrying the device token, answered in order. A request may open a
//!      stream (terminal, tabs, status); the stream's frames are sent on the same connection, through the
//!      one writer task, so frames from streams and replies never interleave mid-frame.
//! A peer outside the allowlist is dropped before step 1. A token is checked on every frame, so a
//! revoked device is refused on its next frame.

use super::devices;
use super::e2ee::{self, Channel, Side};
use super::hybrid;
use super::keys::ServerKeys;
use super::net::{self, Cidr};
use super::rpc::{self, Ctx};
use super::stream::{Conn, Sink};
use super::UNAUTHORIZED_CLOSE;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, Stream, StreamExt};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::WebSocketStream;

/// How long a peer may take to send its hello, and then its auth, before it is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest WebSocket message accepted. Orca's frames are far smaller.
const MAX_MESSAGE_BYTES: usize = 1 << 20;

/// What every connection needs. Immutable after startup, except the device store, which is read
/// from disk on each check.
pub struct Shared {
    /// The bridge's state directory; holds `devices.json`.
    pub dir: PathBuf,
    pub keys: ServerKeys,
    pub allow: Vec<Cidr>,
    pub ctx: Ctx,
}

/// Accept connections until the listener fails repeatedly. Never returns on its own; the caller
/// stops it by dropping the task.
pub async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                if !net::allowed(&shared.allow, peer.ip()) {
                    // Dropping the stream closes it before any protocol bytes are exchanged.
                    eprintln!("bridge: refused {} (outside the allowed networks)", peer.ip());
                    continue;
                }
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    if let Err(e) = connection(stream, peer, shared).await {
                        eprintln!("bridge: connection from {peer} ended: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("bridge: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

type Tx = SplitSink<WebSocketStream<TcpStream>, Message>;

async fn connection(stream: TcpStream, peer: SocketAddr, shared: Arc<Shared>) -> Result<(), String> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let ws = tokio_tungstenite::accept_async_with_config(stream, Some(config)).await
        .map_err(|e| e.to_string())?;
    let (mut tx, mut rx) = ws.split();

    // 1. Plaintext hello and ready. Written before the writer task exists, so nothing else can write.
    let Some(hello) = next_text(&mut rx, Some(HANDSHAKE_TIMEOUT)).await? else { return Ok(()) };
    let hello: Value = serde_json::from_str(&hello).unwrap_or(Value::Null);
    if hello.get("type") != Some(&json!("e2ee_hello")) {
        send_text(&mut tx, json!({ "type": "e2ee_error", "message": "Expected e2ee_hello" }).to_string()).await?;
        return close(&mut tx, None, "").await;
    }
    if hello.get("v").and_then(Value::as_u64) == Some(2) {
        // Orca dials v2 first. A 4001 before ready tells it this host is not v2, and it falls back
        // to the legacy framing this module implements.
        return close(&mut tx, Some(UNAUTHORIZED_CLOSE), "v2 framing is not spoken here").await;
    }
    let Some(client) = hello.get("publicKeyB64").and_then(Value::as_str).and_then(e2ee::parse_client_key) else {
        send_text(&mut tx, json!({ "type": "e2ee_error", "message": "Invalid public key" }).to_string()).await?;
        return close(&mut tx, None, "").await;
    };
    // A sesync client asks for the hybrid (ML-KEM) handshake; the phone sends the legacy hello.
    let channel = if hello.get("v").and_then(Value::as_u64) == Some(hybrid::VERSION) {
        let ek = hello.get("mlkemEncapsulationKeyB64").and_then(Value::as_str).unwrap_or("");
        match hybrid::accept(&shared.keys.secret, &client, ek) {
            Ok((ready, keys)) => {
                send_text(&mut tx, ready.to_string()).await?;
                Arc::new(Channel::hybrid(&keys, Side::Bridge))
            }
            Err(message) => {
                send_text(&mut tx, json!({ "type": "e2ee_error", "message": message }).to_string()).await?;
                return close(&mut tx, None, "").await;
            }
        }
    } else {
        send_text(&mut tx, json!({ "type": "e2ee_ready" }).to_string()).await?;
        Arc::new(Channel::new(&client, &shared.keys.secret))
    };

    // From here on every frame leaves through the writer, so replies and stream frames are ordered.
    let (out_tx, out_rx) = unbounded_channel::<Message>();
    let writer = tokio::spawn(write_frames(tx, out_rx));
    let sink = Sink::new(out_tx, Arc::clone(&channel));
    let conn = Arc::new(Conn::new(sink.clone()));
    let result = serve_requests(&mut rx, &channel, &sink, &conn, &shared, peer).await;
    // The phone's dictation ends with its connection: its audio is dropped, never transcribed later.
    shared.ctx.dictation.cancel_connection(conn.id());
    // Dropping the connection stops its streams; each stream's sender goes with it, so the writer ends.
    drop(conn);
    drop(sink);
    let _ = writer.await;
    result
}

/// Steps 2 and 3: authenticate, then answer requests until the peer leaves.
async fn serve_requests(
    rx: &mut (impl Stream<Item = Result<Message, WsError>> + Unpin),
    channel: &Arc<Channel>,
    sink: &Sink,
    conn: &Arc<Conn>,
    shared: &Arc<Shared>,
    peer: SocketAddr,
) -> Result<(), String> {
    let mut authed = false;
    loop {
        let limit = if authed { None } else { Some(HANDSHAKE_TIMEOUT) };
        let Some(frame) = next_text(rx, limit).await? else { return Ok(()) };
        // An undecryptable frame is dropped, not answered: it proves nothing about the peer.
        let Some(plain) = channel.open(&frame) else { continue };
        let msg: Value = match serde_json::from_str(&plain) {
            Ok(value) => value,
            Err(_) => {
                if !sink.json(&rpc::error(&Value::Null, "bad_request", "Invalid JSON")) {
                    return Ok(());
                }
                continue;
            }
        };
        let token = msg.get("deviceToken").and_then(Value::as_str).unwrap_or("");

        if !authed {
            if msg.get("type") == Some(&json!("e2ee_auth")) && devices::verify(&shared.dir, token) {
                authed = true;
                eprintln!("bridge: {peer} authenticated");
                if !sink.json(&json!({ "type": "e2ee_authenticated" })) {
                    return Ok(());
                }
                continue;
            }
            // The reason is not logged with the token; the peer learns only "unauthorized".
            eprintln!("bridge: {peer} refused at auth");
            sink.json(&json!({ "type": "e2ee_error", "error": { "code": "unauthorized" } }));
            sink.close(UNAUTHORIZED_CLOSE, "unauthorized");
            return Ok(());
        }

        if !devices::verify(&shared.dir, token) {
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            sink.json(&rpc::error(&id, "unauthorized", "this device is not paired"));
            eprintln!("bridge: {peer} is no longer paired; closed");
            sink.close(UNAUTHORIZED_CLOSE, "unauthorized");
            return Ok(());
        }

        // Catalog and herdr reads touch the disk and the socket; keep them off the reactor.
        let ctx = shared.ctx.clone();
        let conn = Arc::clone(conn);
        let reply = tokio::task::spawn_blocking(move || rpc::dispatch(&msg, &ctx, &conn))
            .await
            .map_err(|e| e.to_string())?;
        if let Some(reply) = reply {
            if !sink.json(&reply) {
                return Ok(());
            }
        }
    }
}

/// The only writer to the socket once the handshake is done. Ends when every sender is gone or after
/// a close frame.
async fn write_frames(mut tx: Tx, mut out: UnboundedReceiver<Message>) {
    while let Some(msg) = out.recv().await {
        let closing = matches!(msg, Message::Close(_));
        if tx.send(msg).await.is_err() || closing {
            break;
        }
    }
    let _ = SinkExt::close(&mut tx).await;
}

/// The next text (or UTF-8 binary) message. None when the peer closed the socket. A timeout or a
/// protocol error is an `Err`, which ends the connection.
async fn next_text<S>(rx: &mut S, limit: Option<Duration>) -> Result<Option<String>, String>
where
    S: Stream<Item = Result<Message, WsError>> + Unpin,
{
    loop {
        let next = match limit {
            Some(d) => tokio::time::timeout(d, rx.next()).await
                .map_err(|_| "no frame before the handshake timeout".to_string())?,
            None => rx.next().await,
        };
        match next {
            None | Some(Ok(Message::Close(_))) => return Ok(None),
            Some(Err(e)) => return Err(e.to_string()),
            Some(Ok(Message::Text(text))) => return Ok(Some(text.as_str().to_owned())),
            // Orca's mock reads binary frames as UTF-8 text; do the same.
            Some(Ok(Message::Binary(bytes))) => match String::from_utf8(bytes.to_vec()) {
                Ok(text) => return Ok(Some(text)),
                Err(_) => continue,
            },
            // Pings, pongs and raw frames carry no protocol content.
            Some(Ok(_)) => continue,
        }
    }
}

async fn send_text(tx: &mut Tx, text: String) -> Result<(), String> {
    tx.send(Message::Text(text.into())).await.map_err(|e| e.to_string())
}

/// Close the socket before the writer exists, with `code` when given. An empty reason is sent as none.
async fn close(tx: &mut Tx, code: Option<u16>, reason: &str) -> Result<(), String> {
    let frame = code.map(|code| CloseFrame { code: CloseCode::from(code), reason: reason.into() });
    tx.send(Message::Close(frame)).await.map_err(|e| e.to_string())
}
