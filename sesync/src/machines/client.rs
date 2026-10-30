//! A client of another machine's phone bridge: the hybrid hello (`bridge::hybrid`, X25519 + ML-KEM-768, AES-256-GCM
//! frames), the sealed auth with the paired device token, then sealed RPCs that each carry the token. The RPCs are
//! the phone's (docs/BRIDGE.md), so a machine sees what the phone sees. A bridge that answers with the phone's
//! legacy channel is refused: there is no fallback to classical-only keys.

use super::store::Machine;
use crate::bridge::e2ee::{self, Channel, Side};
use crate::bridge::hybrid;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long the connection and each handshake step may take.
const STEP: Duration = Duration::from_secs(10);
/// How long a plain request waits for its reply.
const REPLY: Duration = Duration::from_secs(20);

/// The sending half: requests, sealed, each with the device token.
pub struct Writer {
    tx: SplitSink<Ws, Message>,
    channel: Arc<Channel>,
    token: String,
    next: u64,
}

/// The receiving half: every message the bridge sends, opened.
pub struct Reader {
    rx: SplitStream<Ws>,
    channel: Arc<Channel>,
}

/// Connect to `machine` and authenticate.
pub async fn connect(machine: &Machine) -> Result<(Writer, Reader), String> {
    let server = e2ee::parse_client_key(&machine.public_key_b64)
        .ok_or_else(|| format!("{}: the saved public key is damaged; add the machine again", machine.name))?;
    let (ws, _) = tokio::time::timeout(STEP, tokio_tungstenite::connect_async(machine.endpoint.as_str()))
        .await
        .map_err(|_| format!("{} did not answer at {}", machine.name, machine.endpoint))?
        .map_err(|e| format!("cannot reach {} at {}: {e}", machine.name, machine.endpoint))?;
    let (mut tx, mut rx) = ws.split();

    let (hello, pending) = hybrid::begin()?;
    tx.send(Message::Text(hello.to_string().into())).await.map_err(|e| e.to_string())?;
    let ready = next_text(&mut rx, Some(STEP))
        .await?
        .ok_or_else(|| format!("{} closed the connection during the hello", machine.name))?;
    let ready: Value = serde_json::from_str(&ready).map_err(|_| format!("{} answered the hello with {ready}", machine.name))?;
    let keys = hybrid::finish(pending, &server, &ready).map_err(|e| format!("{}: {e}", machine.name))?;

    let channel = Arc::new(Channel::hybrid(&keys, Side::Client));
    let mut writer = Writer { tx, channel: Arc::clone(&channel), token: machine.device_token.clone(), next: 0 };
    let mut reader = Reader { rx, channel };
    writer.sealed(&json!({ "type": "e2ee_auth", "deviceToken": writer.token })).await?;
    match tokio::time::timeout(STEP, reader.next()).await {
        Ok(Ok(Some(reply))) if reply["type"] == "e2ee_authenticated" => Ok((writer, reader)),
        Ok(Ok(Some(_))) | Ok(Ok(None)) => Err(format!(
            "{} refused this machine's pairing (revoked there?); pair again with `sesync bridge pair` on it",
            machine.name
        )),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!("{} did not confirm the pairing in time", machine.name)),
    }
}

impl Writer {
    async fn sealed(&mut self, value: &Value) -> Result<(), String> {
        let frame = self.channel.seal(&value.to_string())?;
        self.tx.send(Message::Text(frame.into())).await.map_err(|e| format!("the connection broke: {e}"))
    }

    /// Send a request; returns its id, which its reply (or its stream's frames) carries.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<String, String> {
        self.next += 1;
        let id = format!("m{}", self.next);
        self.sealed(&json!({ "id": id, "deviceToken": self.token, "method": method, "params": params })).await?;
        Ok(id)
    }
}

impl Reader {
    /// The next message, opened and parsed. None when the bridge closed the connection. A frame that does not
    /// open under this connection's key is skipped, as the bridge skips such frames.
    pub async fn next(&mut self) -> Result<Option<Value>, String> {
        loop {
            let Some(frame) = next_text(&mut self.rx, None).await? else { return Ok(None) };
            if let Some(plain) = self.channel.open(&frame) {
                return serde_json::from_str(&plain).map(Some).map_err(|e| format!("a reply is not JSON: {e}"));
            }
        }
    }
}

/// Send one request and wait for its reply; the result on success, `code: message` on a refusal. Messages of
/// other requests that arrive meanwhile are dropped, so use this only while no stream is open.
pub async fn call(writer: &mut Writer, reader: &mut Reader, method: &str, params: Value) -> Result<Value, String> {
    let id = writer.request(method, params).await?;
    let wait = async {
        loop {
            let Some(reply) = reader.next().await? else { return Err("the machine closed the connection".to_string()) };
            if reply["id"] == id.as_str() {
                return outcome(&reply);
            }
        }
    };
    tokio::time::timeout(REPLY, wait).await.map_err(|_| format!("{method}: no reply in time"))?
}

/// A reply's result, or its error as `code: message`.
pub fn outcome(reply: &Value) -> Result<Value, String> {
    if reply["ok"] == true {
        return Ok(reply["result"].clone());
    }
    let code = reply["error"]["code"].as_str().unwrap_or("error");
    let message = reply["error"]["message"].as_str().unwrap_or("");
    Err(if message.is_empty() { code.to_string() } else { format!("{code}: {message}") })
}

async fn next_text(rx: &mut SplitStream<Ws>, limit: Option<Duration>) -> Result<Option<String>, String> {
    loop {
        let next = match limit {
            Some(d) => tokio::time::timeout(d, rx.next()).await.map_err(|_| "no answer in time".to_string())?,
            None => rx.next().await,
        };
        match next {
            None | Some(Ok(Message::Close(_))) => return Ok(None),
            Some(Err(e)) => return Err(format!("the connection broke: {e}")),
            Some(Ok(Message::Text(text))) => return Ok(Some(text.as_str().to_owned())),
            Some(Ok(Message::Binary(bytes))) => {
                if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                    return Ok(Some(text));
                }
            }
            Some(Ok(_)) => {}
        }
    }
}
