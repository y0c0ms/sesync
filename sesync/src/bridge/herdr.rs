//! The herdr socket client. herdr serves one JSON request per line on its API socket and answers each
//! with one line: `{"id","result":{"type":…}}` or `{"id","error":{"code","message"}}`. Every call
//! here opens its own connection, a few milliseconds locally, so a hung call stalls only itself.
//! The socket is a Unix socket; on Windows herdr serves a named pipe instead, named after the same
//! path (see `Conn`).
//!
//! The bridge codes against the [`Herdr`] trait, so tests answer with canned JSON and the real
//! socket is used only in production and in the sandboxed integration test. Pane and read
//! parsing lives here too, as pure functions over the `result` object.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long one herdr call may run. herdr answers in milliseconds; a call that hangs must not hold a
/// phone's request open.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The socket herdr listens on when nothing overrides it: `$HERDR_SOCKET_PATH` (herdr sets it for its
/// own panes), else `herdr/herdr.sock` in herdr's configuration directory: `$HOME/.config`, and on
/// Windows `%XDG_CONFIG_HOME%` when set, else `%APPDATA%`, as herdr itself decides.
pub fn default_socket() -> PathBuf {
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|p| !p.is_empty())
            .or_else(|| std::env::var_os("APPDATA").filter(|p| !p.is_empty()));
        if let Some(base) = base {
            return PathBuf::from(base).join("herdr").join("herdr.sock");
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".config").join("herdr").join("herdr.sock")
}

/// One herdr API call. The bridge's only door to herdr; tests substitute canned answers.
pub trait Herdr: Send + Sync {
    /// Run `method` with `params` and return the reply's `result`, or herdr's error message.
    fn call(&self, method: &str, params: Value) -> Result<Value, String>;

    /// Names the herdr server these calls reach. The bridge talks to one server, so the default is one name for
    /// all of its calls; a test double that stands for several servers names each of them.
    fn scope(&self) -> String {
        String::new()
    }

    /// Pane agent-status changes for `pane_ids`. Canned answers have none, so the default refuses.
    fn status_events(&self, _pane_ids: &[String]) -> Result<Box<dyn StatusSource>, String> {
        Err("herdr status events are not available here".into())
    }
}

/// A stream of pane agent-status changes, read with a timeout.
pub trait StatusSource: Send {
    /// The next `(pane_id, agent_status)`, or None when `wait` passes first. An error ends the stream.
    fn next(&mut self, wait: Duration) -> Result<Option<(String, String)>, String>;
}

/// herdr over its API socket.
pub struct Socket {
    path: PathBuf,
}

