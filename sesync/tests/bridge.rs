//! End-to-end tests of the phone bridge: a real WebSocket server on loopback, driven by a client that
//! speaks Orca's legacy E2EE framing. Everything runs in sandboxed directories under the temp dir.
//! The JavaScript interop check against Orca's own TypeScript is tools/bridge-interop (docs/BRIDGE.md).

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use crypto_box::aead::Aead;
use crypto_box::{Nonce, PublicKey, SalsaBox, SecretKey};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sesync::bridge::catalog::{Catalog, Project, Session, StateCatalog};
use sesync::bridge::devices;
use sesync::bridge::keys;
use sesync::bridge::net;
use sesync::bridge::rpc::Host;
use sesync::bridge::server::{self, Shared};
use sesync::manage::Roots;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

const LOOPBACK: &str = "127.0.0.1/32";

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-bridge-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct FixedCatalog(Vec<Project>);

impl Catalog for FixedCatalog {
    fn projects(&self) -> Result<Vec<Project>, String> {
        Ok(self.0.clone())
    }
}

/// A running bridge on an ephemeral loopback port, with its state directory.
struct Bridge {
    addr: std::net::SocketAddr,
    dir: PathBuf,
    server_public: PublicKey,
}

async fn start(name: &str, catalog: Arc<dyn Catalog>, allow: &str) -> Bridge {
    let dir = sandbox(name);
    let keys = keys::load_or_create(&dir.join("server.key")).unwrap();
    let server_public = PublicKey::from_slice(&STANDARD.decode(keys.public_key_b64()).unwrap()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shared = Arc::new(Shared {
        dir: dir.clone(),
        keys,
        allow: net::parse_list(allow).unwrap(),
        host: Host { machine_name: "test-box".into(), platform: "linux" },
        catalog,
    });
    tokio::spawn(server::serve(listener, shared));
    Bridge { addr, dir, server_public }
}

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// A phone-side connection: the socket and the shared key from its hello.
struct Phone {
    ws: Ws,
    boxed: SalsaBox,
    counter: u64,
}

const STEP: Duration = Duration::from_secs(5);

async fn next_message(ws: &mut Ws) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
    tokio::time::timeout(STEP, ws.next()).await.expect("the bridge did not answer in time")
}

impl Phone {
    /// Connect and complete the plaintext hello: the bridge must answer `e2ee_ready`.
    async fn hello(b: &Bridge, secret_byte: u8) -> Phone {
        let (mut ws, _) = connect_async(format!("ws://{}", b.addr)).await.unwrap();
        let secret = SecretKey::from_slice(&[secret_byte; 32]).unwrap();
        let public = secret.public_key();
        let hello = json!({ "type": "e2ee_hello", "publicKeyB64": STANDARD.encode(public.as_bytes()) });
        ws.send(Message::Text(hello.to_string().into())).await.unwrap();
        let ready = next_text(&mut ws).await;
        assert_eq!(ready, json!({ "type": "e2ee_ready" }).to_string());
        Phone { ws, boxed: SalsaBox::new(&b.server_public, &secret), counter: 0 }
    }

    fn seal(&mut self, value: &Value) -> String {
        self.counter += 1;
        let mut nonce = [0u8; 24];
        nonce[16..].copy_from_slice(&self.counter.to_be_bytes());
        let boxed = self.boxed.encrypt(Nonce::from_slice(&nonce), value.to_string().as_bytes()).unwrap();
        let mut frame = nonce.to_vec();
        frame.extend(boxed);
        STANDARD.encode(frame)
    }

    async fn send(&mut self, value: &Value) {
        let frame = self.seal(value);
        self.ws.send(Message::Text(frame.into())).await.unwrap();
    }

    /// The next encrypted reply, decrypted.
    async fn recv(&mut self) -> Value {
        let text = next_text(&mut self.ws).await;
        let bytes = STANDARD.decode(text).expect("reply is base64");
        let (nonce, boxed) = bytes.split_at(24);
        let plain = self.boxed.decrypt(Nonce::from_slice(nonce), boxed).expect("reply authenticates");
        serde_json::from_slice(&plain).unwrap()
    }

