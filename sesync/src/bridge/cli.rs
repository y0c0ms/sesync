//! `sesync bridge …`: pair a phone, list or revoke paired phones, and serve. Exit codes follow
//! `sesync team`: 0 on success, 2 on a usage mistake, 1 on a failure.

use super::catalog::Catalog;
use super::herdr::{self, Herdr};
use super::offer::{self, Offer};
use super::rpc::{self, Ctx};
use super::turns::{LockTools, Turns};
use super::{dictation, devices, keys, net, notify, ntfy, server, start, uploads, usage, DEFAULT_ALLOW, DEFAULT_ENDPOINT, DEFAULT_PORT};
use crate::qr;
use qrcodegen::QrCode;
use std::collections::HashMap;
use std::net::SocketAddr;
use crate::harness::Locations;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::sync::Arc;
use tokio::net::TcpListener;

/// The homelab ntfy server and topic that finished and needs-input events are pushed to, and the command
/// that prints the topic's publish token. The token is read once at startup and never logged.
const DEFAULT_NTFY_URL: &str = "http://192.168.100.178:30181";
const DEFAULT_NTFY_TOPIC: &str = "agents";
const DEFAULT_NTFY_TOKEN_COMMAND: &str =
    "sudo -n systemd-creds decrypt --name=agenthub-ntfy-token /opt/homelab-creds/agenthub-ntfy-token.cred -";
/// How often plan usage is refreshed with `omp usage --json`, unless `--usage-every` says otherwise.
const DEFAULT_USAGE_EVERY_SECS: u64 = 600;
/// How often the pane list is re-read to follow panes that open or close. Agent statuses arrive as herdr
/// events in between, so a turn is seen however short it is.
const NOTIFY_EVERY: Duration = Duration::from_secs(1);
/// The flags of `serve` that take no value.
const SERVE_FLAGS: &[&str] = &["no-ntfy", "no-usage"];
const USAGE: &str = "usage: sesync bridge pair [--endpoint ws://HOST:PORT] [--label NAME]
       sesync bridge devices
       sesync bridge revoke <device-id>
       sesync bridge serve [--listen ADDR:PORT] [--allow CIDR,CIDR,...] [--herdr-socket PATH]
                           [--ntfy-url URL] [--ntfy-topic NAME] [--ntfy-token-command CMD] [--no-ntfy]
                           [--usage-every SECONDS] [--omp PATH] [--no-usage]";

enum Fail {
    Usage(String),
    Failed(String),
}

/// What `serve` reads sessions from: the catalog the phone lists, and the harness locations and home that resuming a
/// conversation reads (`herdr_open::plan`).
pub struct Sessions {
    pub catalog: Arc<dyn Catalog>,
    pub locations: Locations,
    pub home: PathBuf,
}

/// Run one `sesync bridge` command. `dir` is the bridge's state directory; `sessions` is called only by `serve`, so
/// the other commands never read a session store. `locks` says where sesync-lock is and where its claims live; `serve`
/// uses it for prompt turns. `machines` is `machines.json`, which the `machines.*` RPCs of `serve` keep.
pub fn run(args: &[String], dir: &Path, machines: &Path, locks: LockTools, sessions: impl FnOnce() -> Sessions) -> i32 {
    let result = match args.first().map(String::as_str) {
        Some("pair") => pair(&args[1..], dir),
        Some("devices") => list_devices(&args[1..], dir),
        Some("revoke") => revoke(&args[1..], dir),
        Some("serve") => serve(&args[1..], dir, machines, sessions(), locks),
        _ => Err(Fail::Usage("expected a command".into())),
    };
    match result {
        Ok(()) => 0,
        Err(Fail::Usage(message)) => {
            eprintln!("sesync bridge: {message}\n{USAGE}");
            2
        }
        Err(Fail::Failed(message)) => {
            eprintln!("sesync bridge: {message}");
            1
        }
    }
}

