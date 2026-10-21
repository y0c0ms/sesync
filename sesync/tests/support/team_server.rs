//! An in-process team server for the client end-to-end tests: the real router over a real TCP
//! socket on an ephemeral port, the in-memory store, and a clock the test sets. The same pattern as
//! sesync-team/tests/socket.rs, so the client's own code paths (team_cli, team_join, team_push,
//! team_watch, team_pass and the sesync binary) run against real HTTP without a database.
#![allow(dead_code)]

use sesync_team::server::{self, AppState, Clock};
use sesync_team::service::{MemoryStore, Service};
use sesync_team::totp::code_at;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Long enough for the server's 32-character minimum.
pub const ADMIN_TOKEN: &str = "audit-admin-token-000000000000000000000000";
pub const DAY: u64 = 86_400;

pub struct TeamServer {
    pub base: String,
    pub state: Arc<AppState<MemoryStore>>,
    now: Arc<AtomicU64>,
    rt: Option<tokio::runtime::Runtime>,
}

impl TeamServer {
    /// Start a server whose clock reads `now` until the test moves it.
    pub fn start(now: u64) -> TeamServer {
        let clock_now = Arc::new(AtomicU64::new(now));
        let c = clock_now.clone();
        let clock: Clock = Arc::new(move || c.load(Ordering::SeqCst));
        let state = Arc::new(AppState {
            service: Mutex::new(Service::new(MemoryStore::default())),
            clock,
            metrics: Default::default(),
            admin_token: Some(ADMIN_TOKEN.to_string()),
        });
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        let app = server::router(state.clone());
        rt.spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        TeamServer { base: format!("http://{addr}"), state, now: clock_now, rt: Some(rt) }
    }

    pub fn now(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    pub fn set_now(&self, t: u64) {
        self.now.store(t, Ordering::SeqCst);
    }

    /// The team's authenticator secret, as the phone holds it after scanning the QR.
    pub fn secret(&self, space: &str) -> Vec<u8> {
        self.state.service.lock().unwrap().space_secret(space).expect("the team exists")
    }

    /// The code the authenticator shows at the server's current time.
    pub fn code(&self, space: &str) -> String {
        code_at(&self.secret(space), self.now())
    }

    /// `sesync team create`, through the CLI's own code. Returns the one-time otpauth URI.
    pub fn create(&self, admin_dir: &Path, name: &str, members: &[&str]) -> String {
        let cmd = sesync::team_cli::Command::Create {
            name: name.to_string(),
            members: members.iter().map(|m| m.to_string()).collect(),
        };
        let (_, uri) = sesync::team_cli::run_detailed(&cmd, &self.base, ADMIN_TOKEN, admin_dir)
            .unwrap_or_else(|e| panic!("team '{name}' is created: {e}"));
        uri.expect("the server returns the authenticator URI")
    }

    /// `sesync team join`, through the client's own code, with the code for the current time.
    pub fn join(&self, config_dir: &Path, name: &str, member: &str, machine: &str) -> PathBuf {
        sesync::team_join::join(&self.base, name, member, machine, &self.code(name), config_dir)
            .unwrap_or_else(|e| panic!("'{machine}' joins '{name}': {e}"))
            .path
    }

    /// `sesync team approve`, through the CLI's own code, with the fingerprint given.
    pub fn approve(&self, admin_dir: &Path, name: &str, member: &str, machine: &str, fingerprint: Option<&str>) -> String {
        let cmd = sesync::team_cli::Command::Approve {
            name: name.to_string(), member: member.to_string(), machine: machine.to_string(),
            fingerprint: fingerprint.map(str::to_string),
        };
        sesync::team_cli::run_detailed(&cmd, &self.base, ADMIN_TOKEN, admin_dir)
            .unwrap_or_else(|e| panic!("approve '{machine}' in '{name}': {e}"))
            .0
    }

    /// A device becomes a listed member the only way there is: it joins (a request), the admin
    /// approves it after checking its fingerprint, and the device picks up the signed list the way
    /// the daemon does (`check_approval`). A key the team already lists is re-authenticated at once.
    pub fn admit(&self, admin_dir: &Path, config_dir: &Path, name: &str, member: &str, machine: &str) -> PathBuf {
        let joined = sesync::team_join::join(&self.base, name, member, machine, &self.code(name), config_dir)
            .unwrap_or_else(|e| panic!("'{machine}' joins '{name}': {e}"));
        if joined.pending {
            self.approve(admin_dir, name, member, machine, Some(&joined.fingerprint));
            let r = sesync::team_join::check_approval(None, config_dir, name)
                .unwrap_or_else(|e| panic!("'{machine}' picks up its approval in '{name}': {e}"));
            assert!(matches!(r, Some(sesync::team_join::Approval::Approved { .. })), "approved: {r:?}");
        }
        joined.path
    }
}

impl Drop for TeamServer {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// A fresh, unique directory for one test.
pub fn sandbox(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("sesync-team-audit-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The wall clock in seconds, for tests that run the sesync binary (which reads the real time).
pub fn wall_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// The `sesync` binary with every store and setting pointed into `sandbox`, so nothing of the
/// operator's install is read or written and no real team server is contacted.
pub fn sesync_cmd(sandbox: &Path, config_dir: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_sesync"));
    for (k, _) in std::env::vars() {
        if k.starts_with("SESYNC_") {
            c.env_remove(k);
        }
    }
    c.env("HOME", sandbox.join("home"))
        .env("USERPROFILE", sandbox.join("home"))
        .env("SESYNC_DIR", sandbox.join("state"))
        .env("SESYNC_CONFIG_DIR", config_dir)
        .env("SESYNC_LOCK", sandbox.join("no-sesync_lock"))
        .env("SESYNC_LOCK_DIR", sandbox.join("locks"))
        .env("SESYNC_CONV", sandbox.join("no-sesync-conv"))
        .env("OMP_AGENT_DIR", sandbox.join("omp"))
        .env("PI_AGENT_DIR", sandbox.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sandbox.join("claude"))
        .env("ANTIGRAVITY_CLI_HOME", sandbox.join("antigravity"))
        .env("OPENCODE_DB", sandbox.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", sandbox.join("no-opencode-bin"))
        .stdin(std::process::Stdio::null());
    c
}
