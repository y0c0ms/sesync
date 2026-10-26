//! End-to-end tests of the phone bridge: a real WebSocket server on loopback, driven by a client that
//! speaks Orca's legacy E2EE framing. Everything runs in sandboxed directories under the temp dir.
//! The JavaScript interop check against Orca's own TypeScript is tools/bridge-interop (docs/BRIDGE.md).

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use crypto_box::aead::Aead;
use crypto_box::{Nonce, PublicKey, SalsaBox, SecretKey};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sesync::bridge::catalog::{Catalog, Conversation, StateCatalog};
use sesync::bridge::devices;
use sesync::bridge::keys;
use sesync::bridge::net;
use sesync::bridge::rpc::{Ctx, Host};
use sesync::bridge::turns::{LockTools, Turns};
use sesync::bridge::herdr::{self, Herdr};
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

struct FixedCatalog(Vec<Conversation>);

impl Catalog for FixedCatalog {
    fn conversations(&self) -> Result<Vec<Conversation>, String> {
        Ok(self.0.clone())
    }
}

/// A running bridge on an ephemeral loopback port, with its state directory.
struct Bridge {
    addr: std::net::SocketAddr,
    dir: PathBuf,
    server_public: PublicKey,
}

/// A herdr socket that does not exist: tests that need herdr supply a real one.
fn absent_herdr(dir: &Path) -> Arc<dyn herdr::Herdr> {
    Arc::new(herdr::Socket::new(dir.join("no-herdr.sock")))
}

async fn start(name: &str, catalog: Arc<dyn Catalog>, allow: &str) -> Bridge {
    let dir = sandbox(name);
    let herdr = absent_herdr(&dir);
    start_with(dir, catalog, herdr, allow).await
}