fn pair(args: &[String], dir: &Path) -> Result<(), Fail> {
    let opts = options(args, &["endpoint", "label"], &[])?;
    let endpoint = opts.get("endpoint").cloned().unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
    offer::check_endpoint(&endpoint).map_err(Fail::Usage)?;
    let label = opts.get("label").cloned().unwrap_or_else(|| "phone".to_string());

    let keys = keys::load_or_create(&dir.join("server.key")).map_err(Fail::Failed)?;
    let (device, token) = devices::create(dir, &label, devices::Kind::Phone, devices::now_secs()).map_err(Fail::Failed)?;
    let offer = Offer { endpoint, device_token: token, public_key_b64: keys.public_key_b64() };

    println!("paired {label:?} as device {}", device.id);
    println!("The device token is in this offer and is shown only now; the bridge keeps its SHA-256.");
    println!("offer: {}", offer.to_json());
    println!("url:   {}", offer.url());
    let code = QrCode::encode_text(&offer.url(), qr::ECC)
        .map_err(|_| Fail::Failed("the offer is too long for a QR code".into()))?;
    // Light modules are painted, so a dark terminal shows dark modules on a light field, the way
    // phone scanners read best (see qr::Ink).
    for row in qr::rows_inked(&code, qr::QUIET, qr::Ink::Light) {
        println!("{row}");
    }
    Ok(())
}

fn list_devices(args: &[String], dir: &Path) -> Result<(), Fail> {
    no_arguments(args)?;
    let list = devices::list(dir).map_err(Fail::Failed)?;
    if list.is_empty() {
        println!("no paired devices");
    }
    for d in list {
        let kind = match d.kind {
            devices::Kind::Phone => "phone",
            devices::Kind::Machine => "machine",
        };
        println!("{}  {kind}  {:?}  paired at unix time {}", d.id, d.label, d.created);
    }
    Ok(())
}

fn revoke(args: &[String], dir: &Path) -> Result<(), Fail> {
    let [id] = args else {
        return Err(Fail::Usage("revoke takes exactly one device id (see `sesync bridge devices`)".into()));
    };
    let device = devices::revoke(dir, id).map_err(Fail::Failed)?;
    println!("revoked device {} ({:?}); its token stops working on the next frame", device.id, device.label);
    Ok(())
}

fn serve(args: &[String], dir: &Path, machines: &Path, sessions: Sessions, locks: LockTools) -> Result<(), Fail> {
    let opts = options(
        args,
        &["listen", "allow", "herdr-socket", "ntfy-url", "ntfy-topic", "ntfy-token-command", "usage-every", "omp"],
        SERVE_FLAGS,
    )?;
    let listen_text = opts.get("listen").cloned().unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}"));
    let listen: SocketAddr = listen_text.parse()
        .map_err(|_| Fail::Usage(format!("{listen_text}: not an address:port")))?;
    let allow_text = opts.get("allow").cloned().unwrap_or_else(|| DEFAULT_ALLOW.to_string());
    let allow = net::parse_list(&allow_text).map_err(Fail::Usage)?;
    let herdr_socket = opts.get("herdr-socket").map_or_else(herdr::default_socket, Into::into);

    let keys = keys::load_or_create(&dir.join("server.key")).map_err(Fail::Failed)?;
    let paired = devices::list(dir).map_err(Fail::Failed)?.len();
    let push = push_config(&opts);
    let Sessions { catalog, locations, home } = sessions;
    let whisper = dictation::Whisper::from_env();
    eprintln!("bridge: phone dictation through {} ({})", whisper.url, whisper.model);
    let ctx = Ctx {
        catalog,
        herdr: Arc::new(herdr::Socket::new(herdr_socket.clone())) as Arc<dyn Herdr>,
        host: rpc::Host { machine_name: machine_name(), platform: platform() },
        locks,
        turns: Turns::default(),
        dir: dir.to_path_buf(),
        machines: machines.to_path_buf(),
        notify: notify::Hub::new(push),
        usage: usage::Store::default(),
        uploads: uploads::Store::default(),
        dictation: dictation::Dictation::new(whisper),
        launch: start::Launch::from_env(locations, home, &herdr_socket),
    };
    if !opts.contains_key("no-usage") {
        let every = usage_every(&opts)?;
        let omp = PathBuf::from(opts.get("omp").map_or("omp", String::as_str));
        eprintln!("bridge: plan usage from `{} usage --json` every {}s", omp.display(), every.as_secs());
        usage::spawn_poller(ctx.usage.clone(), omp, every);
    }
    notify::spawn_watcher(ctx.clone(), NOTIFY_EVERY);
    let shared = Arc::new(server::Shared { dir: dir.to_path_buf(), keys, allow, ctx });

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()
        .map_err(|e| Fail::Failed(format!("cannot start the runtime: {e}")))?;
    runtime.block_on(async move {
        let listener = TcpListener::bind(listen).await
            .map_err(|e| Fail::Failed(format!("cannot listen on {listen}: {e}")))?;
        let bound = listener.local_addr().map_err(|e| Fail::Failed(e.to_string()))?;
        eprintln!("bridge: listening on ws://{bound}; allowed networks {allow_text}");
        eprintln!("bridge: herdr socket {}", herdr_socket.display());
        if paired == 0 {
            eprintln!("bridge: no paired devices yet; run `sesync bridge pair`");
        }
        tokio::select! {
            () = server::serve(listener, shared) => {}
            _ = tokio::signal::ctrl_c() => eprintln!("bridge: stopped"),
        }
        Ok(())
    })
}