    /// Authenticate with `token`; returns the bridge's reply.
    async fn auth(&mut self, token: &str) -> Value {
        self.send(&json!({ "type": "e2ee_auth", "deviceToken": token })).await;
        self.recv().await
    }

    async fn rpc(&mut self, token: &str, id: &str, method: &str) -> Value {
        self.send(&json!({ "id": id, "deviceToken": token, "method": method })).await;
        self.recv().await
    }

    /// The close code the bridge ends the socket with.
    async fn closed_with(&mut self) -> Option<u16> {
        match next_message(&mut self.ws).await {
            Some(Ok(Message::Close(Some(frame)))) => Some(u16::from(frame.code)),
            Some(Ok(Message::Close(None))) | None => None,
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

async fn next_text(ws: &mut Ws) -> String {
    match next_message(ws).await {
        Some(Ok(Message::Text(t))) => t.as_str().to_string(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

/// Pair a device in `dir` and return its token.
fn pair(dir: &Path, label: &str) -> (devices::Device, String) {
    devices::create(dir, label, 1_000).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paired_phone_authenticates_and_reads_status() {
    let b = start("status", Arc::new(FixedCatalog(vec![])), LOOPBACK).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 9).await;

    assert_eq!(phone.auth(&token).await, json!({ "type": "e2ee_authenticated" }));
    let reply = phone.rpc(&token, "rpc-1", "status.get").await;
    assert_eq!(reply["id"], "rpc-1");
    assert_eq!(reply["ok"], true);
    // The keys and types of Orca's status.get result (mobile/rpc-foundation/goldens/
    // transport-host-status-gates-ready.json): what the app reads to gate compatibility.
    let result = &reply["result"];
    assert!(result["appVersion"].is_string());
    assert!(result["capabilities"].is_array());
    assert!(result["floatingWorkspaceEnabled"].is_boolean());
    assert!(result["minCompatibleMobileVersion"].is_number());
    assert!(result["protocolVersion"].is_number());
    assert_eq!(result["protocolVersion"], 3);
    assert_eq!(result["minCompatibleMobileVersion"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_token_gets_unauthorized_and_a_4001_close() {
    let b = start("bad-token", Arc::new(FixedCatalog(vec![])), LOOPBACK).await;
    let _ = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 9).await;

    let reply = phone.auth("not-a-paired-token").await;
    assert_eq!(reply, json!({ "type": "e2ee_error", "error": { "code": "unauthorized" } }));
    assert_eq!(phone.closed_with().await, Some(4001));
}

#[tokio::test(flavor = "multi_thread")]
async fn frames_before_auth_that_do_not_decrypt_are_dropped_silently() {
    let b = start("garbage", Arc::new(FixedCatalog(vec![])), LOOPBACK).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 9).await;

    phone.ws.send(Message::Text("bm90IGEgZnJhbWU=".into())).await.unwrap();
    // The garbage gets no answer, so the next reply is the answer to the real auth.
    assert_eq!(phone.auth(&token).await, json!({ "type": "e2ee_authenticated" }));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unserved_method_answers_method_not_found_with_the_request_id() {
    let b = start("not-found", Arc::new(FixedCatalog(vec![])), LOOPBACK).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 9).await;
    phone.auth(&token).await;

    for method in ["terminal.list", "accounts.list", "session.tabs.list", "preflight.detectAgents"] {
        let reply = phone.rpc(&token, "rpc-x", method).await;
        assert_eq!(reply["id"], "rpc-x");
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], "method_not_found", "{method}");
        assert_eq!(reply["error"]["message"], format!("Unknown method: {method}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn worktree_ps_lists_the_catalog_as_one_row_per_project() {
    let projects = vec![Project {
        dir: "/home/u/work/alpha".into(),
        sessions: vec![Session { harness: "omp".into(), title: "fix login".into(), updated: 50.0 }],
    }];
    let b = start("worktrees", Arc::new(FixedCatalog(projects)), LOOPBACK).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 9).await;
    phone.auth(&token).await;

    let reply = phone.rpc(&token, "w", "worktree.ps").await;
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["result"]["totalCount"], 1);
    assert_eq!(reply["result"]["worktrees"][0]["path"], "/home/u/work/alpha");
    assert_eq!(reply["result"]["worktrees"][0]["preview"], "1 session · latest: fix login");

    let repos = phone.rpc(&token, "r", "repo.list").await;
    assert_eq!(repos["result"]["repos"][0]["displayName"], "alpha");
    assert_eq!(repos["result"]["repos"][0]["id"], reply["result"]["worktrees"][0]["repoId"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_real_catalog_groups_an_omp_session_under_its_project_directory() {
    let sb = sandbox("state-catalog");
    let roots = Roots::under(&sb, sb.join("no-sesync-conv"));
    let dir = roots.locations.sessions_dir("omp").join("-work-proj-a");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl"),
        "{\"type\":\"session\",\"cwd\":\"/work/proj-a\"}\n",
    )
    .unwrap();
    let catalog = Arc::new(StateCatalog { roots, state_file: sb.join("state.json") });

    let b = start("state-catalog-bridge", catalog, LOOPBACK).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 9).await;
    phone.auth(&token).await;

    let reply = phone.rpc(&token, "w", "worktree.ps").await;
    assert_eq!(reply["result"]["totalCount"], 1, "one project directory: {reply}");
    assert_eq!(reply["result"]["worktrees"][0]["path"], "/work/proj-a");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_device_is_refused_on_its_next_frame() {
    let b = start("revoke", Arc::new(FixedCatalog(vec![])), LOOPBACK).await;
    let (device, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 9).await;
    phone.auth(&token).await;
    assert_eq!(phone.rpc(&token, "1", "status.get").await["ok"], true);

    devices::revoke(&b.dir, &device.id).unwrap();
    let reply = phone.rpc(&token, "2", "status.get").await;
    assert_eq!(reply["id"], "2");
    assert_eq!(reply["error"]["code"], "unauthorized");
    assert_eq!(phone.closed_with().await, Some(4001));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_v2_hello_is_closed_with_4001_before_ready_so_the_app_falls_back() {
    let b = start("v2", Arc::new(FixedCatalog(vec![])), LOOPBACK).await;
    let (mut ws, _) = connect_async(format!("ws://{}", b.addr)).await.unwrap();
    let public = SecretKey::from_slice(&[9u8; 32]).unwrap().public_key();
    let hello = json!({
        "type": "e2ee_hello",
        "v": 2,
        "publicKeyB64": STANDARD.encode(public.as_bytes()),
        "clientNonceB64": STANDARD.encode([0u8; 32]),
    });
    ws.send(Message::Text(hello.to_string().into())).await.unwrap();
    match next_message(&mut ws).await {
        Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 4001),
        other => panic!("expected a 4001 close, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_outside_the_allowlist_gets_no_protocol_bytes() {
    // Loopback is not in this allowlist, so the bridge drops the socket straight after accept.
    let b = start("allowlist", Arc::new(FixedCatalog(vec![])), "10.10.20.0/24").await;
    match connect_async(format!("ws://{}", b.addr)).await {
        Err(_) => {}
        Ok((mut ws, _)) => {
            let next = tokio::time::timeout(STEP, ws.next()).await.expect("the socket must end");
            assert!(!matches!(next, Some(Ok(Message::Text(_)))), "no protocol frame may be sent");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hello_that_is_not_e2ee_is_refused_without_ready() {
    let b = start("bad-hello", Arc::new(FixedCatalog(vec![])), LOOPBACK).await;
    let (mut ws, _) = connect_async(format!("ws://{}", b.addr)).await.unwrap();
    ws.send(Message::Text(json!({ "type": "hello" }).to_string().into())).await.unwrap();
    let reply: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
    assert_eq!(reply["type"], "e2ee_error");
    assert_eq!(reply["message"], "Expected e2ee_hello");
}
