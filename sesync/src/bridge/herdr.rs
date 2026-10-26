//! The herdr socket client. herdr serves one JSON request per line on a Unix socket and answers each
//! with one line: `{"id","result":{"type":…}}` or `{"id","error":{"code","message"}}`. Every call
//! here opens its own connection, a few milliseconds locally, so a hung call stalls only itself.
//!
//! The bridge codes against the [`Herdr`] trait, so tests answer with canned JSON and the real
//! socket is used only in production and in the sandboxed integration test. Pane and read
//! parsing lives here too, as pure functions over the `result` object.

use serde_json::{json, Value};
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
}

#[cfg(not(unix))]
impl Herdr for Socket {
    fn call(&self, _method: &str, _params: Value) -> Result<Value, String> {
        Err("herdr's socket API needs a Unix socket; this platform has none".into())
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

pub fn pane_info(h: &dyn Herdr, pane_id: &str) -> Result<Pane, String> {
    let result = h.call("pane.get", json!({ "pane_id": pane_id }))?;
    parse_pane(&result["pane"]).ok_or_else(|| format!("herdr's info for pane {pane_id} is incomplete"))
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
}