/// `--name value` pairs for the names in `known`, and bare `--flag`s for the names in `flags` (stored as
/// "true"). Anything else is a usage error.
fn options(args: &[String], known: &[&str], flags: &[&str]) -> Result<HashMap<String, String>, Fail> {
    let mut out = HashMap::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let name = arg.strip_prefix("--").ok_or_else(|| Fail::Usage(format!("unexpected argument {arg:?}")))?;
        if flags.contains(&name) {
            out.insert(name.to_string(), "true".to_string());
            continue;
        }
        if !known.contains(&name) {
            return Err(Fail::Usage(format!("unexpected argument {arg:?}")));
        }
        let value = rest.next().ok_or_else(|| Fail::Usage(format!("--{name} needs a value")))?;
        out.insert(name.to_string(), value.clone());
    }
    Ok(out)
}

/// The ntfy push this serve run makes, or none. The token command runs once, here at startup. When it
/// fails the push stays off and the reason is said, never the token.
fn push_config(opts: &HashMap<String, String>) -> Option<ntfy::Config> {
    if opts.contains_key("no-ntfy") {
        return None;
    }
    let url = opts.get("ntfy-url").map_or(DEFAULT_NTFY_URL, String::as_str);
    let topic = opts.get("ntfy-topic").map_or(DEFAULT_NTFY_TOPIC, String::as_str);
    let command = opts.get("ntfy-token-command").map_or(DEFAULT_NTFY_TOKEN_COMMAND, String::as_str);
    match ntfy::load(url, topic, command) {
        Ok(config) => {
            eprintln!("bridge: ntfy push to {url}, topic {topic}");
            Some(config)
        }
        Err(reason) => {
            eprintln!("bridge: ntfy push off: {reason}");
            None
        }
    }
}

/// The plan-usage refresh interval from `--usage-every`, in whole seconds above zero.
fn usage_every(opts: &HashMap<String, String>) -> Result<Duration, Fail> {
    match opts.get("usage-every") {
        None => Ok(Duration::from_secs(DEFAULT_USAGE_EVERY_SECS)),
        Some(text) => text
            .parse::<u64>()
            .ok()
            .filter(|secs| *secs > 0)
            .map(Duration::from_secs)
            .ok_or_else(|| Fail::Usage(format!("--usage-every {text}: not a number of seconds above 0"))),
    }
}

fn no_arguments(args: &[String]) -> Result<(), Fail> {
    match args.first() {
        Some(arg) => Err(Fail::Usage(format!("unexpected argument {arg:?}"))),
        None => Ok(()),
    }
}

/// This machine's name as the phone shows it. The kernel's hostname, else the environment's
/// (`HOSTNAME`, or `COMPUTERNAME` on Windows).
fn machine_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .or_else(|| std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "this machine".to_string())
}

/// Orca's platform name for this OS. Anything else is `unknown`, which the app ignores rather than
/// refusing the host over.
fn platform() -> &'static str {
    match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "win32",
        _ => "unknown",
    }
}
