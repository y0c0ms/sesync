//! The WebSocket server. A connection is a fixed sequence:
//!   1. plaintext `e2ee_hello` → plaintext `e2ee_ready` (a v2 hello is closed with 4001 instead);
//!   2. encrypted `e2ee_auth` with a paired device token → encrypted `e2ee_authenticated`;
//!   3. encrypted RPC requests, each carrying the device token, answered in order.
//! A peer outside the allowlist is dropped before step 1. A token is checked on every frame, so a
//! revoked device is refused on its next frame.

use super::catalog::Catalog;
use super::devices;
use super::e2ee::{self, Channel};
use super::keys::ServerKeys;
use super::net::{self, Cidr};
use super::rpc::{self, Host};
use super::UNAUTHORIZED_CLOSE;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, Stream, StreamExt};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
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
    pub host: Host,
    pub catalog: Arc<dyn Catalog>,
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

    // 1. Plaintext hello and ready.
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
    send_text(&mut tx, json!({ "type": "e2ee_ready" }).to_string()).await?;
    let channel = Channel::new(&client, &shared.keys.secret);

    // 2 and 3. Encrypted auth, then encrypted RPC.
    let mut authed = false;
    loop {
        let limit = if authed { None } else { Some(HANDSHAKE_TIMEOUT) };
        let Some(frame) = next_text(&mut rx, limit).await? else { return Ok(()) };
        // An undecryptable frame is dropped, not answered: it proves nothing about the peer.
        let Some(plain) = channel.open(&frame) else { continue };
        let msg: Value = match serde_json::from_str(&plain) {
            Ok(value) => value,
            Err(_) => {
                send_sealed(&mut tx, &channel, &rpc::error(&Value::Null, "bad_request", "Invalid JSON")).await?;
                continue;
            }
        };
        let token = msg.get("deviceToken").and_then(Value::as_str).unwrap_or("");

        if !authed {
            if msg.get("type") == Some(&json!("e2ee_auth")) && devices::verify(&shared.dir, token) {
                authed = true;
                eprintln!("bridge: {peer} authenticated");
                send_sealed(&mut tx, &channel, &json!({ "type": "e2ee_authenticated" })).await?;
                continue;
            }
            // The reason is not logged with the token; the peer learns only "unauthorized".
            eprintln!("bridge: {peer} refused at auth");
            send_sealed(&mut tx, &channel, &json!({ "type": "e2ee_error", "error": { "code": "unauthorized" } })).await?;
            return close(&mut tx, Some(UNAUTHORIZED_CLOSE), "unauthorized").await;
        }

        if !devices::verify(&shared.dir, token) {
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            send_sealed(&mut tx, &channel, &rpc::error(&id, "unauthorized", "this device is not paired")).await?;
            eprintln!("bridge: {peer} is no longer paired; closed");
            return close(&mut tx, Some(UNAUTHORIZED_CLOSE), "unauthorized").await;
        }

        // Catalog reads touch the disk; keep them off the reactor.
        let worker = Arc::clone(&shared);
        let reply = tokio::task::spawn_blocking(move || rpc::dispatch(&msg, worker.catalog.as_ref(), &worker.host))
            .await
            .map_err(|e| e.to_string())?;
        send_sealed(&mut tx, &channel, &reply).await?;
    }
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

async fn send_sealed(tx: &mut Tx, channel: &Channel, value: &Value) -> Result<(), String> {
    let frame = channel.seal(&value.to_string())?;
    send_text(tx, frame).await
}

/// Close the socket, with `code` when given. An empty reason is sent as none.
async fn close(tx: &mut Tx, code: Option<u16>, reason: &str) -> Result<(), String> {
    let frame = code.map(|code| CloseFrame { code: CloseCode::from(code), reason: reason.into() });
    tx.send(Message::Close(frame)).await.map_err(|e| e.to_string())
}
