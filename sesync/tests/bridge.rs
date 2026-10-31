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
use sesync::bridge::{notify, ntfy, start, uploads, usage};
use sesync::bridge::dictation::{Dictation, Whisper};
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
            dir: dir.clone(),
            machines: dir.join("machines.json"),
            notify: notify::Hub::new(None),
            usage: usage::Store::default(),
            uploads: uploads::Store::default(),
            dictation: Dictation::new(Whisper::from_env()),
            launch: start::Launch::default(),
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
    devices::create(dir, label, devices::Kind::Phone, 1_000).unwrap()
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

fn machine_for(b: &Bridge, token: &str) -> sesync::machines::store::Machine {
    sesync::machines::store::Machine {
        name: "it".into(),
        endpoint: format!("ws://{}", b.addr),
        device_token: token.into(),
        public_key_b64: STANDARD.encode(b.server_public.as_bytes()),
        grant_device_id: None,
        local_endpoint: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sesync_client_connects_over_the_hybrid_handshake_and_a_phone_still_over_the_legacy_one() {
    use sesync::machines::client;
    let b = start("hybrid", Arc::new(FixedCatalog(vec![])), LOOPBACK).await;
    let (_, token) = pair(&b.dir, "machine");
    let (mut writer, mut reader) = client::connect(&machine_for(&b, &token)).await.expect("hybrid handshake and auth");
    let status = client::call(&mut writer, &mut reader, "status.get", json!({})).await.unwrap();
    assert_eq!(status["protocolVersion"], 3);
    let list = client::call(&mut writer, &mut reader, "worktree.ps", json!({})).await.unwrap();
    assert_eq!(list["worktrees"], json!([]));

    // The same bridge still serves the phone's legacy hello.
    let mut phone = Phone::hello(&b, 9).await;
    assert_eq!(phone.auth(&token).await, json!({ "type": "e2ee_authenticated" }));

    // A wrong token is refused after the hybrid handshake as after the legacy one.
    let refused = client::connect(&machine_for(&b, "not-a-paired-token")).await.err().expect("refused");
    assert!(refused.contains("refused this machine's pairing"), "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hybrid_hello_with_a_broken_ml_kem_key_gets_an_error_and_no_ready() {
    let b = start("hybrid-broken", Arc::new(FixedCatalog(vec![])), LOOPBACK).await;
    let (mut ws, _) = connect_async(format!("ws://{}", b.addr)).await.unwrap();
    let public = SecretKey::from_slice(&[9; 32]).unwrap().public_key();
    let hello = json!({ "type": "e2ee_hello", "v": 3, "publicKeyB64": STANDARD.encode(public.as_bytes()), "mlkemEncapsulationKeyB64": STANDARD.encode([1u8; 64]) });
    ws.send(Message::Text(hello.to_string().into())).await.unwrap();
    let reply: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
    assert_eq!(reply["type"], "e2ee_error", "{reply}");
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

    for method in ["git.commit", "nativeChat.send", "agentSession.send", "agentSession.currentRequests", "worktree.create", "terminal.multiplex", "files.writeTerminalArtifact"] {
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
    assert_eq!(row["conversationAgent"], "omp", "a closed conversation resumes in its own harness");
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
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("OLLAMA_HOST", "http://127.0.0.1:9")
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
        // The sandbox holds omp's extracted natives (hundreds of MB); it goes with the server.
        let _ = std::fs::remove_dir_all(&self.home);
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

    // The announced size is the pane's real size, from herdr's layout, in desktop mode.
    let (cols, rows) = herdr::pane_size(&api, &root).expect("herdr lays the pane out");
    assert_eq!((subscribed["result"]["cols"].as_u64(), subscribed["result"]["rows"].as_u64()), (Some(cols as u64), Some(rows as u64)), "{subscribed}");
    assert_eq!(subscribed["result"]["displayMode"], "desktop", "{subscribed}");

    // Splitting the pane halves its width: one resized snapshot follows, at the width herdr now lays out.
    server.cli(&["pane", "split", &root, "--direction", "right"]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let resized = loop {
        assert!(std::time::Instant::now() < deadline, "no resized snapshot after the split");
        if let Frame::Binary(bytes) = phone.next_frame().await {
            let (opcode, id, payload) = frame_parts(&bytes);
            assert_eq!(id, stream_id, "frames carry the announced stream id");
            if opcode == 2 {
                let meta: Value = serde_json::from_slice(&payload).unwrap();
                if meta["kind"] == "resized" {
                    break meta;
                }
            }
        }
    };
    let (new_cols, new_rows) = {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let size = herdr::pane_size(&api, &root).expect("herdr lays the pane out");
            if size.0 != cols || std::time::Instant::now() >= deadline {
                break size;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    assert!(new_cols < cols, "the split narrows the pane: {cols} -> {new_cols}");
    assert_eq!((resized["cols"].as_u64(), resized["rows"].as_u64()), (Some(new_cols as u64), Some(new_rows as u64)), "{resized}");
    assert_eq!(resized["displayMode"], "desktop", "{resized}");

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

// ----- phase 2: a live world (herdr, fake-model, omp) -----------------------------------------------

/// A one-pixel PNG: the image the upload test sends. omp decodes it as an image input.
const PIXEL_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

/// How long a turn, a pane or a notification may take before a test fails.
const TURN: Duration = Duration::from_secs(40);

/// The fake-model binary: `FAKE_MODEL_BIN`, else the sibling worktree's static build. None when absent.
fn fake_model_bin() -> Option<PathBuf> {
    let bin = match std::env::var_os("FAKE_MODEL_BIN") {
        Some(path) => PathBuf::from(path),
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../sesync-wt-fake-model/fake-model/target/x86_64-unknown-linux-musl/release/fake-model"),
    };
    bin.is_file().then_some(bin)
}

/// A `fake-model` on loopback serving `scenario`, stopped when dropped. `log` is its request log.
struct FakeModel {
    child: std::process::Child,
    _stdout: std::io::BufReader<std::process::ChildStdout>,
    port: u16,
    log: PathBuf,
}

impl FakeModel {
    fn start(dir: &Path, scenario: &str) -> Option<FakeModel> {
        let bin = fake_model_bin()?;
        let scenario_file = dir.join("scenario.yaml");
        std::fs::write(&scenario_file, scenario).unwrap();
        let log = dir.join("requests.jsonl");
        let mut child = std::process::Command::new(bin)
            .args(["--listen", "127.0.0.1:0", "--scenario"])
            .arg(&scenario_file)
            .arg("--log")
            .arg(&log)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("fake-model starts");
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        std::io::BufRead::read_line(&mut stdout, &mut line).unwrap();
        let port = line.split("port=").nth(1)?.trim().parse().ok()?;
        Some(FakeModel { child, _stdout: stdout, port, log })
    }

    /// Every request the fake model has served, parsed from its log.
    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

impl Drop for FakeModel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// omp's models for a sandbox HOME: the fake provider alone, its model taking text and images.
fn write_models(home: &Path, port: u16) {
    let dir = home.join(".omp/agent");
    std::fs::create_dir_all(&dir).unwrap();
    let models = format!(
        "providers:\n  fake:\n    baseUrl: http://127.0.0.1:{port}/v1\n    api: openai-completions\n    apiKey: fake-key-not-a-secret\n    authHeader: true\n    discovery:\n      type: openai-models-list\n      injectV1: false\n    modelOverrides:\n      fake-32k:\n        input: [text, image]\n"
    );
    std::fs::write(dir.join("models.yml"), models).unwrap();
}

/// An omp command in the sandbox HOME, with the environment that keeps every model call on the fake.
fn omp_in(home: &Path, cwd: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("omp");
    cmd.current_dir(cwd)
        .env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("TERM", "dumb")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("OLLAMA_HOST", "http://127.0.0.1:9");
    cmd
}

/// Every session journal (`.jsonl`) under `dir`.
fn journals_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                todo.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                found.push(path);
            }
        }
    }
    found
}

/// Poll `check` every 100 ms until it yields a value; panics naming `what` after `secs`.
fn wait_until<T>(what: &str, secs: u64, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl Phone {
    /// The next frame within `within`, decrypted to JSON. Binary frames are skipped. None on timeout.
    async fn frame_within(&mut self, within: Duration) -> Option<Value> {
        loop {
            let next = tokio::time::timeout(within, self.ws.next()).await.ok()?;
            match next?.ok()? {
                Message::Text(text) => {
                    let bytes = STANDARD.decode(text.as_str()).expect("text frames are base64");
                    let (nonce, boxed) = bytes.split_at(24);
                    let plain = self.boxed.decrypt(Nonce::from_slice(nonce), boxed).expect("text frame authenticates");
                    return Some(serde_json::from_slice(&plain).unwrap());
                }
                Message::Binary(_) => continue,
                _ => return None,
            }
        }
    }
}

/// One request, its reply awaited. Stream frames that arrive meanwhile are kept in `inbox`.
async fn call_into(phone: &mut Phone, token: &str, id: &str, method: &str, params: Value, inbox: &mut Vec<Value>) -> Value {
    phone.request(token, id, method, params).await;
    loop {
        let frame = phone.frame_within(STEP).await.expect("the bridge answers in time");
        if frame["id"] == id {
            return frame;
        }
        inbox.push(frame);
    }
}

/// A bridge on loopback that serves `ctx`, with its keys in `dir`.
async fn start_ctx(dir: PathBuf, ctx: Ctx) -> Bridge {
    let keys = keys::load_or_create(&dir.join("server.key")).unwrap();
    let server_public = PublicKey::from_slice(&STANDARD.decode(keys.public_key_b64()).unwrap()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shared = Arc::new(Shared { dir: dir.clone(), keys, allow: net::parse_list(LOOPBACK).unwrap(), ctx });
    tokio::spawn(server::serve(listener, shared));
    Bridge { addr, dir, server_public }
}

/// One conversation open in an omp pane, served by a bridge with its phone connected.
struct World {
    /// Held for the whole test: see `LIVE_TESTS`.
    _serial: parking_lot::MutexGuard<'static, ()>,
    herdr: SandboxHerdr,
    fake: FakeModel,
    project: PathBuf,
    ctx: Ctx,
    bridge: Bridge,
    token: String,
    phone: Phone,
    inbox: Vec<Value>,
    worktree: String,
    /// The tab's terminal id: the handle the phone types into.
    terminal: String,
    /// The journal session id the chat view reads by.
    session: String,
}

impl World {
    /// One request; stream frames that arrive meanwhile are kept in the inbox.
    async fn rpc(&mut self, id: &str, method: &str, params: Value) -> Value {
        call_into(&mut self.phone, &self.token, id, method, params, &mut self.inbox).await
    }

    /// Type `text` and press Enter in the conversation's pane, as the composer does. A previous turn
    /// holds the conversation until the bridge sees its agent idle, so a refusal for that is retried.
    async fn send(&mut self, text: &str) {
        let terminal = self.terminal.clone();
        let deadline = std::time::Instant::now() + TURN;
        loop {
            let reply = self.rpc("send", "terminal.send", json!({ "terminal": terminal, "text": text, "enter": true })).await;
            if reply["result"]["send"]["accepted"] == true {
                return;
            }
            // A turn's end and herdr's status can differ by a moment: a conversation still held, or a pane herdr still reports blocked, is retried.
            let busy = matches!(reply["error"]["code"].as_str(), Some("conversation_locked" | "agent_waiting_on_prompt"));
            assert!(busy && std::time::Instant::now() < deadline, "{reply}");
            self.pump(Duration::from_millis(200)).await;
        }
    }

    /// Type `text` without Enter: an approval card's key.
    async fn key(&mut self, text: &str) {
        let terminal = self.terminal.clone();
        let reply = self.rpc("key", "terminal.send", json!({ "terminal": terminal, "text": text, "enter": false })).await;
        assert_eq!(reply["result"]["send"]["accepted"], true, "{reply}");
    }

    /// Keep the frames the bridge sends for `within` in the inbox.
    async fn pump(&mut self, within: Duration) {
        let deadline = std::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return;
            }
            match self.phone.frame_within(left).await {
                Some(frame) => self.inbox.push(frame),
                None => return,
            }
        }
    }

    /// The conversation's tab that satisfies `pred`, polled until one does.
    async fn tab_where(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let worktree = self.worktree.clone();
        let deadline = std::time::Instant::now() + TURN;
        loop {
            let reply = self.rpc("tabs", "session.tabs.list", json!({ "worktree": worktree })).await;
            if let Some(tab) = reply["result"]["tabs"].as_array().and_then(|tabs| tabs.iter().find(|t| pred(*t))) {
                return tab.clone();
            }
            assert!(std::time::Instant::now() < deadline, "{what}: last reply {reply}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// The first notification frame with agent state `state`, read for up to the turn limit.
    async fn notification(&mut self, state: &str) -> Value {
        let deadline = std::time::Instant::now() + TURN;
        loop {
            if let Some(frame) = self.inbox.iter().find(|f| is_notification(f, state)) {
                return frame.clone();
            }
            assert!(std::time::Instant::now() < deadline, "no '{state}' notification in {:?}", self.inbox);
            self.pump(Duration::from_millis(200)).await;
        }
    }
}

fn is_notification(frame: &Value, state: &str) -> bool {
    frame["result"]["type"] == "notification" && frame["result"]["agentState"] == state
}

/// The worktree id of the conversation whose path ends with `suffix`, polled until listed.
async fn wait_for_worktree(phone: &mut Phone, token: &str, inbox: &mut Vec<Value>, suffix: &str) -> String {
    let deadline = std::time::Instant::now() + TURN;
    loop {
        let reply = call_into(phone, token, "wt", "worktree.ps", json!({}), inbox).await;
        let found = reply["result"]["worktrees"].as_array().and_then(|rows| {
            rows.iter()
                .find(|r| r["path"].as_str().is_some_and(|p| p.ends_with(suffix)))
                .and_then(|r| r["worktreeId"].as_str().map(str::to_string))
        });
        if let Some(id) = found {
            return id;
        }
        assert!(std::time::Instant::now() < deadline, "the conversation is listed: {reply}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The first tab of `worktree`, polled until herdr's pane for the conversation shows up.
async fn wait_for_tab(phone: &mut Phone, token: &str, inbox: &mut Vec<Value>, worktree: &str) -> Value {
    let deadline = std::time::Instant::now() + TURN;
    loop {
        let reply = call_into(phone, token, "tabs", "session.tabs.list", json!({ "worktree": worktree }), inbox).await;
        if let Some(tab) = reply["result"]["tabs"].as_array().and_then(|tabs| tabs.first()) {
            return tab.clone();
        }
        assert!(std::time::Instant::now() < deadline, "the conversation's pane is listed: {reply}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A live world for one test: a sandboxed herdr, a fake-model serving `scenario`, the conversation
/// (an omp session) open in a pane, and a bridge with a paired phone. None, with a note, when herdr or
/// the fake-model binary is not available.
/// Live tests take this lock, so their omp and herdr runs never compete for CPU and time budgets.
static LIVE_TESTS: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

async fn live_world(name: &str, scenario: &str, push: Option<ntfy::Config>) -> Option<World> {
    // One live world at a time: each runs omp and herdr, and their 40 s budgets fail under parallel load.
    let serial = LIVE_TESTS.lock();
    let Some(herdr) = SandboxHerdr::start(name) else {
        eprintln!("skipped {name}: no herdr on PATH");
        return None;
    };
    let Some(fake) = FakeModel::start(&herdr.home, scenario) else {
        eprintln!("skipped {name}: set FAKE_MODEL_BIN to a fake-model build");
        return None;
    };
    write_models(&herdr.home, fake.port);
    let project = herdr.home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    // The same locations the bridge derives from its HOME in production: each harness's own `locate`.
    use sesync::harness::Harness;
    let mut locations = sesync::harness::Locations::default();
    for h in sesync::harness::ALL {
        locations.set(h.id(), h.locate(&herdr.home));
    }
    let roots = Roots { locations, home: herdr.home.clone(), sesync_conv: herdr.home.join("no-sesync-conv") };

    // The first turn writes the conversation's journal. herdr reports an omp pane's agent only once
    // omp's integration is installed, and that needs omp to have run once.
    let first = omp_in(&herdr.home, &project)
        .args(["-p", "--model", "fake-32k", "--no-title", "--no-lsp", "e2e chat start"])
        .output()
        .expect("omp runs");
    assert!(first.status.success(), "omp -p: {}", String::from_utf8_lossy(&first.stderr));
    herdr.cli(&["integration", "install", "omp"]);
    let journals = journals_under(&roots.locations.sessions_dir("omp"));
    assert_eq!(journals.len(), 1, "one conversation: {journals:?}");
    let session = journals[0].display().to_string();

    let dir = herdr.home.join("config/bridge");
    let resume = start::Resume::new(roots.locations.clone(), roots.home.clone(), &herdr.socket, None);
    let catalog = Arc::new(StateCatalog { roots, state_file: herdr.home.join("state.json") });
    let ctx = Ctx {
        catalog,
        herdr: Arc::new(herdr::Socket::new(herdr.socket.clone())) as Arc<dyn Herdr>,
        host: Host { machine_name: "test-box".into(), platform: "linux" },
        locks: LockTools { sesync_lock: herdr.home.join("no-sesync-lock"), lock_dir: herdr.home.join("locks") },
        turns: Turns::default(),
        dir: dir.clone(),
        machines: dir.join("machines.json"),
        notify: notify::Hub::new(push),
        usage: usage::Store::default(),
        uploads: uploads::Store::default(),
        dictation: Dictation::new(Whisper::from_env()),
        launch: start::Launch::resuming(resume),
    };
    let bridge = start_ctx(dir, ctx.clone()).await;
    let (_, token) = pair(&bridge.dir, "phone");
    let mut phone = Phone::hello(&bridge, 9).await;
    assert_eq!(phone.auth(&token).await["type"], "e2ee_authenticated");

    // A workspace whose one pane runs the conversation's omp, with the session in its argv.
    let project_text = project.display().to_string();
    herdr.cli(&["workspace", "create", "--cwd", project_text.as_str(), "--label", "e2e-chat", "--no-focus"]);
    let api = herdr.api();
    let pane = wait_until("the workspace's pane", 20, || herdr::list_panes(&api).ok()?.into_iter().next());
    let command = format!("omp --resume {session} --model fake-32k --no-title --no-lsp --approval-mode always-ask");
    herdr.cli(&["pane", "run", pane.pane_id.as_str(), command.as_str()]);
    wait_until("omp detected in the pane", 40, || {
        herdr::list_panes(&api)
            .ok()?
            .into_iter()
            .find(|p| p.pane_id == pane.pane_id && p.agent.as_deref() == Some("omp"))
    });

    let mut inbox = Vec::new();
    let worktree = wait_for_worktree(&mut phone, &token, &mut inbox, "/project").await;
    let tab = wait_for_tab(&mut phone, &token, &mut inbox, &worktree).await;
    let terminal = tab["terminal"].as_str().expect("a tab names its terminal").to_string();
    let session = tab["agentStatus"]["providerSession"]["id"].as_str().expect("a tab names its session").to_string();
    Some(World { herdr, fake, project, ctx, bridge, token, phone, inbox, worktree, terminal, session, _serial: serial })
}

/// A local ntfy stand-in on loopback. It records each request's head and answers 200.
struct FakeNtfy {
    addr: std::net::SocketAddr,
    heads: Arc<parking_lot::Mutex<Vec<String>>>,
}

impl FakeNtfy {
    fn start() -> FakeNtfy {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let heads = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen = Arc::clone(&heads);
        std::thread::spawn(move || {
            for stream in listener.incoming().map_while(Result::ok) {
                let seen = Arc::clone(&seen);
                std::thread::spawn(move || answer_ntfy(stream, &seen));
            }
        });
        FakeNtfy { addr, heads }
    }

    /// The heads (request line and headers, lower-cased) of every request so far.
    fn heads(&self) -> Vec<String> {
        self.heads.lock().iter().map(|h| h.to_ascii_lowercase()).collect()
    }
}

/// Read one HTTP request, record its head, and answer 200 with no body.
fn answer_ntfy(mut stream: std::net::TcpStream, seen: &parking_lot::Mutex<Vec<String>>) {
    use std::io::{Read, Write};
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            lower.strip_prefix("content-length:")?.trim().parse::<usize>().ok()
        })
        .unwrap_or(0);
    while buf.len() < head_end + 4 + length {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    seen.lock().push(head);
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

/// The approval scenario: a bash call on `approved.txt` once the prompt is sent, and a plain reply
/// after the tool's result (refused or run).
const APPROVAL_RULES: &str = "  - name: approval
    match: 'e2e approve please\\s*$'
    reply: {tool_call: {name: bash, arguments: {command: \"printf approved > approved.txt\"}}}
  - name: after-tool
    match: 'e2e approve please\\s*$'
    when: tool_result
    reply: {text: \"finished\"}
";

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_typed_into_a_live_omp_pane_reaches_the_chat_transcript() {
    let scenario = "rules:\n  - name: reply\n    match: 'e2e reply please\\s*$'\n    reply: {text: \"hello from the fake model\"}\n";
    let Some(mut w) = live_world("prompt", scenario, None).await else { return };
    w.send("e2e reply please").await;
    let deadline = std::time::Instant::now() + TURN;
    loop {
        let session = w.session.clone();
        let page = w.rpc("page", "nativeChat.readSession", json!({ "agent": "omp", "sessionId": session, "limit": 50 })).await;
        if page["result"]["messages"].to_string().contains("hello from the fake model") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the reply never reached the transcript: {page}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(w.fake.requests().iter().any(|r| r["reply"]["rule_name"] == "reply"), "the scenario answered");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_card_denies_without_running_and_approves_by_running_the_tool() {
    let scenario = format!("rules:\n{APPROVAL_RULES}");
    let Some(mut w) = live_world("approve", &scenario, None).await else { return };
    let approved = w.project.join("approved.txt");

    // A bash call waits for approval: the tab turns blocked and carries the approval envelope.
    w.send("e2e approve please").await;
    let card = w.tab_where("the approval card", |t| t["agentStatus"]["state"] == "blocked").await;
    let prompt: Value =
        serde_json::from_str(card["agentStatus"]["interactivePrompt"].as_str().expect("the card carries its prompt")).unwrap();
    assert_eq!(prompt["approval"]["tool"], "bash", "{card}");

    // Deny (Esc): the turn finishes and the call never runs.
    w.key("\u{1b}").await;
    let fake = &w.fake;
    wait_until("the refused turn to finish", 40, || {
        fake.requests().iter().any(|r| r["reply"]["rule_name"] == "after-tool").then_some(())
    });
    assert!(!approved.exists(), "a denied call must not write its file");

    // Approve (Enter) the same call: the tool runs and writes its file.
    w.send("e2e approve please").await;
    w.tab_where("the second approval card", |t| t["agentStatus"]["state"] == "blocked").await;
    w.key("\r").await;
    wait_until("the approved call to write its file", 40, || approved.exists().then_some(()));
    assert_eq!(std::fs::read_to_string(&approved).unwrap(), "approved");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_image_is_staged_as_a_path_and_reaches_the_model() {
    // omp expands an `@image` reference into a file block: the last user message is that block, not the text.
    let scenario = "rules:\n  - name: image\n    match: '\\.png'\n    reply: {text: \"I see a pixel\"}\n";
    let Some(mut w) = live_world("image", scenario, None).await else { return };

    let started = w.rpc("up", "clipboard.startImageUpload", json!({ "expectedBase64Length": PIXEL_PNG_B64.len() })).await;
    let upload = started["result"]["uploadId"].as_str().expect("an upload id").to_string();
    let appended = w
        .rpc("chunk", "clipboard.appendImageUploadChunk", json!({ "uploadId": upload, "offset": 0, "contentBase64": PIXEL_PNG_B64 }))
        .await;
    assert_eq!(appended["result"]["ok"], true, "{appended}");
    let committed = w.rpc("commit", "clipboard.commitImageUpload", json!({ "uploadId": upload })).await;
    let path = committed["result"].as_str().expect("the staged path").to_string();
    assert!(Path::new(&path).is_file(), "staged at {path}");

    // The app inserts the path into the prompt; omp attaches the image and the model receives it.
    w.send(&format!("@{path} e2e image please")).await;
    let fake = &w.fake;
    wait_until("the model to receive an image part", 40, || {
        fake.requests().iter().any(|r| r["images"].as_u64().is_some_and(|n| n >= 1)).then_some(())
    });
    let deadline = std::time::Instant::now() + TURN;
    loop {
        let session = w.session.clone();
        let page = w.rpc("page", "nativeChat.readSession", json!({ "agent": "omp", "sessionId": session, "limit": 50 })).await;
        if page["result"]["messages"].to_string().contains("I see a pixel") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the image turn reached the transcript: {page}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_tab_for_an_open_conversation_is_its_own_pane_and_a_project_tab_is_not_it() {
    let Some(mut w) = live_world("create", "rules:\n  - match: 'never zzz'\n    reply: {text: x}\n", None).await else {
        return;
    };
    let detected = w.rpc("detect", "preflight.detectAgents", json!({})).await;
    let agents = detected["result"].as_array().expect("a bare array of agent ids");
    assert!(agents.iter().any(|a| a == "omp"), "omp is installed: {detected}");
    let worktree = w.worktree.clone();

    // The conversation is open: its own agent asks for a tab and gets the pane already running it.
    let again = w.rpc("again", "session.tabs.createTerminal", json!({ "worktree": worktree, "agent": "omp" })).await;
    assert_eq!(again["result"]["tab"]["id"], w.terminal.as_str(), "{again}");

    // Another harness is refused: the conversation is never opened as a different session.
    let other = w.rpc("other", "session.tabs.createTerminal", json!({ "worktree": worktree, "agent": "claude" })).await;
    assert_eq!(other["error"]["code"], "bad_request", "{other}");

    // A project's tab is a fresh agent of the project: listed under no conversation.
    let rows = w.rpc("ps", "worktree.ps", json!({})).await;
    let repo = rows["result"]["worktrees"][0]["repoId"].as_str().expect("the conversation's repo id").to_string();
    let project = w.rpc("project", "session.tabs.createTerminal", json!({ "worktree": repo, "agent": "omp" })).await;
    let fresh = project["result"]["tab"]["id"].as_str().expect("the project tab's id").to_string();
    assert_ne!(fresh, w.terminal);
    let tabs = w.rpc("tabs", "session.tabs.list", json!({ "worktree": worktree })).await;
    let listed: Vec<&str> = tabs["result"]["tabs"].as_array().unwrap().iter().filter_map(|t| t["id"].as_str()).collect();
    assert_eq!(listed, vec![w.terminal.as_str()], "the project tab is not the conversation's: {tabs}");
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_conversations_tab_and_resuming_it_continues_the_same_conversation() {
    // A resumed omp asks the model for a title first (the first run used --no-title). The catch-all answers that
    // request briefly, so the prompt is not queued behind the fake model's long default reply.
    let scenario = "rules:\n  - name: resumed\n    match: 'e2e resume please\\s*$'\n    reply: {text: \"resumed reply\"}\n  - name: other\n    reply: {text: \"ok\"}\n";
    let Some(mut w) = live_world("resume", scenario, None).await else {
        return;
    };

    // The operator closes the conversation's tab in herdr: the phone then lists no tab for it.
    let pane = herdr::list_panes(&w.herdr.api()).unwrap().into_iter().find(|p| p.terminal_id == w.terminal).expect("the conversation's pane");
    w.herdr.cli(&["tab", "close", pane.tab_id.as_str()]);
    let worktree = w.worktree.clone();
    let deadline = std::time::Instant::now() + TURN;
    loop {
        let tabs = w.rpc("gone", "session.tabs.list", json!({ "worktree": worktree })).await;
        if tabs["result"]["tabs"].as_array().is_some_and(|t| t.is_empty()) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the closed tab is still listed: {tabs}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Resume: the create names the conversation's own agent and gets the conversation's pane back.
    let created = w
        .rpc("resume", "session.tabs.createTerminal", json!({ "worktree": worktree, "agent": "omp", "clientMutationId": "resume-1" }))
        .await;
    let resumed = created["result"]["tab"]["id"].as_str().unwrap_or_else(|| panic!("the resumed tab: {created}")).to_string();
    let tab = w.tab_where("the resumed tab in the conversation", |t| t["id"] == resumed.as_str()).await;
    assert_eq!(tab["agentStatus"]["providerSession"]["id"], w.session.as_str(), "the same journal: {tab}");

    // The phone's prompt lands in that pane, and the model answers in the same transcript.
    w.terminal = resumed;
    w.send("e2e resume please").await;
    let session = w.session.clone();
    let deadline = std::time::Instant::now() + TURN;
    loop {
        let page = w.rpc("page", "nativeChat.readSession", json!({ "agent": "omp", "sessionId": session, "limit": 50 })).await;
        if page["result"]["messages"].to_string().contains("resumed reply") {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let screen = w.rpc("screen", "terminal.read", json!({ "terminal": w.terminal })).await;
            panic!("the resumed reply never reached the transcript: {page}\nscreen: {screen}\nmodel requests: {:?}", w.fake.requests());
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // Resuming writes to the conversation's own journal: exactly one exists, and it is the one the tab showed.
    let journals = journals_under(&w.herdr.home.join(".omp"));
    assert_eq!(journals.len(), 1, "no second journal: {journals:?}");
    assert_eq!(journals[0].display().to_string(), w.session, "the same session file");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_resume_is_refused_for_a_session_this_host_does_not_list() {
    let Some(mut w) = live_world("remote", "rules:\n  - match: 'never zzz'\n    reply: {text: x}\n", None).await else {
        return;
    };
    let remote = w.rpc("remote", "session.tabs.createTerminal", json!({ "worktree": "sesync-0000000000000000::conv-0000000000000000", "agent": "omp" })).await;
    assert_eq!(remote["error"]["code"], "worktree_not_found", "{remote}");
    assert!(remote["error"]["message"].as_str().unwrap_or("").contains("not on this machine"), "{remote}");
    assert_eq!(w.herdr.cli(&["pane", "list"]).matches("\"pane_id\"").count(), 1, "the remote create opened nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn finished_and_needs_input_reach_the_app_stream_and_the_ntfy_topic() {
    let ntfy = FakeNtfy::start();
    let push = ntfy::load(&format!("http://{}", ntfy.addr), "agents", "printf fixture-token").expect("the fixture token loads");
    let scenario = format!(
        "rules:\n  - name: reply\n    match: 'e2e reply please\\s*$'\n    reply: {{text: \"done\"}}\n{APPROVAL_RULES}"
    );
    let Some(mut w) = live_world("notify", &scenario, Some(push)).await else { return };
    notify::spawn_watcher(w.ctx.clone(), Duration::from_millis(200));

    let subscribed = w.rpc("sub", "notifications.subscribe", json!({})).await;
    assert_eq!(subscribed["result"]["type"], "ready", "{subscribed}");
    let terminal = w.terminal.clone();

    // working -> idle: finished.
    w.send("e2e reply please").await;
    let done = w.notification("done").await;
    assert_eq!(done["result"]["paneKey"], terminal.as_str(), "{done}");

    // working -> blocked: needs input. Deny afterwards so the turn ends.
    w.send("e2e approve please").await;
    let blocked = w.notification("blocked").await;
    assert_eq!(blocked["result"]["paneKey"], terminal.as_str(), "{blocked}");
    w.key("\u{1b}").await;

    wait_until("both pushes on the ntfy topic", 20, || {
        let heads = ntfy.heads();
        let posted = |title: &str| {
            heads.iter().any(|h| {
                h.starts_with("post /agents ")
                    && h.contains("authorization: bearer fixture-token")
                    && h.contains(&format!("title: {title}"))
            })
        };
        (posted("agent finished") && posted("agent needs input")).then_some(())
    });

    let missed = w.rpc("missed", "notifications.getMissedSince", json!({ "lastSeenSeq": 0 })).await;
    assert!(missed["result"]["notifications"].as_array().is_some_and(|n| n.len() >= 2), "{missed}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_phase_two_stubs_answer_the_shapes_the_app_reads() {
    let b = start("stubs", Arc::new(FixedCatalog(vec![])), LOOPBACK).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 9).await;
    phone.auth(&token).await;

    let settings = phone.rpc_params(&token, "s", "settings.get", json!({})).await;
    assert_eq!(settings["result"], json!({ "settings": {} }), "{settings}");
    let push = phone.rpc_params(&token, "p", "notifications.registerPush", json!({})).await;
    assert_eq!(push["ok"], true, "{push}");
    let send = phone.rpc_params(&token, "x", "agentSession.send", json!({})).await;
    assert_eq!(send["error"]["code"], "method_not_found", "the terminal-backed app never calls it: {send}");
}

// ----- Claude and OpenCode chats: their journals are converted by sesync-conv ------------------

const CLAUDE_SESSION: &str = "0d42eb7d-c935-538f-a4bb-5f8602b7e466";
const OPENCODE_SESSION: &str = "ses_ec3ed9f85ffe9BWEPMWmZNHxrh";
const OPENCODE_SCHEMA: &str = "create table session (id text primary key, parent_id text, directory text not null, title text not null, time_created integer not null, time_updated integer not null, time_archived integer);
create table message (id text primary key, session_id text not null, time_created integer not null, time_updated integer not null, data text not null);
create table part (id text primary key, message_id text not null, session_id text not null, time_created integer not null, time_updated integer not null, data text not null);";

/// The release `sesync-conv` the bridge converts transcripts with, as the other tests use it.
fn sesync_conv_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

/// A Claude transcript: one record per turn, each child of the one before, under the session id.
fn claude_transcript_text(turns: &[(&str, &str)]) -> String {
    let mut out = String::new();
    let mut parent = Value::Null;
    for (i, (role, text)) in turns.iter().enumerate() {
        let uuid = format!("aaaaaaaa-0000-4000-8000-{i:012}");
        let content = if *role == "user" { json!(text) } else { json!([{ "type": "text", "text": text }]) };
        let record = json!({
            "type": role,
            "uuid": uuid.clone(),
            "parentUuid": parent,
            "sessionId": CLAUDE_SESSION,
            "cwd": "/work/proj-c",
            "timestamp": format!("2026-10-27T10:00:{:02}.000Z", i * 5),
            "message": { "role": role, "content": content },
        });
        out.push_str(&format!("{record}\n"));
        parent = json!(uuid);
    }
    out
}

/// What `opencode export` prints for the OpenCode session: its info, then its messages with one text part each.
fn opencode_export_text(turns: &[(&str, &str)]) -> String {
    let messages: Vec<Value> = turns
        .iter()
        .enumerate()
        .map(|(i, (role, text))| {
            json!({
                "info": { "id": format!("msg_{i}"), "sessionID": OPENCODE_SESSION, "role": role, "time": { "created": 1100 + 100 * i as i64 } },
                "parts": [{ "id": format!("prt_{i}"), "sessionID": OPENCODE_SESSION, "messageID": format!("msg_{i}"), "type": "text", "text": text }],
            })
        })
        .collect();
    json!({
        "info": { "id": OPENCODE_SESSION, "directory": "/work/proj-o", "title": "ProjO", "time": { "created": 1000, "updated": 2000 } },
        "messages": messages,
    })
    .to_string()
}

/// A sandbox with one Claude session in `/work/proj-c` and one OpenCode session in `/work/proj-o`, in the
/// stores the bridge reads. OpenCode's CLI is a script that prints the export the test writes.
struct ChatFixture {
    sb: PathBuf,
    roots: Roots,
    claude_transcript: PathBuf,
    opencode_export: PathBuf,
    opencode_db: PathBuf,
}

#[cfg(unix)]
fn chat_fixture(name: &str) -> ChatFixture {
    use std::os::unix::fs::PermissionsExt;
    let sb = sandbox(name);
    let mut roots = Roots::under(&sb, sesync_conv_bin());
    let claude_dir = roots.locations.sessions_dir("claude").join("-work-proj-c");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let claude_transcript = claude_dir.join(format!("{CLAUDE_SESSION}.jsonl"));
    std::fs::write(&claude_transcript, claude_transcript_text(&[("user", "hello claude"), ("assistant", "hi from claude")])).unwrap();

    let opencode_db = roots.locations.of("opencode").root.clone();
    std::fs::create_dir_all(opencode_db.parent().unwrap()).unwrap();
    let con = rusqlite::Connection::open(&opencode_db).unwrap();
    con.execute_batch(OPENCODE_SCHEMA).unwrap();
    con.execute(
        "insert into session (id, directory, title, time_created, time_updated) values (?1, '/work/proj-o', 'ProjO', 1000, 2000)",
        [OPENCODE_SESSION],
    )
    .unwrap();
    drop(con);
    let opencode_export = sb.join("opencode-export.json");
    std::fs::write(&opencode_export, opencode_export_text(&[("user", "hello opencode"), ("assistant", "hi from opencode")])).unwrap();
    let cli = sb.join("opencode-bin");
    std::fs::write(&cli, format!("#!/bin/sh\ncat '{}'\n", opencode_export.display())).unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    roots.locations.set("opencode", sesync::harness::Location { root: opencode_db.clone(), cli: Some(cli) });
    // A store's sessions are listed from the state the daemon keeps for them, not from the store alone.
    let state = json!({ "sources": { OPENCODE_SESSION: { "fmt": "opencode", "key": OPENCODE_SESSION, "title": "ProjO", "last_seen_change": 2.0, "mirrors": {} } } });
    std::fs::write(sb.join("state.json"), state.to_string()).unwrap();
    ChatFixture { sb, roots, claude_transcript, opencode_export, opencode_db }
}

/// Herdr's answers for the two panes the fixture runs, as herdr lists them live: a Claude pane whose
/// foreground process is `claude --resume <id>`, and an OpenCode pane running `opencode --session <id>`.
#[cfg(unix)]
fn chat_panes(method: &str, params: &Value) -> Value {
    let pane = |pane_id: &str, terminal: &str, tab: &str, agent: &str, cwd: &str, title: &str| {
        json!({
            "pane_id": pane_id, "terminal_id": terminal, "tab_id": tab, "focused": false,
            "agent": agent, "agent_status": "idle", "cwd": cwd, "foreground_cwd": cwd, "terminal_title": title,
        })
    };
    match method {
        "pane.list" => json!({ "type": "pane_list", "panes": [
            pane("w3:pG", "term_claude", "w3:tG", "claude", "/work/proj-c", "✳ TestFromOPENCODE"),
            pane("w3:pF", "term_opencode", "w3:tF", "opencode", "/work/proj-o", "OC | ProjO"),
        ] }),
        "pane.process_info" => {
            let argv = if params["pane_id"] == "w3:pG" {
                json!(["claude", "--resume", CLAUDE_SESSION])
            } else {
                json!(["/snap/opencode/287/bin/opencode", "--session", OPENCODE_SESSION])
            };
            json!({ "type": "pane_process_info", "process_info": { "shell_pid": 1, "foreground_processes": [{ "argv": argv }] } })
        }
        "pane.read" => json!({ "type": "pane_read", "read": { "text": "", "pane_id": params["pane_id"] } }),
        _ => json!({ "type": "ok" }),
    }
}

type Calls = Arc<parking_lot::Mutex<Vec<(String, Value)>>>;

/// A herdr socket in the sandbox: each call's result is `answer(method, params)`, and every call is recorded.
/// Serves until the test process exits.
#[cfg(unix)]
fn fake_herdr(socket: &Path, answer: fn(&str, &Value) -> Value) -> Calls {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    let calls: Calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let listener = UnixListener::bind(socket).unwrap();
    let recorded = Arc::clone(&calls);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let recorded = Arc::clone(&recorded);
            std::thread::spawn(move || {
                let mut line = String::new();
                let read = BufReader::new(stream.try_clone().unwrap()).read_line(&mut line);
                if read.unwrap_or(0) == 0 {
                    return;
                }
                let request: Value = serde_json::from_str(line.trim()).unwrap();
                let method = request["method"].as_str().unwrap_or("").to_string();
                let params = request["params"].clone();
                let result = answer(&method, &params);
                recorded.lock().push((method, params));
                let _ = writeln!(stream, "{}", json!({ "id": request["id"], "result": result }));
            });
        }
    });
    calls
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_claude_and_an_opencode_session_open_on_the_phone_with_their_transcripts() {
    let fx = chat_fixture("chats-open");
    let _herdr_calls = fake_herdr(&fx.sb.join("herdr.sock"), chat_panes);
    let catalog = StateCatalog { roots: fx.roots, state_file: fx.sb.join("state.json") };
    let listed = catalog.conversations().unwrap();
    let claude = listed.iter().find(|c| c.harness == "claude").expect("the Claude session is listed").worktree_id();
    let opencode = listed.iter().find(|c| c.harness == "opencode").expect("the OpenCode session is listed").worktree_id();
    let herdr: Arc<dyn Herdr> = Arc::new(herdr::Socket::new(fx.sb.join("herdr.sock")));
    let b = start_with(fx.sb.join("bridge"), Arc::new(catalog), herdr, LOOPBACK).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 9).await;
    phone.auth(&token).await;

    for (worktree, agent, session) in [(&claude, "claude", CLAUDE_SESSION), (&opencode, "opencode", OPENCODE_SESSION)] {
        let tabs = phone.rpc_params(&token, "tabs", "session.tabs.list", json!({ "worktree": format!("id:{worktree}") })).await;
        assert_eq!(tabs["ok"], true, "{agent} tabs: {tabs}");
        assert_eq!(tabs["result"]["tabs"][0]["agentStatus"]["providerSession"]["id"], session, "{agent} tab: {tabs}");
        let page = phone.rpc_params(&token, "page", "nativeChat.readSession", json!({ "agent": agent, "sessionId": session, "limit": 50 })).await;
        assert_eq!(page["ok"], true, "{agent} transcript: {page}");
        assert!(page["result"]["messages"].to_string().contains(&format!("hi from {agent}")), "{agent} transcript: {page}");
    }
}

/// A `sesync-lock` that grants every claim, as when no other agent holds the conversation.
#[cfg(unix)]
fn granting_sesync_lock(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("sesync-lock");
    std::fs::write(&bin, "#!/bin/sh\ncase \"$1\" in\n  claim) echo \"phone $4\"; exit 0;;\n  release) exit 0;;\nesac\nexit 1\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// The next frame for request `id` that contains `text`, within twelve seconds.
async fn frame_with(phone: &mut Phone, id: &str, text: &str) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    while std::time::Instant::now() < deadline {
        if let Some(frame) = phone.frame_within(Duration::from_secs(4)).await {
            if frame["id"] == id && frame.to_string().contains(text) {
                return frame;
            }
        }
    }
    panic!("no frame with {text:?} on {id}");
}

fn append_line(path: &Path, line: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_claude_and_an_opencode_chat_follows_its_turns_and_sends_into_its_pane() {
    let fx = chat_fixture("chats-live");
    let lock = granting_sesync_lock(&fx.sb);
    let herdr_calls = fake_herdr(&fx.sb.join("herdr.sock"), chat_panes);
    let catalog = StateCatalog { roots: fx.roots, state_file: fx.sb.join("state.json") };
    let herdr: Arc<dyn Herdr> = Arc::new(herdr::Socket::new(fx.sb.join("herdr.sock")));
    let bridge_dir = fx.sb.join("bridge");
    let ctx = Ctx {
        catalog: Arc::new(catalog),
        herdr,
        host: Host { machine_name: "test-box".into(), platform: "linux" },
        locks: LockTools { sesync_lock: lock, lock_dir: fx.sb.join("locks") },
        turns: Turns::default(),
        dir: bridge_dir.clone(),
        machines: bridge_dir.join("machines.json"),
        notify: notify::Hub::new(None),
        usage: usage::Store::default(),
        uploads: uploads::Store::default(),
        dictation: Dictation::new(Whisper::from_env()),
        launch: start::Launch::default(),
    };
    let b = start_ctx(bridge_dir, ctx).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 9).await;
    phone.auth(&token).await;

    // Claude: the chat opens on its live window, and a turn appended to the transcript arrives.
    phone.request(&token, "claude-chat", "nativeChat.subscribe", json!({ "agent": "claude", "sessionId": CLAUDE_SESSION, "subscriptionId": "chat-claude", "limit": 50 })).await;
    let opened = phone.reply_to("claude-chat").await;
    assert!(opened.to_string().contains("hi from claude"), "{opened}");
    let turn = json!({
        "type": "assistant",
        "uuid": "aaaaaaaa-0000-4000-8000-00000000000f",
        "parentUuid": "aaaaaaaa-0000-4000-8000-000000000001",
        "sessionId": CLAUDE_SESSION,
        "cwd": "/work/proj-c",
        "timestamp": "2026-10-27T10:01:00.000Z",
        "message": { "role": "assistant", "content": [{ "type": "text", "text": "a live reply from claude" }] },
    });
    append_line(&fx.claude_transcript, &turn.to_string());
    frame_with(&mut phone, "claude-chat", "a live reply from claude").await;

    // OpenCode: the chat opens on its export, and a turn stored in its database and export arrives.
    phone.request(&token, "opencode-chat", "nativeChat.subscribe", json!({ "agent": "opencode", "sessionId": OPENCODE_SESSION, "subscriptionId": "chat-opencode", "limit": 50 })).await;
    let opened = phone.reply_to("opencode-chat").await;
    assert!(opened.to_string().contains("hi from opencode"), "{opened}");
    let con = rusqlite::Connection::open(&fx.opencode_db).unwrap();
    con.execute("insert into message (id, session_id, time_created, time_updated, data) values ('msg_2', ?1, 1300, 1300, '{}')", [OPENCODE_SESSION]).unwrap();
    con.execute(
        "insert into part (id, message_id, session_id, time_created, time_updated, data) values ('prt_2', 'msg_2', ?1, 1300, 1300, '{}')",
        [OPENCODE_SESSION],
    )
    .unwrap();
    drop(con);
    std::fs::write(
        &fx.opencode_export,
        opencode_export_text(&[("user", "hello opencode"), ("assistant", "hi from opencode"), ("assistant", "a live reply from opencode")]),
    )
    .unwrap();
    frame_with(&mut phone, "opencode-chat", "a live reply from opencode").await;

    // A prompt goes into the pane of the chat it was typed in.
    let claude_send = phone.rpc_params(&token, "send-c", "terminal.send", json!({ "terminal": "term_claude", "text": "continue", "enter": true })).await;
    assert_eq!(claude_send["ok"], true, "{claude_send}");
    let opencode_send = phone.rpc_params(&token, "send-o", "terminal.send", json!({ "terminal": "term_opencode", "text": "continue", "enter": true })).await;
    assert_eq!(opencode_send["ok"], true, "{opencode_send}");
    let calls = herdr_calls.lock().clone();
    for pane in ["w3:pG", "w3:pF"] {
        assert!(
            calls.iter().any(|(m, p)| m == "agent.prompt" && p["target"] == pane && p["text"] == "continue"),
            "{pane} received the prompt: {calls:?}"
        );
    }
}

/// A whisper stand-in on loopback. Each request is read in full, answered after `delay` with `status` and
/// `reply`. Returns the transcription URL.
fn fake_whisper(status: u16, reply: &'static str, delay: Duration) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/audio/transcriptions", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            read_http_request(&mut stream);
            std::thread::sleep(delay);
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.len()
            );
            let _ = std::io::Write::write_all(&mut stream, head.as_bytes());
            let _ = std::io::Write::write_all(&mut stream, reply.as_bytes());
        }
    });
    url
}

/// Reads one HTTP request: its head, then as many body bytes as its Content-Length names.
fn read_http_request(stream: &mut std::net::TcpStream) {
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    while let Ok(n) = std::io::Read::read(stream, &mut buf) {
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&data[..end]).to_lowercase();
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                .unwrap_or(0);
            if data.len() >= end + 4 + length {
                break;
            }
        }
    }
}

fn whisper_at(url: String, timeout: Duration) -> Whisper {
    Whisper { url, model: "whisper-gpu".into(), language: "auto".into(), timeout }
}

/// A bridge over a fresh directory whose dictation transcribes through `whisper`.
async fn dictation_bridge(name: &str, whisper: Whisper) -> Bridge {
    let dir = sandbox(name);
    let ctx = Ctx {
        catalog: Arc::new(FixedCatalog(vec![])),
        herdr: absent_herdr(&dir),
        host: Host { machine_name: "test-box".into(), platform: "linux" },
        locks: LockTools { sesync_lock: dir.join("no-sesync-lock"), lock_dir: dir.join("locks") },
        turns: Turns::default(),
        dir: dir.clone(),
        machines: dir.join("machines.json"),
        notify: notify::Hub::new(None),
        usage: usage::Store::default(),
        uploads: uploads::Store::default(),
        dictation: Dictation::new(whisper),
        launch: start::Launch::default(),
    };
    start_ctx(dir, ctx).await
}

/// `ms` milliseconds of 16 kHz mono PCM, base64 as the app sends a chunk.
fn speech_chunk(ms: u32) -> String {
    let pcm: Vec<u8> = (0..16 * ms).flat_map(|i| ((i % 64) as i16 - 32).to_le_bytes()).collect();
    STANDARD.encode(pcm)
}

#[tokio::test(flavor = "multi_thread")]
async fn dictation_round_trips_over_the_encrypted_link() {
    let url = fake_whisper(200, r#"{"text":" hello over the link "}"#, Duration::ZERO);
    let b = dictation_bridge("dictation-ok", whisper_at(url, Duration::from_secs(5))).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 11).await;
    assert_eq!(phone.auth(&token).await["type"], "e2ee_authenticated");

    let models = phone.rpc(&token, "m", "speech.models.list").await;
    assert_eq!(models["result"]["selectedModelId"], "whisper-gpu", "{models}");
    assert_eq!(models["result"]["models"][0]["status"], "ready", "{models}");

    let started = phone.rpc_params(&token, "s", "speech.dictation.start", json!({ "dictationId": "d1" })).await;
    assert_eq!(started["ok"], true, "{started}");
    let chunk = phone
        .rpc_params(&token, "c", "speech.dictation.chunk", json!({ "dictationId": "d1", "audioBase64": speech_chunk(1000), "sampleRate": 16000 }))
        .await;
    assert_eq!(chunk["ok"], true, "{chunk}");
    let finished = phone.rpc_params(&token, "f", "speech.dictation.finish", json!({ "dictationId": "d1" })).await;
    assert_eq!(finished["ok"], true, "{finished}");
    assert_eq!(finished["result"]["text"], "[TRANSCRIBED] hello over the link", "{finished}");

    // The setup and model writes stay unserved: the mic path never calls them.
    let setup = phone.rpc(&token, "x", "speech.dictation.setup").await;
    assert_eq!(setup["error"]["code"], "method_not_found", "{setup}");
}

#[tokio::test(flavor = "multi_thread")]
async fn dictation_refusals_and_server_errors_reach_the_phone() {
    let url = fake_whisper(503, "{}", Duration::ZERO);
    let b = dictation_bridge("dictation-503", whisper_at(url, Duration::from_secs(5))).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 12).await;
    assert_eq!(phone.auth(&token).await["type"], "e2ee_authenticated");
    phone.rpc_params(&token, "s", "speech.dictation.start", json!({ "dictationId": "d2" })).await;

    // Six seconds is over the five-second chunk limit: refused before it is decoded.
    let long = phone
        .rpc_params(&token, "c", "speech.dictation.chunk", json!({ "dictationId": "d2", "audioBase64": speech_chunk(6000), "sampleRate": 16000 }))
        .await;
    assert_eq!(long["error"]["code"], "bad_request", "{long}");
    assert_eq!(long["error"]["message"], "Dictation audio chunk is longer than 5 seconds", "{long}");

    let ok = phone
        .rpc_params(&token, "c2", "speech.dictation.chunk", json!({ "dictationId": "d2", "audioBase64": speech_chunk(1000), "sampleRate": 16000 }))
        .await;
    assert_eq!(ok["ok"], true, "{ok}");
    let finished = phone.rpc_params(&token, "f", "speech.dictation.finish", json!({ "dictationId": "d2" })).await;
    assert_eq!(finished["ok"], false, "{finished}");
    assert_eq!(finished["error"]["code"], "unavailable", "{finished}");
    assert!(finished["error"]["message"].as_str().unwrap_or_default().contains("HTTP 503"), "{finished}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_whisper_times_out_over_the_encrypted_link() {
    let url = fake_whisper(200, r#"{"text":"late"}"#, Duration::from_secs(3));
    let b = dictation_bridge("dictation-slow", whisper_at(url, Duration::from_millis(300))).await;
    let (_, token) = pair(&b.dir, "phone");
    let mut phone = Phone::hello(&b, 13).await;
    assert_eq!(phone.auth(&token).await["type"], "e2ee_authenticated");
    phone.rpc_params(&token, "s", "speech.dictation.start", json!({ "dictationId": "d3" })).await;
    phone.rpc_params(&token, "c", "speech.dictation.chunk", json!({ "dictationId": "d3", "audioBase64": speech_chunk(1000), "sampleRate": 16000 })).await;
    let finished = phone.rpc_params(&token, "f", "speech.dictation.finish", json!({ "dictationId": "d3" })).await;
    assert_eq!(finished["error"]["code"], "unavailable", "{finished}");
    assert!(finished["error"]["message"].as_str().unwrap_or_default().contains("timed out"), "{finished}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_grants_a_machine_a_link_and_taking_it_back_stops_that_link() {
    use sesync::machines::{client, store};
    // Two bridges, a phone on each. The phone on B grants A a link, the phone on A accepts it, and A's machine client
    // reads B. Taking the link back on both sides stops that same client; a machine device cannot grant anything.
    let a = start("grant-a", Arc::new(FixedCatalog(vec![])), LOOPBACK).await;
    let b = start("grant-b", Arc::new(FixedCatalog(vec![])), LOOPBACK).await;
    let (_, phone_a) = pair(&a.dir, "phone");
    let (phone_b_device, phone_b) = pair(&b.dir, "phone");
    let mut on_a = Phone::hello(&a, 9).await;
    assert_eq!(on_a.auth(&phone_a).await, json!({ "type": "e2ee_authenticated" }));
    let mut on_b = Phone::hello(&b, 10).await;
    assert_eq!(on_b.auth(&phone_b).await, json!({ "type": "e2ee_authenticated" }));

    // A names B as B names itself: status.get's machineName, cut to the store's rule.
    let status = on_b.rpc(&phone_b, "s", "status.get").await;
    let name = store::sanitize_name(status["result"]["machineName"].as_str().unwrap());
    assert_eq!(name, "test-box");

    let granted = on_b.rpc_params(&phone_b, "g", "machines.grant", json!({ "label": "a-box" })).await;
    assert_eq!(granted["ok"], true, "{granted}");
    let grant_id = granted["result"]["deviceId"].as_str().unwrap().to_string();
    let grant_token = granted["result"]["deviceToken"].as_str().unwrap().to_string();
    assert_eq!(granted["result"]["publicKeyB64"], STANDARD.encode(b.server_public.as_bytes()));
    let kind = devices::list(&b.dir).unwrap().into_iter().find(|d| d.id == grant_id).unwrap().kind;
    assert_eq!(kind, devices::Kind::Machine);

    let accepted = on_a.rpc_params(&phone_a, "acc", "machines.accept", json!({
        "name": name,
        "endpoint": format!("ws://{}", b.addr),
        "deviceToken": grant_token,
        "publicKeyB64": granted["result"]["publicKeyB64"],
        "grantDeviceId": grant_id,
    })).await;
    assert_eq!(accepted["result"], json!({ "name": "test-box", "answers": true }), "{accepted}");

    let listed = on_a.rpc_params(&phone_a, "l", "machines.list", json!({})).await;
    assert_eq!(
        listed["result"]["machines"],
        json!([{ "name": "test-box", "endpoint": format!("ws://{}", b.addr), "grantDeviceId": grant_id }]),
        "{listed}"
    );
    assert!(!listed.to_string().contains(&grant_token), "the list never carries a token");

    // A's `sesync machine` client reads B with the saved entry.
    let saved = store::load(&a.dir.join("machines.json")).unwrap().into_iter().find(|m| m.name == "test-box").unwrap();
    assert_eq!(saved.grant_device_id.as_deref(), Some(grant_id.as_str()));
    let (mut writer, mut reader) = client::connect(&saved).await.expect("the granted machine authenticates");
    let rows = client::call(&mut writer, &mut reader, "worktree.ps", json!({})).await.unwrap();
    assert_eq!(rows["worktrees"], json!([]));
    // As a machine device it cannot mint access for another machine, over the wire.
    let refused = client::call(&mut writer, &mut reader, "machines.grant", json!({ "label": "third" })).await.unwrap_err();
    assert!(refused.starts_with("forbidden"), "{refused}");
    let refused = client::call(&mut writer, &mut reader, "devices.revoke", json!({ "deviceId": phone_b_device.id })).await.unwrap_err();
    assert!(refused.starts_with("forbidden"), "{refused}");

    // The phone on B cannot revoke a phone through the grant method.
    let not_a_grant = on_b.rpc_params(&phone_b, "rp", "devices.revoke", json!({ "deviceId": phone_b_device.id })).await;
    assert_eq!(not_a_grant["error"]["code"], "forbidden", "{not_a_grant}");

    // Taking the link back: A forgets it and returns the grant id; B revokes that device.
    let forgot = on_a.rpc_params(&phone_a, "f", "machines.forget", json!({ "name": "test-box" })).await;
    assert_eq!(forgot["result"], json!({ "grantDeviceId": grant_id }), "{forgot}");
    let revoked = on_b.rpc_params(&phone_b, "r", "devices.revoke", json!({ "deviceId": grant_id })).await;
    assert_eq!(revoked["result"], json!({ "revoked": true }), "{revoked}");

    // The same client is refused on its next frame, and a new connection with the saved entry cannot authenticate.
    let after = client::call(&mut writer, &mut reader, "worktree.ps", json!({})).await.unwrap_err();
    assert!(after.starts_with("unauthorized"), "{after}");
    let again = client::connect(&saved).await.err().expect("a revoked grant is refused at auth");
    assert!(again.contains("refused"), "{again}");
    assert!(store::load(&a.dir.join("machines.json")).unwrap().is_empty());
}