async fn start_with(dir: PathBuf, catalog: Arc<dyn Catalog>, herdr: Arc<dyn herdr::Herdr>, allow: &str) -> Bridge {
    let keys = keys::load_or_create(&dir.join("server.key")).unwrap();
    let server_public = PublicKey::from_slice(&STANDARD.decode(keys.public_key_b64()).unwrap()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shared = Arc::new(Shared {
        dir: dir.clone(),
        keys,
        allow: net::parse_list(allow).unwrap(),
        ctx: Ctx {
            catalog,
            herdr,
            host: Host { machine_name: "test-box".into(), platform: "linux" },
            locks: LockTools { sesync_lock: dir.join("no-sesync-lock"), lock_dir: dir.join("locks") },
            turns: Turns::default(),
        },
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
    async fn rpc_params(&mut self, token: &str, id: &str, method: &str, params: Value) -> Value {
        self.send(&json!({ "id": id, "deviceToken": token, "method": method, "params": params })).await;
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

    for method in ["accounts.list", "files.read", "nativeChat.send", "preflight.detectAgents", "terminal.multiplex"] {
        let reply = phone.rpc(&token, "rpc-x", method).await;
        assert_eq!(reply["id"], "rpc-x");
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], "method_not_found", "{method}");
        assert_eq!(reply["error"]["message"], format!("Unknown method: {method}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn worktree_ps_lists_each_conversation_as_a_row_and_reports_it_not_open_without_herdr() {
    let conversations = vec![Conversation {
        harness: "omp".into(),
        key: "/s/fix-login.jsonl".into(),
        title: "fix login".into(),
        dir: "/home/u/work/alpha".into(),
        updated: 50.0,
        idents: vec!["fix-login".into()],
    }];
    let b = start("worktrees", Arc::new(FixedCatalog(conversations)), LOOPBACK).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 9).await;
    phone.auth(&token).await;

    let reply = phone.rpc(&token, "w", "worktree.ps").await;
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["result"]["totalCount"], 1);
    let row = &reply["result"]["worktrees"][0];
    assert_eq!(row["path"], "/home/u/work/alpha");
    assert_eq!(row["displayName"], "fix login");
    assert_eq!(row["status"], "inactive", "herdr is absent, so nothing is open");
    assert_eq!(row["liveTerminalCount"], 0);

    let repos = phone.rpc(&token, "r", "repo.list").await;
    assert_eq!(repos["result"]["repos"][0]["displayName"], "alpha");
    assert_eq!(repos["result"]["repos"][0]["id"], row["repoId"]);
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
    assert_eq!(reply["result"]["totalCount"], 1, "one conversation: {reply}");
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

// ----- a live herdr server in a sandboxed HOME -------------------------------------------------

/// One frame from the bridge: a decrypted JSON reply or event, or a decrypted binary terminal frame.
enum Frame {
    Json(Value),
    Binary(Vec<u8>),
}

impl Phone {
    /// The next frame of either kind, decrypted.
    async fn next_frame(&mut self) -> Frame {
        let msg = next_message(&mut self.ws).await.expect("the socket stays open").expect("no protocol error");
        match msg {
            Message::Text(text) => {
                let bytes = STANDARD.decode(text.as_str()).expect("text frames are base64");
                let (nonce, boxed) = bytes.split_at(24);
                let plain = self.boxed.decrypt(Nonce::from_slice(nonce), boxed).expect("text frame authenticates");
                Frame::Json(serde_json::from_slice(&plain).unwrap())
            }
            Message::Binary(bytes) => {
                let (nonce, boxed) = bytes.split_at(24);
                Frame::Binary(self.boxed.decrypt(Nonce::from_slice(nonce), boxed).expect("binary frame authenticates"))
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }

    /// Skip everything else until the JSON reply for request `id` arrives.
    async fn reply_to(&mut self, id: &str) -> Value {
        loop {
            if let Frame::Json(v) = self.next_frame().await {
                if v["id"] == id {
                    return v;
                }
            }
        }
    }

    async fn request(&mut self, token: &str, id: &str, method: &str, params: Value) {
        self.send(&json!({ "id": id, "deviceToken": token, "method": method, "params": params })).await;
    }
}

/// The header fields of a binary terminal frame and its payload.
fn frame_parts(bytes: &[u8]) -> (u8, u32, Vec<u8>) {
    assert_eq!(bytes[0], 0x74, "terminal stream kind");
    assert_eq!(bytes[1], 1, "terminal stream version");
    (bytes[2], u32::from_le_bytes(bytes[4..8].try_into().unwrap()), bytes[16..].to_vec())
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
}

/// A `herdr server` in its own HOME, stopped when dropped. Never the operator's server: the socket
/// is under the sandbox, and every CLI call runs with the sandbox's environment.
struct SandboxHerdr {
    child: std::process::Child,
    home: PathBuf,
    socket: PathBuf,
    bin: PathBuf,
}

impl SandboxHerdr {
    /// None when no herdr is on PATH. Panics when one is but does not come up.
    fn start(name: &str) -> Option<SandboxHerdr> {
        let bin = which("herdr")?;
        let home = sandbox(name);
        let config = home.join(".config");
        std::fs::create_dir_all(config.join("herdr")).unwrap();
        let log = std::fs::File::create(home.join("server.log")).unwrap();
        let child = std::process::Command::new(&bin)
            .arg("server")
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("herdr server starts");
        let socket = config.join("herdr").join("herdr.sock");
        let server = SandboxHerdr { child, home, socket, bin };
        let api = herdr::Socket::new(server.socket.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while api.call("ping", json!({})).is_err() {
            assert!(std::time::Instant::now() < deadline, "the sandboxed herdr did not answer on {}", server.socket.display());
            std::thread::sleep(Duration::from_millis(100));
        }
        Some(server)
    }

    /// One herdr CLI call with the sandbox's environment. Its stdout.
    fn cli(&self, args: &[&str]) -> String {
        let out = std::process::Command::new(&self.bin)
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .output()
            .expect("herdr CLI runs");
        assert!(out.status.success(), "herdr {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn api(&self) -> herdr::Socket {
        herdr::Socket::new(self.socket.clone())
    }
}

impl Drop for SandboxHerdr {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Poll `check` until it holds or ten seconds pass.
fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_lists_subscribes_types_into_and_watches_a_live_herdr_pane() {
    let Some(server) = SandboxHerdr::start("herdr-it") else {
        eprintln!("skipped: no herdr on PATH, so the live-herdr test did not run");
        return;
    };
    let ident = "conv-it-1";
    let home = server.home.display().to_string();
    let created: Value = serde_json::from_str(&server.cli(&["workspace", "create", "--cwd", &home, "--label", "it"])).unwrap();
    let root = created["result"]["root_pane"]["pane_id"].as_str().unwrap().to_string();
    let api = server.api();

    // The pane runs a fake agent whose foreground argv carries the session id, as a resumed agent does.
    let program = format!("sh -c 'echo hello; while read l; do echo \"got: $l\"; done' sesync --resume {ident}");
    api.call("pane.send_text", json!({ "pane_id": root, "text": program })).unwrap();
    api.call("pane.send_keys", json!({ "pane_id": root, "keys": ["enter"] })).unwrap();
    eventually("the fake agent to be running and to have printed hello", || {
        let argvs = herdr::foreground_argvs(&api, &root).unwrap_or_default();
        let screen = herdr::read_visible(&api, &root).unwrap_or_default();
        argvs.iter().any(|a| a.contains(&ident.to_string())) && screen.lines().any(|l| l.trim() == "hello")
    });

    let conversation = Conversation {
        harness: "omp".into(),
        key: "/s/herdr-it.jsonl".into(),
        title: "live herdr".into(),
        dir: home.clone(),
        updated: 1.0,
        idents: vec![ident.into()],
    };
    let wt = conversation.worktree_id();
    let selector = format!("id:{wt}");
    let herdr_socket: Arc<dyn herdr::Herdr> = Arc::new(server.api());
    let dir = sandbox("herdr-it-bridge");
    let b = start_with(dir, Arc::new(FixedCatalog(vec![conversation])), herdr_socket, LOOPBACK).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 9).await;
    assert_eq!(phone.auth(&token).await["type"], "e2ee_authenticated");

    // The home list shows the conversation open, with its agent row.
    let home_list = phone.rpc_params(&token, "w", "worktree.ps", json!({})).await;
    let row = &home_list["result"]["worktrees"][0];
    assert_eq!(row["worktreeId"], wt.as_str(), "{home_list}");
    assert_eq!(row["liveTerminalCount"], 1);
    assert_eq!(row["agents"][0]["state"], "waiting", "an idle shell-like agent is waiting");

    // Its tab and terminal are the pane.
    let tabs = phone.rpc_params(&token, "t", "session.tabs.list", json!({ "worktree": selector })).await;
    let handle = tabs["result"]["tabs"][0]["terminal"].as_str().expect("a terminal tab").to_string();
    assert!(handle.starts_with("term_"), "herdr's terminal id: {handle}");
    assert_eq!(tabs["result"]["activeTabId"], handle.as_str());
    let terminals = phone.rpc_params(&token, "l", "terminal.list", json!({ "worktree": selector })).await;
    assert_eq!(terminals["result"]["terminals"][0]["handle"], handle.as_str());

    // The read is the pane's text.
    let read = phone.rpc_params(&token, "rd", "terminal.read", json!({ "terminal": handle })).await;
    assert!(read["result"]["terminal"]["tail"].to_string().contains("hello"), "{read}");

    // Subscribe with the binary stream: a subscribed reply, then the screen as snapshot frames.
    phone.request(&token, "sub", "terminal.subscribe", json!({
        "terminal": handle,
        "client": { "id": "phone", "type": "mobile" },
        "viewport": { "cols": 80, "rows": 24 },
        "capabilities": { "terminalBinaryStream": 1 },
    })).await;
    let subscribed = phone.reply_to("sub").await;
    assert_eq!(subscribed["result"]["type"], "subscribed", "{subscribed}");
    let stream_id = subscribed["result"]["streamId"].as_u64().expect("a stream id") as u32;
    let mut snapshot = String::new();
    loop {
        match phone.next_frame().await {
            Frame::Binary(bytes) => {
                let (opcode, id, payload) = frame_parts(&bytes);
                assert_eq!(id, stream_id, "frames carry the announced stream id");
                match opcode {
                    2 => {}
                    3 => snapshot.push_str(std::str::from_utf8(&payload).unwrap()),
                    4 => break,
                    other => panic!("opcode {other} inside a snapshot"),
                }
            }
            Frame::Json(v) => panic!("a reply inside the snapshot: {v}"),
        }
    }
    assert!(snapshot.contains("hello"), "the snapshot shows the pane: {snapshot:?}");

    // Typing a prompt: the text and Enter reach the pane, and the output comes back as Output frames.
    // The send's reply and the output frames may interleave, so read until both have arrived.
    phone.request(&token, "s", "terminal.send", json!({
        "terminal": handle, "text": "ping", "enter": true, "client": { "id": "phone", "type": "mobile" },
    })).await;
    let mut sent: Option<Value> = None;
    let mut echoed = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while sent.is_none() || !echoed.contains("got: ping") {
        assert!(std::time::Instant::now() < deadline, "the send or its echo never arrived: {echoed:?}");
        match phone.next_frame().await {
            Frame::Json(v) if v["id"] == "s" => sent = Some(v),
            Frame::Json(_) => {}
            Frame::Binary(bytes) => {
                let (opcode, _, payload) = frame_parts(&bytes);
                if opcode == 1 {
                    echoed = std::str::from_utf8(&payload).unwrap().to_string();
                }
            }
        }
    }
    let sent = sent.expect("the send reply");
    assert_eq!(sent["result"]["send"]["accepted"], true, "{sent}");

    // Stopping the stream answers the subscribe request with its end, and the unsubscribe with true.
    phone.request(&token, "unsub", "terminal.unsubscribe", json!({
        "subscriptionId": format!("{handle}:phone"), "client": { "id": "phone" }, "requestId": "sub",
    })).await;
    let stopped = phone.reply_to("unsub").await;
    assert_eq!(stopped["result"]["unsubscribed"], true, "{stopped}");
    // A phone without the binary capability gets the legacy JSON events: the screen as `scrollback`, which
    // by now shows the echo, then `data` events for later changes.
    phone.request(&token, "json", "terminal.subscribe", json!({
        "terminal": handle, "client": { "id": "phone-json", "type": "mobile" },
    })).await;
    let legacy = phone.reply_to("json").await;
    assert_eq!(legacy["result"]["type"], "scrollback", "{legacy}");
    assert!(legacy["result"]["serialized"].as_str().unwrap().contains("got: ping"), "{legacy}");
    phone.request(&token, "json-stop", "terminal.unsubscribe", json!({
        "subscriptionId": format!("{handle}:phone-json"), "client": { "id": "phone-json" }, "requestId": "json",
    })).await;
    assert_eq!(phone.reply_to("json-stop").await["result"]["unsubscribed"], true);

    // The status stream lists the pane as a session; herdr has no agent status for a plain shell.
    phone.request(&token, "st", "agentSession.subscribeStatus", json!({})).await;
    let status = phone.reply_to("st").await;
    assert_eq!(status["result"]["type"], "snapshot");
    let sessions = &status["result"]["sessions"];
    assert_eq!(sessions[0]["sessionId"], handle.as_str());
    assert_eq!(sessions[0]["workspaceId"], wt.as_str());
    assert!(sessions[0]["status"].is_null(), "herdr reports no status: {status}");
}
