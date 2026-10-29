//! The herdr socket client. herdr serves one JSON request per line on a Unix socket and answers each
//! with one line: `{"id","result":{"type":…}}` or `{"id","error":{"code","message"}}`. Every call
//! here opens its own connection, a few milliseconds locally, so a hung call stalls only itself.
//!
//! The bridge codes against the [`Herdr`] trait, so tests answer with canned JSON and the real
//! socket is used only in production and in the sandboxed integration test. Pane and read
//! parsing lives here too, as pure functions over the `result` object.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long one herdr call may run. herdr answers in milliseconds; a call that hangs must not hold a
/// phone's request open.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The socket herdr listens on when nothing overrides it: `$HERDR_SOCKET_PATH` (herdr sets it for its
/// own panes), else `$HOME/.config/herdr/herdr.sock`.
pub fn default_socket() -> PathBuf {
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
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

/// herdr over its Unix socket.
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

#[cfg(unix)]
impl Herdr for Socket {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(&self.path)
            .map_err(|e| format!("herdr is not reachable at {} ({e})", self.path.display()))?;
        stream.set_read_timeout(Some(CALL_TIMEOUT)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(CALL_TIMEOUT)).map_err(|e| e.to_string())?;
        let request = json!({ "id": "sesync", "method": method, "params": params });
        writeln!(stream, "{request}").map_err(|e| format!("herdr did not take {method} ({e})"))?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)
            .map_err(|e| format!("herdr did not answer {method} ({e})"))?;
        reply(method, &line)
    }

    fn status_events(&self, pane_ids: &[String]) -> Result<Box<dyn StatusSource>, String> {
        StatusEvents::open(&self.path, pane_ids).map(|events| Box::new(events) as Box<dyn StatusSource>)
    }
}

#[cfg(not(unix))]
impl Herdr for Socket {
    fn call(&self, _method: &str, _params: Value) -> Result<Value, String> {
        Err("herdr's socket API needs a Unix socket; this platform has none".into())
    }
}
#[cfg(unix)]
impl StatusSource for StatusEvents {
    fn next(&mut self, wait: Duration) -> Result<Option<(String, String)>, String> {
        StatusEvents::next(self, wait)
    }
}

/// A subscription to `pane.agent_status_changed` for a set of panes. herdr pushes one event line per
/// change, in order, so no status between two reads is lost. Sampling cannot promise that: an agent
/// turn can start and finish between two samples.
#[cfg(unix)]
pub struct StatusEvents {
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    pending: String,
}

#[cfg(unix)]
impl StatusEvents {
    /// Subscribe to the status of each pane in `pane_ids` (herdr pane ids such as `w1:p1`).
    pub fn open(path: &Path, pane_ids: &[String]) -> Result<StatusEvents, String> {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(path)
            .map_err(|e| format!("herdr is not reachable at {} ({e})", path.display()))?;
        stream.set_write_timeout(Some(CALL_TIMEOUT)).map_err(|e| e.to_string())?;
        let subscriptions: Vec<Value> = pane_ids
            .iter()
            .map(|id| json!({ "type": "pane.agent_status_changed", "pane_id": id }))
            .collect();
        let request = json!({ "id": "sesync-events", "method": "events.subscribe", "params": { "subscriptions": subscriptions } });
        writeln!(stream, "{request}").map_err(|e| format!("herdr did not take the event subscription ({e})"))?;
        let mut events = StatusEvents { reader: std::io::BufReader::new(stream), pending: String::new() };
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

    /// One whole line from the socket, or None when `wait` passes first. A partial line waits in `pending`.
    fn line(&mut self, wait: Duration) -> Result<Option<String>, String> {
        use std::io::BufRead;
        self.reader.get_ref().set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
        loop {
            if let Some(end) = self.pending.find('\n') {
                let rest = self.pending.split_off(end + 1);
                let line = std::mem::replace(&mut self.pending, rest);
                return Ok(Some(line));
            }
            let mut chunk = String::new();
            match self.reader.read_line(&mut chunk) {
                Ok(0) => return Err("herdr closed the event subscription".into()),
                Ok(_) => self.pending.push_str(&chunk),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    // read_line keeps what it read before the timeout in `chunk`; keep it for the next line.
                    self.pending.push_str(&chunk);
                    return Ok(None);
                }
                Err(e) => return Err(format!("herdr's event stream failed ({e})")),
            }
        }
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
/// lists are dropped.
pub fn parse_process_argvs(result: &Value) -> Result<Vec<Vec<String>>, String> {
    let procs = result["process_info"]["foreground_processes"]
        .as_array()
        .ok_or("herdr's process info has no foreground processes")?;
    Ok(procs
        .iter()
        .filter_map(|p| p["argv"].as_array())
        .map(|argv| argv.iter().filter_map(|a| a.as_str().map(str::to_string)).collect::<Vec<String>>())
        .filter(|argv| !argv.is_empty())
        .collect())
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
    parse_process_argvs(&h.call("pane.process_info", json!({ "pane_id": pane_id }))?)
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
        assert_eq!(parse_process_argvs(&result).unwrap(), vec![vec!["claude", "--resume", "abc"]]);
        assert!(parse_process_argvs(&json!({"process_info": {}})).is_err());
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