impl Socket {
    pub fn new(path: impl Into<PathBuf>) -> Socket {
        Socket { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// One connection to herdr's API. On Unix it is the socket file. On Windows herdr (through the
/// `interprocess` crate) serves a named pipe called `\\.\pipe\` followed by the socket path, and the
/// file at that path holds only a marker. A Windows pipe has no read timeout, so waiting for a line
/// polls `PeekNamedPipe`, as herdr's own client does.
struct Conn {
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    pipe: std::fs::File,
    /// Bytes read past the last whole line.
    pending: Vec<u8>,
}

impl Conn {
    fn open(path: &Path) -> Result<Conn, String> {
        let unreachable = |e: std::io::Error| format!("herdr is not reachable at {} ({e})", path.display());
        #[cfg(unix)]
        {
            let stream = std::os::unix::net::UnixStream::connect(path).map_err(unreachable)?;
            stream.set_write_timeout(Some(CALL_TIMEOUT)).map_err(|e| e.to_string())?;
            Ok(Conn { stream, pending: Vec::new() })
        }
        #[cfg(windows)]
        {
            let name = format!(r"\\.\pipe\{}", path.display());
            let deadline = Instant::now() + CALL_TIMEOUT;
            loop {
                match std::fs::OpenOptions::new().read(true).write(true).open(&name) {
                    Ok(pipe) => return Ok(Conn { pipe, pending: Vec::new() }),
                    // ERROR_PIPE_BUSY: every instance is taken; herdr opens the next one as it accepts.
                    Err(e) if e.raw_os_error() == Some(231) && Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => return Err(unreachable(e)),
                }
            }
        }
    }

    fn send(&mut self, request: &Value) -> std::io::Result<()> {
        use std::io::Write;
        let line = format!("{request}\n");
        #[cfg(unix)]
        let out = &mut self.stream;
        #[cfg(windows)]
        let out = &mut self.pipe;
        out.write_all(line.as_bytes())
    }

    /// One whole line, or None when `wait` passes first; a partial line waits in `pending`. An
    /// `UnexpectedEof` error means herdr closed the connection.
    fn line(&mut self, wait: Duration) -> std::io::Result<Option<String>> {
        let deadline = Instant::now() + wait;
        let mut buf = [0u8; 16 * 1024];
        loop {
            if let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=end).collect();
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            match self.read_some(&mut buf, left)? {
                None => return Ok(None),
                Some(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                Some(n) => self.pending.extend_from_slice(&buf[..n]),
            }
        }
    }

    /// Read what arrives within `wait`: the byte count (0 when herdr closed the connection), or None.
    #[cfg(unix)]
    fn read_some(&mut self, buf: &mut [u8], wait: Duration) -> std::io::Result<Option<usize>> {
        use std::io::Read;
        self.stream.set_read_timeout(Some(wait))?;
        match self.stream.read(buf) {
            Ok(n) => Ok(Some(n)),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(e),
        }
    }

    #[cfg(windows)]
    fn read_some(&mut self, buf: &mut [u8], wait: Duration) -> std::io::Result<Option<usize>> {
        use std::io::Read;
        use std::os::windows::io::AsRawHandle;
        let deadline = Instant::now() + wait;
        loop {
            let mut available: u32 = 0;
            let ok = unsafe {
                windows_sys::Win32::System::Pipes::PeekNamedPipe(
                    self.pipe.as_raw_handle() as _,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let e = std::io::Error::last_os_error();
                // ERROR_INVALID_HANDLE, ERROR_BROKEN_PIPE, ERROR_BAD_PIPE, ERROR_PIPE_NOT_CONNECTED.
                return match e.raw_os_error() {
                    Some(6 | 109 | 232 | 233) => Ok(Some(0)),
                    _ => Err(e),
                };
            }
            if available > 0 {
                // At most what is waiting, so the read never blocks.
                let n = buf.len().min(available as usize);
                return self.pipe.read(&mut buf[..n]).map(Some);
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Herdr for Socket {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut conn = Conn::open(&self.path)?;
        let request = json!({ "id": "sesync", "method": method, "params": params });
        conn.send(&request).map_err(|e| format!("herdr did not take {method} ({e})"))?;
        match conn.line(CALL_TIMEOUT) {
            Ok(Some(line)) => reply(method, &line),
            Ok(None) => Err(format!("herdr did not answer {method} within {} s", CALL_TIMEOUT.as_secs())),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => reply(method, ""),
            Err(e) => Err(format!("herdr did not answer {method} ({e})")),
        }
    }

    fn status_events(&self, pane_ids: &[String]) -> Result<Box<dyn StatusSource>, String> {
        StatusEvents::open(&self.path, pane_ids).map(|events| Box::new(events) as Box<dyn StatusSource>)
    }
}

impl StatusSource for StatusEvents {
    fn next(&mut self, wait: Duration) -> Result<Option<(String, String)>, String> {
        StatusEvents::next(self, wait)
    }
}

/// A subscription to `pane.agent_status_changed` for a set of panes. herdr pushes one event line per
/// change, in order, so no status between two reads is lost. Sampling cannot promise that: an agent
/// turn can start and finish between two samples.
pub struct StatusEvents {
    conn: Conn,
}

impl StatusEvents {
    /// Subscribe to the status of each pane in `pane_ids` (herdr pane ids such as `w1:p1`).
    pub fn open(path: &Path, pane_ids: &[String]) -> Result<StatusEvents, String> {
        let mut conn = Conn::open(path)?;
        let subscriptions: Vec<Value> = pane_ids
            .iter()
            .map(|id| json!({ "type": "pane.agent_status_changed", "pane_id": id }))
            .collect();
        let request = json!({ "id": "sesync-events", "method": "events.subscribe", "params": { "subscriptions": subscriptions } });
        conn.send(&request).map_err(|e| format!("herdr did not take the event subscription ({e})"))?;
        let mut events = StatusEvents { conn };
        let ack = events.line(CALL_TIMEOUT)?.ok_or("herdr did not acknowledge the event subscription")?;
        let ack: Value = serde_json::from_str(ack.trim()).map_err(|e| format!("herdr's subscription reply: {e}"))?;
        if ack["result"]["type"] != "subscription_started" {
            return Err(format!("herdr refused the event subscription: {}", ack["error"]["message"]));
        }
        Ok(events)
    }

    /// The next pane status change as `(pane_id, agent_status)`, or None when `wait` passes without one.
    /// Other events are skipped. An error means the connection ended and the subscription must be reopened.
    pub fn next(&mut self, wait: Duration) -> Result<Option<(String, String)>, String> {
        loop {
            let Some(line) = self.line(wait)? else { return Ok(None) };
            let event: Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if event["event"] != "pane.agent_status_changed" {
                continue;
            }
            let data = &event["data"];
            if let (Some(pane), Some(status)) = (data["pane_id"].as_str(), data["agent_status"].as_str()) {
                return Ok(Some((pane.to_string(), status.to_string())));
            }
        }
    }

    /// One whole line from the socket, or None when `wait` passes first.
    fn line(&mut self, wait: Duration) -> Result<Option<String>, String> {
        self.conn.line(wait).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => "herdr closed the event subscription".to_string(),
            _ => format!("herdr's event stream failed ({e})"),
        })
    }
}

/// Read one reply line: its `result`, or the error message herdr gave.
pub fn reply(method: &str, line: &str) -> Result<Value, String> {
    let text = line.trim();
    if text.is_empty() {
        return Err(format!("herdr closed the connection without answering {method}"));
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|_| format!("herdr answered {method} with something other than JSON"))?;
    if let Some(err) = value.get("error") {
        return Err(err["message"].as_str().unwrap_or("herdr refused the call").to_string());
    }
    value.get("result").cloned().ok_or_else(|| format!("herdr's reply to {method} has no result"))
}

fn text(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().map(str::to_string)
}

/// A pane as herdr lists it, with the fields the bridge maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub pane_id: String,
    /// Stable for the life of the terminal; the bridge's terminal handle.
    pub terminal_id: String,
    pub tab_id: String,
    pub focused: bool,
    /// herdr's status: `idle`, `working`, `blocked`, `done` or `unknown`.
    pub agent_status: String,
    /// The agent herdr detected in the pane, when it detected one.
    pub agent: Option<String>,
    pub title: Option<String>,
    /// The directory the pane's foreground process runs in, when herdr reports it.
    pub foreground_cwd: Option<String>,
    /// The visible rows, when herdr reports the pane's scroll geometry.
    pub rows: Option<u64>,
}

/// One pane object of herdr's `pane_list` or `pane_info` results. None when an identifying field is
/// missing.
pub fn parse_pane(p: &Value) -> Option<Pane> {
    Some(Pane {
        pane_id: text(p, "pane_id")?,
        terminal_id: text(p, "terminal_id")?,
        tab_id: text(p, "tab_id")?,
        focused: p["focused"].as_bool().unwrap_or(false),
        agent_status: text(p, "agent_status").unwrap_or_else(|| "unknown".into()),
        agent: text(p, "agent").filter(|a| !a.is_empty()),
        title: text(p, "title").or_else(|| text(p, "terminal_title")).filter(|t| !t.trim().is_empty()),
        foreground_cwd: text(p, "foreground_cwd").filter(|d| !d.trim().is_empty()),
        rows: p["scroll"]["viewport_rows"].as_u64(),
    })
}

/// The panes of a `pane_list` result. Entries without the identifying fields are skipped.
pub fn parse_panes(result: &Value) -> Result<Vec<Pane>, String> {
    let panes = result["panes"].as_array().ok_or("herdr's pane list has no panes")?;
    Ok(panes.iter().filter_map(parse_pane).collect())
}

/// The argument lists of the foreground processes in a `pane_process_info` result. Empty argument
/// lists are dropped. A session-file argument (`*.jsonl`) is replaced by the one top-level session file the process
/// has open when they differ (`open_session`, given the pid and herdr's argv): omp started with `--resume <file>`
/// moves to a new file when the session is open elsewhere, and keeps its argv. Matching by the stale argv attached the
/// pane to the abandoned copy and left the live conversation without a pane (2026-10-29).
fn parse_process_argvs_with(
    result: &Value,
    open_session: impl Fn(u64, &[String]) -> Option<String>,
) -> Result<Vec<Vec<String>>, String> {
    let procs = result["process_info"]["foreground_processes"]
        .as_array()
        .ok_or("herdr's process info has no foreground processes")?;
    Ok(procs
        .iter()
        .filter_map(|p| {
            let argv: Vec<String> = p["argv"].as_array()?.iter().filter_map(|a| a.as_str().map(str::to_string)).collect();
            if argv.is_empty() {
                return None;
            }
            let open = argv
                .iter()
                .any(|a| a.ends_with(".jsonl"))
                .then(|| p["pid"].as_u64().and_then(|pid| open_session(pid, &argv)))
                .flatten();
            Some(match open {
                Some(open) => argv.into_iter().map(|a| if a.ends_with(".jsonl") { open.clone() } else { a }).collect(),
                None => argv,
            })
        })
        .collect())
}

/// The session file local process `pid` runs when herdr shows it started with a session file in `argv`: omp, asked to
/// resume a session another omp already has, moves it to a new file whose header names the old session as
/// `parentSession`, and keeps its argv. So the process runs the earliest such copy created after the process started,
/// or the argv's file when there is none. The process must be the one herdr described (its command line equals
/// `argv`), so a pid from another machine's herdr never reads an unrelated local process. None when nothing moved.
#[cfg(target_os = "linux")]
fn open_session_file(pid: u64, argv: &[String]) -> Option<String> {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let cmdline = std::fs::read(proc.join("cmdline")).ok()?;
    let local: Vec<&[u8]> = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
    if local.len() != argv.len() || local.iter().zip(argv).any(|(l, a)| *l != a.as_bytes()) {
        return None;
    }
    let file = argv.iter().find(|a| a.ends_with(".jsonl"))?;
    moved_copy(Path::new(file), process_start(&proc)?)
}

#[cfg(not(target_os = "linux"))]
fn open_session_file(_pid: u64, _argv: &[String]) -> Option<String> {
    None
}

/// When the process at `proc` (a `/proc/<pid>` directory) started, in Unix seconds.
#[cfg(target_os = "linux")]
fn process_start(proc: &Path) -> Option<f64> {
    let stat = std::fs::read_to_string(proc.join("stat")).ok()?;
    // Field 22, counted after the parenthesised command name, which may itself hold spaces.
    let ticks: f64 = stat[stat.rfind(')')? + 2..].split_whitespace().nth(19)?.parse().ok()?;
    let btime: f64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    // USER_HZ is 100 on every Linux the bridge runs on.
    Some(btime + ticks / 100.0)
}

/// The session record of an omp session file: its id, `parentSession` and creation time. It follows an optional
/// title record, within the file's first few kilobytes.
fn session_header(path: &Path) -> Option<(String, Option<String>, f64)> {
    use std::io::Read;
    let mut head = vec![0u8; 16 * 1024];
    let n = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    let text = String::from_utf8_lossy(&head[..n]);
    text.lines().take(3).find_map(|line| {
        let v: Value = serde_json::from_str(line).ok()?;
        (v["type"] == "session").then(|| {
            let ts = crate::prompts::parse_rfc3339(v["timestamp"].as_str().unwrap_or(""))?;
            Some((v["id"].as_str()?.to_string(), v["parentSession"].as_str().map(str::to_string), ts))
        })?
    })
}

/// The earliest session file next to `file` whose header names `file`'s session as its parent and was created at or
/// after `started` (Unix seconds; one second of slack for clock rounding). Its path, or None. `file` itself may be
/// gone (an abandoned copy deleted while the agent that moved away from it still runs): its id then comes from the
/// file name, which omp writes as `<created>_<id>.jsonl`.
fn moved_copy(file: &Path, started: f64) -> Option<String> {
    let id = match session_header(file) {
        Some((id, _, _)) => id,
        None => file.file_stem()?.to_str()?.rsplit_once('_')?.1.to_string(),
    };
    let mut best: Option<(f64, PathBuf)> = None;
    for entry in std::fs::read_dir(file.parent()?).ok()?.flatten() {
        let path = entry.path();
        if path == file || path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        // A copy made after the process started was written since then: skip reading every older file's header.
        let modified = entry.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
        if modified.is_none_or(|m| m.as_secs_f64() + 1.0 < started) {
            continue;
        }
        let Some((_, Some(parent), created)) = session_header(&path) else { continue };
        if parent == id && created + 1.0 >= started && best.as_ref().is_none_or(|(t, _)| created < *t) {
            best = Some((created, path));
        }
    }
    best.and_then(|(_, p)| p.to_str().map(str::to_string))
}

/// A local process already running the session `ident` (a resume argument such as omp's session file), herdr pane or
/// not: its command line, with a stale session file replaced by the copy it moved to (`open_session_file`), carries
/// `ident` as a flag's value. Every start of an agent checks this first: a second agent on one session splits it, and
/// omp answers a second open by forking the session into another file (three copies of one conversation on
/// 2026-10-29). The pid, or None. Never this process; off Linux always None.
pub fn agent_running(ident: &str) -> Option<u64> {
    if ident.is_empty() {
        return None;
    }
    local_argvs().into_iter().find_map(|(pid, argv)| {
        let argv = match open_session_file(pid, &argv) {
            Some(open) => argv.into_iter().map(|a| if a.ends_with(".jsonl") { open.clone() } else { a }).collect(),
            None => argv,
        };
        crate::herdr_open::argv_carries(&[argv], ident).then_some(pid)
    })
}

/// Every other local process's command line that names a session: a `*.jsonl` file or a flag value equal to an
/// identifier is checked by the caller; processes without arguments are skipped.
#[cfg(target_os = "linux")]
fn local_argvs() -> Vec<(u64, Vec<String>)> {
    let me = std::process::id() as u64;
    let Ok(procs) = std::fs::read_dir("/proc") else { return Vec::new() };
    procs
        .flatten()
        .filter_map(|e| {
            let pid: u64 = e.file_name().to_str()?.parse().ok()?;
            if pid == me {
                return None;
            }
            let raw = std::fs::read(e.path().join("cmdline")).ok()?;
            let argv: Vec<String> =
                raw.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
            (argv.len() > 1).then_some((pid, argv))
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn local_argvs() -> Vec<(u64, Vec<String>)> {
    Vec::new()
}

/// The text of a `pane_read` result.
pub fn parse_read(result: &Value) -> Result<String, String> {
    result["read"]["text"].as_str().map(str::to_string).ok_or_else(|| "herdr's read has no text".to_string())
}

pub fn list_panes(h: &dyn Herdr) -> Result<Vec<Pane>, String> {
    parse_panes(&h.call("pane.list", json!({}))?)
}

/// Every tab's label by tab id. A tab sesync opened for a session carries the session's tag in its label.
pub fn tab_labels(h: &dyn Herdr) -> Result<HashMap<String, String>, String> {
    let result = h.call("tab.list", json!({}))?;
    let tabs = result["tabs"].as_array().ok_or("herdr's tab list has no tabs")?;
    Ok(tabs.iter().filter_map(|t| Some((text(t, "tab_id")?, text(t, "label").unwrap_or_default()))).collect())
}

pub fn pane_info(h: &dyn Herdr, pane_id: &str) -> Result<Pane, String> {
    let result = h.call("pane.get", json!({ "pane_id": pane_id }))?;
    parse_pane(&result["pane"]).ok_or_else(|| format!("herdr's info for pane {pane_id} is incomplete"))
}

/// The pane's size in cells: the width and height of its rect in `pane.layout`. None when herdr answers
/// with an error or its layout has no such pane.
pub fn pane_size(h: &dyn Herdr, pane_id: &str) -> Option<(u32, u32)> {
    parse_pane_size(&h.call("pane.layout", json!({ "pane_id": pane_id })).ok()?, pane_id)
}

/// The size of `pane_id` in a `pane.layout` result, clamped to what a terminal shows (1..=1000 columns,
/// 1..=500 rows). None when the layout has no such pane or its rect is empty.
pub fn parse_pane_size(result: &Value, pane_id: &str) -> Option<(u32, u32)> {
    let pane = result["layout"]["panes"].as_array()?.iter().find(|p| p["pane_id"].as_str() == Some(pane_id))?;
    let cols = pane["rect"]["width"].as_u64().filter(|w| *w > 0)?;
    let rows = pane["rect"]["height"].as_u64().filter(|h| *h > 0)?;
    Some((cols.min(1000) as u32, rows.min(500) as u32))
}

pub fn foreground_argvs(h: &dyn Herdr, pane_id: &str) -> Result<Vec<Vec<String>>, String> {
    live_process_argvs(&h.call("pane.process_info", json!({ "pane_id": pane_id }))?)
}

/// `parse_process_argvs` for a live local herdr: each stale session-file argument is replaced by the session file
/// the process has open. Used by the bridge and by "open in herdr", so both find the same pane for a conversation.
pub fn live_process_argvs(result: &Value) -> Result<Vec<Vec<String>>, String> {
    parse_process_argvs_with(result, open_session_file)
}

/// The session each agent pane reported to herdr through its integration, by pane id: the `value` of
/// `agent.list`'s `agent_session` (a session file path for omp and Pi, a session id for Claude and OpenCode).
pub fn agent_sessions(h: &dyn Herdr) -> Result<HashMap<String, String>, String> {
    Ok(parse_agent_sessions(&h.call("agent.list", json!({}))?))
}

/// The pane-to-session map of an `agent_list` result. Agents without a reported session are skipped.
pub fn parse_agent_sessions(result: &Value) -> HashMap<String, String> {
    result["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let pane = a["pane_id"].as_str()?;
            let session = a["agent_session"]["value"].as_str().filter(|s| !s.is_empty())?;
            Some((pane.to_string(), session.to_string()))
        })
        .collect()
}

/// `argvs` with each session-file argument replaced by the session file the pane's agent reported to herdr, when that
/// is another file in the same directory: the move `open_session_file` follows through `/proc` on Linux, found on
/// every platform. The argument keeps its own spelling of the directory (on Windows sesync's resume path mixes `/` and
/// `\`, herdr's does not), so it still equals the path a conversation resumes. Unchanged when nothing was reported, the
/// report is not a file in that directory, or it names the same file.
pub fn with_reported_session(argvs: Vec<Vec<String>>, reported: Option<&str>) -> Vec<Vec<String>> {
    let Some(reported) = reported.filter(|r| r.ends_with(".jsonl")) else { return argvs };
    let reported = Path::new(reported);
    let (Some(dir), Some(name)) = (reported.parent(), reported.file_name()) else { return argvs };
    argvs
        .into_iter()
        .map(|argv| {
            argv.into_iter()
                .map(|arg| {
                    let current = Path::new(&arg);
                    let moved = arg.ends_with(".jsonl")
                        && current.parent().is_some_and(|d| same_path(d, dir))
                        && current.file_name().is_some_and(|n| !same_path(Path::new(n), Path::new(name)));
                    if moved { current.with_file_name(name).to_string_lossy().into_owned() } else { arg }
                })
                .collect()
        })
        .collect()
}

/// Whether a session the agent reported to herdr (`agent_sessions`) is the conversation identifier `ident`: the same
/// file, compared as `same_path` does. Places a pane whose command line names no session, as `omp --resume` started from
/// omp's picker.
pub fn same_session(reported: &str, ident: &str) -> bool {
    !ident.is_empty() && same_path(Path::new(reported), Path::new(ident))
}

/// Two paths name the same file: equal as written, or, on Windows, equal once `/` and `\` are unified and case is ignored.
fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    cfg!(windows) && {
        let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
        norm(a) == norm(b)
    }
}

/// The pane's visible screen with its colour codes, every row as herdr rendered it.
pub fn read_visible(h: &dyn Herdr, pane_id: &str) -> Result<String, String> {
    let result = h.call(
        "pane.read",
        json!({ "pane_id": pane_id, "source": "visible", "format": "ansi", "strip_ansi": false }),
    )?;
    parse_read(&result)
}

/// The pane's last `lines` rows of text, colour codes removed.
pub fn read_recent_text(h: &dyn Herdr, pane_id: &str, lines: u32) -> Result<String, String> {
    let result = h.call(
        "pane.read",
        json!({ "pane_id": pane_id, "source": "recent", "lines": lines, "format": "text" }),
    )?;
    parse_read(&result)
}

/// The pane's last `lines` rows with their colour codes: its history above the screen, then the screen (herdr keeps
/// 1000 rows of a pane).
pub fn read_recent_ansi(h: &dyn Herdr, pane_id: &str, lines: u32) -> Result<String, String> {
    let result = h.call(
        "pane.read",
        json!({ "pane_id": pane_id, "source": "recent", "lines": lines, "format": "ansi", "strip_ansi": false }),
    )?;
    parse_read(&result)
}

/// Type `text` into the pane without pressing Enter.
pub fn send_text(h: &dyn Herdr, pane_id: &str, text: &str) -> Result<(), String> {
    h.call("pane.send_text", json!({ "pane_id": pane_id, "text": text })).map(|_| ())
}

/// Press the named keys in order (`enter`, `ctrl+c`, `esc`, …).
pub fn send_keys(h: &dyn Herdr, pane_id: &str, keys: &[&str]) -> Result<(), String> {
    h.call("pane.send_keys", json!({ "pane_id": pane_id, "keys": keys })).map(|_| ())
}

/// Submit `text` to the agent in `target` (a pane id or agent name): the text, then Enter, as one
/// ordered submission. Acknowledges the writes, not the start of a turn.
pub fn agent_prompt(h: &dyn Herdr, target: &str, text: &str) -> Result<(), String> {
    h.call("agent.prompt", json!({ "target": target, "text": text })).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_yields_its_result_or_herdrs_message() {
        let ok = reply("pane.list", r#"{"id":"sesync","result":{"type":"pane_list","panes":[]}}"#).unwrap();
        assert_eq!(ok["type"], "pane_list");
        let err = reply("pane.get", r#"{"id":"sesync","error":{"code":"pane_not_found","message":"no such pane"}}"#);
        assert_eq!(err, Err("no such pane".to_string()));
        assert!(reply("pane.list", "").unwrap_err().contains("closed the connection"));
        assert!(reply("pane.list", "not json").unwrap_err().contains("something other than JSON"));
        assert!(reply("pane.list", r#"{"id":"x"}"#).unwrap_err().contains("has no result"));
    }

    #[test]
    fn pane_list_maps_the_fields_the_bridge_uses_and_skips_incomplete_entries() {
        let result = json!({
            "type": "pane_list",
            "panes": [
                {
                    "pane_id": "w1:p1", "terminal_id": "term_a", "tab_id": "w1:t1", "focused": true,
                    "agent_status": "working", "agent": "claude", "scroll": {"viewport_rows": 40}, "revision": 3
                },
                { "pane_id": "w1:p2", "terminal_id": "term_b", "tab_id": "w1:t2", "agent_status": "idle", "agent": "" },
                { "pane_id": "w1:p3" }
            ]
        });
        let panes = parse_panes(&result).unwrap();
        assert_eq!(panes.len(), 2, "the entry without a terminal id is skipped");
        assert_eq!(panes[0].terminal_id, "term_a");
        assert!(panes[0].focused);
        assert_eq!(panes[0].agent_status, "working");
        assert_eq!(panes[0].agent.as_deref(), Some("claude"));
        assert_eq!(panes[0].rows, Some(40));
        assert_eq!(panes[1].agent, None, "an empty agent name is no agent");
        assert_eq!(panes[1].agent_status, "idle");
        assert_eq!(panes[1].rows, None);
        assert!(parse_panes(&json!({})).is_err());
    }

    #[test]
    fn process_argvs_keep_only_non_empty_lists() {
        let result = json!({ "process_info": { "foreground_processes": [
            { "pid": 1, "name": "claude", "argv": ["claude", "--resume", "abc"] },
            { "pid": 2, "name": "x", "argv": [] },
            { "pid": 3, "name": "y" }
        ]}});
        let none = |_: u64, _: &[String]| None;
        assert_eq!(parse_process_argvs_with(&result, none).unwrap(), vec![vec!["claude", "--resume", "abc"]]);
        assert!(parse_process_argvs_with(&json!({"process_info": {}}), none).is_err());
    }

    #[test]
    fn agent_sessions_map_each_pane_to_the_session_its_agent_reported() {
        let result = json!({ "type": "agent_list", "agents": [
            { "pane_id": "w6:p3", "agent": "omp", "agent_session": { "source": "herdr:omp", "kind": "path", "value": "/s/-p/new.jsonl" } },
            { "pane_id": "w1:pD", "agent": "claude", "agent_session": { "kind": "id", "value": "uuid-1" } },
            { "pane_id": "w5:p7", "agent": "omp" },
            { "pane_id": "w3:p2", "agent": "omp", "agent_session": { "kind": "path", "value": "" } }
        ]});
        let sessions = parse_agent_sessions(&result);
        assert_eq!(sessions.len(), 2, "panes without a reported session are skipped: {sessions:?}");
        assert_eq!(sessions["w6:p3"], "/s/-p/new.jsonl");
        assert_eq!(sessions["w1:pD"], "uuid-1");
        assert!(parse_agent_sessions(&json!({})).is_empty());
    }

    #[test]
    fn a_session_file_the_agent_moved_to_replaces_the_stale_argument_in_its_spelling() {
        let argvs = || vec![vec!["omp".to_string(), "--resume".to_string(), "/s/-p/old.jsonl".to_string()]];
        assert_eq!(
            with_reported_session(argvs(), Some("/s/-p/new.jsonl")),
            vec![vec!["omp", "--resume", "/s/-p/new.jsonl"]],
            "the move omp makes: same directory, new file"
        );
        assert_eq!(with_reported_session(argvs(), Some("/s/-p/old.jsonl")), argvs(), "the same file stays");
        assert_eq!(with_reported_session(argvs(), Some("/s/-other/new.jsonl")), argvs(), "another directory is not a move");
        assert_eq!(with_reported_session(argvs(), Some("uuid-1")), argvs(), "an id is not a session file");
        assert_eq!(with_reported_session(argvs(), None), argvs());
        let claude = vec![vec!["claude".to_string(), "--resume".to_string(), "abc".to_string()]];
        assert_eq!(with_reported_session(claude.clone(), Some("/s/-p/new.jsonl")), claude, "only session-file arguments change");
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_moved_file_keeps_the_arguments_mixed_separators() {
        let argvs = vec![vec!["omp.exe".to_string(), "--resume".to_string(), r"C:\Users\u\.omp/agent\sessions\-P\old.jsonl".to_string()]];
        assert_eq!(
            with_reported_session(argvs.clone(), Some(r"c:\users\u\.omp\agent\sessions\-P\new.jsonl")),
            vec![vec!["omp.exe", "--resume", r"C:\Users\u\.omp/agent\sessions\-P\new.jsonl"]]
        );
        assert_eq!(with_reported_session(argvs.clone(), Some(r"C:\Users\u\.omp\agent\sessions\-P\OLD.jsonl")), argvs);
    }

    #[test]
    fn a_reported_session_is_the_identifier_naming_the_same_file() {
        assert!(same_session("/s/-p/a.jsonl", "/s/-p/a.jsonl"));
        assert!(same_session("uuid-1", "uuid-1"), "an id reported by Claude or OpenCode");
        assert!(!same_session("/s/-p/a.jsonl", "/s/-p/b.jsonl"));
        assert!(!same_session("", ""), "nothing matches an empty identifier");
        assert_eq!(same_session("/S/-p/a.jsonl", "/s/-p/a.jsonl"), cfg!(windows), "case counts off Windows");
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_herdrs_backslashed_report_is_the_mixed_resume_path() {
        // herdr's report and sesync's resume path for OTmain on the work PC, 2026-10-30.
        let reported = r"C:\Users\u\.omp\agent\sessions\-Projetos-admin\2026-10-27T16-57-49-910Z_01a17e4b.jsonl";
        assert!(same_session(reported, r"C:\Users\u\.omp/agent\sessions\-Projetos-admin\2026-10-27T16-57-49-910Z_01a17e4b.jsonl"));
        assert!(same_session(reported, r"c:/users/u/.omp/agent/sessions/-projetos-admin/2026-10-27t16-57-49-910z_01a17e4b.jsonl"));
    }

    #[test]
    fn a_stale_resume_file_is_replaced_by_the_session_file_the_process_has_open() {
        let result = json!({ "process_info": { "foreground_processes": [
            { "pid": 7, "name": "omp", "argv": ["omp", "--resume", "/s/-p/old.jsonl"] },
            { "pid": 8, "name": "claude", "argv": ["claude", "--resume", "abc"] }
        ]}});
        let open = |pid: u64, _: &[String]| (pid == 7).then(|| "/s/-p/new.jsonl".to_string());
        assert_eq!(
            parse_process_argvs_with(&result, open).unwrap(),
            vec![vec!["omp", "--resume", "/s/-p/new.jsonl"], vec!["claude", "--resume", "abc"]]
        );
        // Nothing open (or another machine's pid): herdr's argv stands.
        assert_eq!(parse_process_argvs_with(&result, |_, _| None).unwrap()[0], vec!["omp", "--resume", "/s/-p/old.jsonl"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_whose_command_line_differs_from_herdrs_is_not_read() {
        let me = std::process::id() as u64;
        assert_eq!(open_session_file(me, &["not".into(), "this".into(), "x.jsonl".into()]), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_session_another_local_process_runs_is_reported_until_that_process_ends() {
        let ident = format!("/nowhere/sesync-guard-{}.jsonl", std::process::id());
        assert_eq!(agent_running(&ident), None);
        // A stand-in agent: its argv carries the session as a flag's value, as `omp --resume <file>` does.
        let mut agent = std::process::Command::new("sh")
            // Two commands, so `sh` does not exec into `sleep` and drop its arguments.
            .args(["-c", "sleep 30; :", "sh", "--resume", &ident])
            .spawn()
            .unwrap();
        let pid = agent.id() as u64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while agent_running(&ident).is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(agent_running(&ident), Some(pid));
        agent.kill().unwrap();
        agent.wait().unwrap();
        assert_eq!(agent_running(&ident), None);
    }

    #[test]
    fn a_process_runs_the_copy_its_session_was_moved_to_after_it_started() {
        let dir = std::env::temp_dir().join(format!("sesync-moved-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, id: &str, parent: Option<&str>, ts: &str| {
            let parent = parent.map_or(String::new(), |p| format!(",\"parentSession\":\"{p}\""));
            std::fs::write(
                dir.join(name),
                format!("{{\"type\":\"title\",\"title\":\"Main agent\"}}\n{{\"type\":\"session\",\"id\":\"{id}\",\"timestamp\":\"{ts}\"{parent}}}\n"),
            )
            .unwrap();
        };
        // The 2026-10-29 chain: root, moved at 10:52:33 by an omp started 10:51:51, moved again at 12:47:15 by an omp
        // started 12:46:28 with the first copy in its argv.
        write("root.jsonl", "root", None, "2026-07-17T21:39:39.522Z");
        write("one.jsonl", "one", Some("root"), "2026-10-29T10:52:33.075Z");
        write("two.jsonl", "two", Some("one"), "2026-10-29T12:47:15.385Z");
        let at = |s: &str| crate::prompts::parse_rfc3339(s).unwrap();
        let path = |n: &str| dir.join(n).to_str().unwrap().to_string();
        assert_eq!(moved_copy(&dir.join("root.jsonl"), at("2026-10-29T10:51:51Z")), Some(path("one.jsonl")));
        assert_eq!(moved_copy(&dir.join("one.jsonl"), at("2026-10-29T12:46:28Z")), Some(path("two.jsonl")));
        // A process started after the last move runs its argv's file.
        assert_eq!(moved_copy(&dir.join("two.jsonl"), at("2026-10-29T13:00:00Z")), None);
        // A copy made before the process started is someone else's: the process still runs its argv's file.
        assert_eq!(moved_copy(&dir.join("root.jsonl"), at("2026-10-29T12:00:00Z")), None);
        // The file in the argv was deleted after the move: its id comes from omp's `<created>_<id>.jsonl` name.
        write("2026-10-29T12-47-15-385Z_kid.jsonl", "kid", Some("gone"), "2026-10-29T12:47:15.385Z");
        assert_eq!(
            moved_copy(&dir.join("2026-10-29T10-52-33-075Z_gone.jsonl"), at("2026-10-29T12:46:28Z")),
            Some(path("2026-10-29T12-47-15-385Z_kid.jsonl"))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_read_result_yields_its_text() {
        assert_eq!(parse_read(&json!({"read": {"text": "hello\r\n"}})).unwrap(), "hello\r\n");
        assert!(parse_read(&json!({"read": {}})).is_err());
    }

    #[test]
    fn a_layout_gives_the_size_of_the_named_pane_only() {
        let layout = json!({"type": "pane_layout", "layout": {
            "focused_pane_id": "w1:p2",
            "panes": [
                {"pane_id": "w1:p1", "focused": false, "rect": {"x": 0, "y": 0, "width": 30, "height": 10}},
                {"pane_id": "w1:p2", "focused": true, "rect": {"x": 30, "y": 0, "width": 57, "height": 27}}
            ],
            "splits": []
        }});
        assert_eq!(parse_pane_size(&layout, "w1:p2"), Some((57, 27)));
        assert_eq!(parse_pane_size(&layout, "w9:p9"), None, "a pane outside the layout has no size");
        assert_eq!(parse_pane_size(&json!({"layout": {"panes": [{"pane_id": "w1:p1", "rect": {"width": 0, "height": 5}}]}}), "w1:p1"), None);
        assert_eq!(parse_pane_size(&json!({}), "w1:p1"), None);
        let huge = json!({"layout": {"panes": [{"pane_id": "w1:p1", "rect": {"width": 5000, "height": 900}}]}});
        assert_eq!(parse_pane_size(&huge, "w1:p1"), Some((1000, 500)), "clamped to a terminal's range");
    }

    #[test]
    fn a_pane_without_a_layout_has_no_size() {
        assert_eq!(pane_size(&crate::bridge::testkit::NoHerdr, "w1:p1"), None);
    }

    /// A fake herdr for one subscription. It answers `ack`, writes each chunk after a short pause, and
    /// holds the connection until the returned sender is dropped.
    #[cfg(unix)]
    fn fake_events_server(name: &str, ack: &'static str, chunks: Vec<&'static str>) -> (PathBuf, std::sync::mpsc::Sender<()>) {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("sesync-herdr-events-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("herdr.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let (release, held) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(conn.try_clone().unwrap()).read_line(&mut request).unwrap();
            let request: Value = serde_json::from_str(request.trim()).unwrap();
            assert_eq!(request["method"], "events.subscribe");
            conn.write_all(ack.as_bytes()).unwrap();
            for chunk in chunks {
                std::thread::sleep(Duration::from_millis(100));
                conn.write_all(chunk.as_bytes()).unwrap();
            }
            let _ = held.recv();
        });
        (path, release)
    }

    #[cfg(unix)]
    #[test]
    fn status_events_follow_each_status_change_and_skip_other_events() {
        let (path, release) = fake_events_server(
            "changes",
            "{\"id\":\"sesync-events\",\"result\":{\"type\":\"subscription_started\"}}\n",
            vec![
                "{\"event\":\"pane.output_matched\",\"data\":{}}\n{\"event\":\"pane.agent_status_changed\",\"data\":{\"pane_id\":\"w1:p1\",\"agent_status\":\"working\"}}\n{\"event\":\"pane.agent_status_changed\",\"data\":{\"pane_id\":\"w1:p1\",\"agent_status\":\"idle\"",
                "}}\n",
            ],
        );
        let mut events = StatusEvents::open(&path, &["w1:p1".to_string()]).unwrap();
        let wait = Duration::from_secs(2);
        assert_eq!(events.next(wait).unwrap(), Some(("w1:p1".to_string(), "working".to_string())));
        assert_eq!(
            events.next(wait).unwrap(),
            Some(("w1:p1".to_string(), "idle".to_string())),
            "a line split across writes is joined"
        );
        assert_eq!(events.next(Duration::from_millis(100)).unwrap(), None, "no change within the wait");
        drop(release);
        assert!(events.next(wait).is_err(), "a closed stream is an error the watcher reopens on");
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_subscription_says_why() {
        let (path, release) = fake_events_server(
            "refused",
            "{\"id\":\"sesync-events\",\"error\":{\"code\":\"pane_not_found\",\"message\":\"no such pane\"}}\n",
            Vec::new(),
        );
        match StatusEvents::open(&path, &["w9:p9".to_string()]) {
            Ok(_) => panic!("a refused subscription must not open"),
            Err(e) => assert!(e.contains("no such pane"), "{e}"),
        }
        drop(release);
    }
}
