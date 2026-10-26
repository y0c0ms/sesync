//! The RPC subset the phone uses. A sesync conversation is a worktree, its herdr panes are its terminals
//! and tabs, and the host is this machine (see docs/BRIDGE.md, "Mapping").
//!   * `status.get`: what this host is, for the app's compatibility gate;
//!   * `worktree.ps`: the home list, one row per conversation, newest first;
//!   * `repo.list`: the conversations' project directories, for grouping;
//!   * `session.tabs.list` / `session.tabs.subscribe` / `session.tabs.unsubscribe`: the panes running a
//!     conversation;
//!   * `terminal.list`, `terminal.read`, `terminal.send`, `terminal.subscribe`, `terminal.unsubscribe`;
//!   * `agentSession.subscribeStatus`: the agent statuses of those panes.
//! Every other method answers `method_not_found`. The app reads that as "not available here" and skips
//! the read, so the rest of its screens degrade rather than fail.

use super::catalog::{self, Catalog, Conversation, Project};
use super::herdr::{self, Herdr};
use super::stream::{self, Conn};
use super::terminals::{self, Running};
use super::turns::{LockTools, Turns};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

pub const RUNTIME_ID: &str = "sesync-bridge";
/// Orca's `MOBILE_PROTOCOL_VERSION` (mobile/src/transport/protocol-version.ts). The phone refuses a
/// host whose `minCompatibleMobileVersion` is above that, so this is the version whose subset is
/// served here.
pub const PROTOCOL_VERSION: u64 = 3;
/// The oldest phone protocol this host supports. The app enforces its own floor of 2 on hosts.
pub const MIN_COMPATIBLE_MOBILE_VERSION: u64 = 2;
/// The rows a `terminal.read` returns.
const READ_LINES: u32 = 200;

/// This host's identity, as `status.get` reports it.
#[derive(Debug, Clone)]
pub struct Host {
    pub machine_name: String,
    /// Orca's platform names: `linux`, `darwin`, `win32`.
    pub platform: &'static str,
}

/// Everything a request may use. Cheap to clone: every part is shared.
#[derive(Clone)]
pub struct Ctx {
    pub catalog: Arc<dyn Catalog>,
    pub herdr: Arc<dyn Herdr>,
    pub host: Host,
    pub locks: LockTools,
    pub turns: Turns,
}

/// A failed request: the error code the phone sees, and its message.
type Failure = (&'static str, String);

/// Answer one decrypted request frame. The reply carries the request's `id`, as the app matches on it.
/// `None` means a stream took the request over: its first frame was already sent on `conn`.
pub fn dispatch(request: &Value, ctx: &Ctx, conn: &Conn) -> Option<Value> {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let outcome = match method {
        "session.tabs.subscribe" => return stream_or_error(&id, tabs_subscribe(&params, ctx, conn, &id)),
        "terminal.subscribe" => return stream_or_error(&id, terminal_subscribe(&params, ctx, conn, &id)),
        "agentSession.subscribeStatus" => return stream_or_error(&id, status_subscribe(ctx, conn, &id)),
        "status.get" => Ok(status(&ctx.host)),
        "worktree.ps" => worktree_ps(ctx),
        "repo.list" => repo_list(ctx),
        "session.tabs.list" => tabs_list(&params, ctx),
        "session.tabs.unsubscribe" | "terminal.unsubscribe" => Ok(unsubscribe(&params, conn)),
        "terminal.list" => terminal_list(&params, ctx),
        "terminal.read" => terminal_read(&params, ctx),
        "terminal.send" => terminal_send(&params, ctx),
        other => return Some(error(&id, "method_not_found", &format!("Unknown method: {other}"))),
    };
    Some(match outcome {
        Ok(result) => success(&id, result),
        Err((code, message)) => error(&id, code, &message),
    })
}

fn stream_or_error(id: &Value, started: Result<(), Failure>) -> Option<Value> {
    match started {
        Ok(()) => None,
        Err((code, message)) => Some(error(id, code, &message)),
    }
}

pub fn success(id: &Value, result: Value) -> Value {
    json!({ "id": id, "ok": true, "result": result, "_meta": { "runtimeId": RUNTIME_ID } })
}

pub fn error(id: &Value, code: &str, message: &str) -> Value {
    json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message },
        "_meta": { "runtimeId": RUNTIME_ID },
    })
}

fn status(host: &Host) -> Value {
    json!({
        "runtimeId": RUNTIME_ID,
        "appVersion": env!("CARGO_PKG_VERSION"),
        "protocolVersion": PROTOCOL_VERSION,
        "minCompatibleMobileVersion": MIN_COMPATIBLE_MOBILE_VERSION,
        "capabilities": [],
        "floatingWorkspaceEnabled": false,
        "hostPlatform": host.platform,
        "machineName": host.machine_name,
    })
}

/// The worktree id a selector names: the app sends `id:<worktree id>`.
fn worktree_param(params: &Value) -> Result<String, Failure> {
    let selector = params["worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(("bad_request", "missing worktree selector".to_string()))?;
    Ok(terminals::worktree_from_selector(selector).to_string())
}

fn handle_param(params: &Value) -> Result<String, Failure> {
    params["terminal"]
        .as_str()
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .ok_or(("bad_request", "missing terminal handle".to_string()))
}

/// The conversation a worktree id names, and the herdr panes now. Fails when the conversation is not
/// listed, or herdr cannot be read: an empty tab list would then claim nothing is open.
fn resolve(ctx: &Ctx, worktree: &str) -> Result<(Conversation, Vec<Running>), Failure> {
    let conversation = ctx
        .catalog
        .conversations()
        .map_err(|m| ("unavailable", m))?
        .into_iter()
        .find(|c| c.worktree_id() == worktree)
        .ok_or_else(|| ("worktree_not_found", format!("no conversation {worktree}")))?;
    let panes = terminals::running_panes(ctx.herdr.as_ref()).map_err(|m| ("herdr_unavailable", m))?;
    Ok((conversation, panes))
}

/// The pane a terminal handle names, from herdr's list now.
fn find_terminal(ctx: &Ctx, handle: &str) -> Result<herdr::Pane, Failure> {
    herdr::list_panes(ctx.herdr.as_ref())
        .map_err(|m| ("herdr_unavailable", m))?
        .into_iter()
        .find(|p| p.terminal_id == handle)
        .ok_or_else(|| ("terminal_not_found", format!("no terminal {handle}")))
}

fn worktree_ps(ctx: &Ctx) -> Result<Value, Failure> {
    let conversations = ctx.catalog.conversations().map_err(|m| ("unavailable", m))?;
    // Live state is best-effort: without herdr every conversation shows as not open, and the list still loads.
    let panes = terminals::running_panes(ctx.herdr.as_ref()).unwrap_or_default();
    let now = terminals::now_ms();
    let rows: Vec<Value> = conversations
        .iter()
        .map(|c| worktree_row(c, &terminals::panes_of(&panes, c), now))
        .collect();
    Ok(json!({ "worktrees": rows, "totalCount": rows.len(), "truncated": false }))
}

/// One `RuntimeWorktreePsSummary`: a conversation, its repo, its status, and one agent row per pane.
fn worktree_row(c: &Conversation, panes: &[&Running], now: f64) -> Value {
    let repo = c.repo_id();
    let name = catalog::project_name(&c.dir);
    let updated_ms = (c.updated * 1000.0).round();
    let live = panes.len();
    let preview = if live == 0 {
        format!("{} session", c.harness)
    } else {
        format!("{} session · {live} open in herdr", c.harness)
    };
    let agents: Vec<Value> = panes.iter().map(|r| terminals::agent_row(r, c, now)).collect();
    json!({
        "worktreeId": c.worktree_id(),
        "repoId": repo,
        "repo": name,
        "path": c.dir,
        "branch": "",
        "workspaceKind": "folder-workspace",
        "isArchived": false,
        "isMainWorktree": false,
        "hasHostSidebarActivity": false,
        "parentWorktreeId": null,
        "childWorktreeIds": [],
        "displayName": c.title,
        "workspaceStatus": "in-progress",
        "sortOrder": updated_ms,
        "lastActivityAt": updated_ms,
        "linkedIssue": null,
        "linkedPR": null,
        "linkedLinearIssue": null,
        "linkedGitLabMR": null,
        "linkedGitLabIssue": null,
        "comment": "",
        "isPinned": false,
        "isActive": live > 0,
        "unread": false,
        "liveTerminalCount": live,
        "hasAttachedPty": live > 0,
        "lastOutputAt": null,
        "preview": preview,
        "status": terminals::worktree_status(panes),
        "agents": agents,
    })
}

fn repo_list(ctx: &Ctx) -> Result<Value, Failure> {
    let projects = ctx.catalog.projects().map_err(|m| ("unavailable", m))?;
    let repos: Vec<Value> = projects.iter().map(repo_row).collect();
    Ok(json!({ "repos": repos }))
}

fn repo_row(p: &Project) -> Value {
    json!({
        "id": catalog::repo_id(&p.dir),
        "displayName": catalog::project_name(&p.dir),
        "path": p.dir,
        "badgeColor": "#64748b",
        "connectionId": null,
    })
}

fn tabs_list(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let worktree = worktree_param(params)?;
    let (conv, panes) = resolve(ctx, &worktree)?;
    let mine = terminals::panes_of(&panes, &conv);
    Ok(terminals::tabs_result(&worktree, &conv, &mine, terminals::publication_epoch(), terminals::next_version()))
}

fn tabs_subscribe(params: &Value, ctx: &Ctx, conn: &Conn, id: &Value) -> Result<(), Failure> {
    let worktree = worktree_param(params)?;
    let (conv, panes) = resolve(ctx, &worktree)?;
    let snapshot = terminals::tabs_result(
        &worktree,
        &conv,
        &terminals::panes_of(&panes, &conv),
        terminals::publication_epoch(),
        terminals::next_version(),
    );
    let mut first = snapshot.clone();
    first["type"] = json!("snapshot");
    let stop = conn.register(format!("tabs:{worktree}"), id.clone());
    let _ = conn.sink.json(&stream::streamed(id, first));
    let tabs = stream::Tabs {
        herdr: Arc::clone(&ctx.herdr),
        catalog: Arc::clone(&ctx.catalog),
        sink: conn.sink.clone(),
        request_id: id.clone(),
        worktree,
        stop,
        last: snapshot,
    };
    thread::spawn(move || stream::run_tabs(tabs));
    Ok(())
}

fn terminal_list(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let handles: Option<Vec<String>> = params["handles"]
        .as_array()
        .map(|list| list.iter().filter_map(|h| h.as_str().map(str::to_string)).collect());
    let rows: Vec<Value> = match params["worktree"].as_str().filter(|s| !s.is_empty()) {
        Some(selector) => {
            let (conv, panes) = resolve(ctx, terminals::worktree_from_selector(selector))?;
            terminals::panes_of(&panes, &conv).into_iter().map(|r| terminals::terminal_row(r, &conv)).collect()
        }
        None => {
            let conversations = ctx.catalog.conversations().map_err(|m| ("unavailable", m))?;
            let panes = terminals::running_panes(ctx.herdr.as_ref()).map_err(|m| ("herdr_unavailable", m))?;
            panes
                .iter()
                .filter_map(|r| terminals::conversation_of(r, &conversations).map(|c| terminals::terminal_row(r, c)))
                .collect()
        }
    };
    let rows: Vec<Value> = rows
        .into_iter()
        .filter(|row| {
            handles.as_ref().map_or(true, |list| list.iter().any(|h| row["handle"].as_str() == Some(h.as_str())))
        })
        .collect();
    Ok(json!({ "terminals": rows, "totalCount": rows.len(), "truncated": false }))
}

fn terminal_read(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let handle = handle_param(params)?;
    let pane = find_terminal(ctx, &handle)?;
    let text = herdr::read_recent_text(ctx.herdr.as_ref(), &pane.pane_id, READ_LINES)
        .map_err(|m| ("herdr_unavailable", m))?;
    let tail: Vec<&str> = text.lines().collect();
    Ok(json!({ "terminal": {
        "handle": handle,
        "status": "running",
        "tail": tail,
        "truncated": false,
        "nextCursor": null,
        "source": "screen",
    }}))
}

fn terminal_send(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let handle = handle_param(params)?;
    let pane = find_terminal(ctx, &handle)?;
    let h = ctx.herdr.as_ref();
    let text = params["text"].as_str().unwrap_or("");
    let enter = params["enter"].as_bool() == Some(true);
    let interrupt = params["interrupt"].as_bool() == Some(true);
    let send = |r: Result<(), String>| r.map_err(|m| ("send_failed", m));
    let mut written = 0;
    if interrupt {
        send(herdr::send_keys(h, &pane.pane_id, &["ctrl+c"]))?;
    } else if enter && !text.is_empty() {
        submit(ctx, &pane, text)?;
        written = text.len();
    } else if !text.is_empty() {
        send(herdr::send_text(h, &pane.pane_id, text))?;
        written = text.len();
    } else if enter {
        send(herdr::send_keys(h, &pane.pane_id, &["enter"]))?;
    }
    Ok(json!({ "send": { "handle": handle, "accepted": true, "bytesWritten": written } }))
}

/// A prompt submission. When the pane runs a conversation, the conversation is claimed in sesync-lock
/// first and released when the agent's turn ends. A pane with a detected agent gets the prompt through
/// herdr's agent API, which orders the text before Enter; any other pane gets the text, then Enter.
fn submit(ctx: &Ctx, pane: &herdr::Pane, text: &str) -> Result<(), Failure> {
    let h = ctx.herdr.as_ref();
    let conversations = ctx.catalog.conversations().map_err(|m| ("unavailable", m))?;
    let argvs = herdr::foreground_argvs(h, &pane.pane_id).map_err(|m| ("herdr_unavailable", m))?;
    let running = Running { pane: pane.clone(), argvs };
    let conv = terminals::conversation_of(&running, &conversations).cloned();
    if let Some(c) = &conv {
        ctx.turns.begin(&ctx.locks, &c.lock_id(), &pane.terminal_id).map_err(|m| ("conversation_locked", m))?;
    }
    let sent = if pane.agent.is_some() {
        herdr::agent_prompt(h, &pane.pane_id, text)
    } else {
        herdr::send_text(h, &pane.pane_id, text).and_then(|()| herdr::send_keys(h, &pane.pane_id, &["enter"]))
    };
    match (sent, conv) {
        (Ok(()), Some(c)) => {
            ctx.turns.watch(ctx.locks.clone(), Arc::clone(&ctx.herdr), c.lock_id(), pane.pane_id.clone());
            Ok(())
        }
        (Ok(()), None) => Ok(()),
        (Err(message), conv) => {
            if let Some(c) = conv {
                ctx.turns.end(&ctx.locks, &c.lock_id());
            }
            Err(("send_failed", message))
        }
    }
}

fn terminal_subscribe(params: &Value, ctx: &Ctx, conn: &Conn, id: &Value) -> Result<(), Failure> {
    let handle = handle_param(params)?;
    let pane = find_terminal(ctx, &handle)?;
    let client = params["client"]["id"].as_str().unwrap_or("");
    let key = if client.is_empty() { handle.clone() } else { format!("{handle}:{client}") };
    let binary = params["capabilities"]["terminalBinaryStream"].as_u64() == Some(1);
    let cols = params["viewport"]["cols"].as_u64().unwrap_or(80).clamp(1, 1000) as u32;
    let rows = params["viewport"]["rows"].as_u64().or(pane.rows).unwrap_or(24).clamp(1, 500) as u32;
    let shown = stream::screen_text(
        &herdr::read_visible(ctx.herdr.as_ref(), &pane.pane_id).map_err(|m| ("herdr_unavailable", m))?,
    );
    let stream_id = conn.next_stream_id();
    let stop = conn.register(key, id.clone());
    let head = if binary {
        json!({ "type": "subscribed", "streamId": stream_id, "lines": [], "truncated": false,
                "cols": cols, "rows": rows, "displayMode": "auto", "seq": 0 })
    } else {
        json!({ "type": "scrollback", "lines": [], "truncated": false, "serialized": shown.clone(),
                "cols": cols, "rows": rows, "displayMode": "auto", "seq": 0 })
    };
    let _ = conn.sink.json(&stream::streamed(id, head));
    let term = stream::Terminal {
        herdr: Arc::clone(&ctx.herdr),
        sink: conn.sink.clone(),
        request_id: id.clone(),
        terminal_id: handle,
        pane_id: pane.pane_id,
        stream_id,
        binary,
        cols,
        rows,
        stop,
        shown,
    };
    thread::spawn(move || stream::run_terminal(term));
    Ok(())
}

fn unsubscribe(params: &Value, conn: &Conn) -> Value {
    let key = params["subscriptionId"].as_str().map(str::to_string);
    let mut stopped = conn.stop(key.as_deref(), params.get("requestId"));
    if let Some(selector) = params["worktree"].as_str().filter(|s| !s.is_empty()) {
        let worktree = terminals::worktree_from_selector(selector);
        stopped |= conn.stop(Some(&format!("tabs:{worktree}")), None);
    }
    json!({ "unsubscribed": stopped })
}

fn status_subscribe(ctx: &Ctx, conn: &Conn, id: &Value) -> Result<(), Failure> {
    let conversations = ctx.catalog.conversations().map_err(|m| ("unavailable", m))?;
    let panes = terminals::running_panes(ctx.herdr.as_ref()).map_err(|m| ("herdr_unavailable", m))?;
    let sessions = stream::sessions(&panes, &conversations, terminals::now_ms());
    let last: HashMap<String, Value> = sessions
        .iter()
        .map(|s| (s["sessionId"].as_str().unwrap_or("").to_string(), s.clone()))
        .collect();
    let stop = conn.register("status".into(), id.clone());
    let _ = conn.sink.json(&stream::streamed(id, json!({ "type": "snapshot", "sessions": sessions })));
    let status = stream::Status {
        herdr: Arc::clone(&ctx.herdr),
        catalog: Arc::clone(&ctx.catalog),
        sink: conn.sink.clone(),
        request_id: id.clone(),
        stop,
        last,
    };
    thread::spawn(move || stream::run_status(status));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use parking_lot::Mutex;

    struct Fixed(Vec<Conversation>);

    impl Catalog for Fixed {
        fn conversations(&self) -> Result<Vec<Conversation>, String> {
            Ok(self.0.clone())
        }
    }

    struct Broken;

    impl Catalog for Broken {
        fn conversations(&self) -> Result<Vec<Conversation>, String> {
            Err("cannot read the sesync state".into())
        }
    }

    /// Canned herdr answers, keyed by method. Records each call's method and params.
    struct Canned {
        answers: HashMap<&'static str, Value>,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl Herdr for Canned {
        fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            self.calls.lock().push((method.to_string(), params));
            self.answers
                .get(method)
                .cloned()
                .ok_or_else(|| format!("no canned answer for {method}"))
        }
    }

    fn herdr_with(pane_list: Value, argvs: Value) -> Arc<Canned> {
        let mut answers = HashMap::new();
        answers.insert("pane.list", pane_list);
        answers.insert("pane.process_info", argvs);
        answers.insert("pane.get", json!({"type":"pane_info","pane":{"pane_id":"w1:p1","terminal_id":"term_a","tab_id":"w1:t1","agent_status":"working"}}));
        answers.insert("pane.read", json!({"type":"pane_read","read":{"text":"hello\r\n","pane_id":"w1:p1"}}));
        answers.insert("pane.send_text", json!({"type":"ok"}));
        answers.insert("pane.send_keys", json!({"type":"ok"}));
        answers.insert("agent.prompt", json!({"type":"agent_prompted"}));
        Arc::new(Canned { answers, calls: Mutex::new(Vec::new()) })
    }

    fn ctx(catalog: Arc<dyn Catalog>, herdr: Arc<dyn Herdr>) -> Ctx {
        Ctx {
            catalog,
            herdr,
            host: Host { machine_name: "box".into(), platform: "linux" },
            locks: LockTools { sesync_lock: "/nonexistent/sesync-lock".into(), lock_dir: "/nonexistent".into() },
            turns: Turns::default(),
        }
    }

    fn conv(title: &str, ident: &str, dir: &str, updated: f64) -> Conversation {
        Conversation {
            harness: "omp".into(),
            key: format!("/s/{title}.jsonl"),
            title: title.into(),
            dir: dir.into(),
            updated,
            idents: vec![ident.into()],
        }
    }

    fn req(id: &str, method: &str, params: Value) -> Value {
        json!({ "id": id, "deviceToken": "t", "method": method, "params": params })
    }

    /// The connection a test dispatches on. Its frames go nowhere; the tests read the reply.
    fn conn() -> (Conn, tokio::sync::mpsc::UnboundedReceiver<tokio_tungstenite::tungstenite::Message>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let channel = Arc::new(crate::bridge::e2ee::Channel::new(
            &crypto_box::SecretKey::from_slice(&[1u8; 32]).unwrap().public_key(),
            &crypto_box::SecretKey::from_slice(&[2u8; 32]).unwrap(),
        ));
        (Conn::new(stream::Sink::new(tx, channel)), rx)
    }

    #[test]
    fn unknown_methods_answer_method_not_found_with_the_request_id() {
        let c = ctx(Arc::new(Fixed(vec![])), herdr_with(json!({}), json!({})));
        let (conn, _rx) = conn();
        let reply = dispatch(&req("rpc-7", "terminal.nope", json!({})), &c, &conn).unwrap();
        assert_eq!(reply["id"], "rpc-7");
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], "method_not_found");
        assert_eq!(reply["error"]["message"], "Unknown method: terminal.nope");
        assert_eq!(reply["_meta"]["runtimeId"], RUNTIME_ID);
    }

    #[test]
    fn status_reports_the_protocol_the_app_gates_on() {
        let c = ctx(Arc::new(Fixed(vec![])), herdr_with(json!({}), json!({})));
        let (conn, _rx) = conn();
        let result = dispatch(&req("s", "status.get", json!({})), &c, &conn).unwrap()["result"].clone();
        assert_eq!(result["protocolVersion"], 3);
        assert_eq!(result["minCompatibleMobileVersion"], 2);
        assert_eq!(result["hostPlatform"], "linux");
        assert_eq!(result["capabilities"], json!([]));
        assert!(result["appVersion"].is_string());
    }

    #[test]
    fn worktree_rows_are_conversations_with_their_live_panes_and_agents() {
        let a = conv("fix the login", "uuid-1", "/home/u/work/alpha", 200.0);
        let b = conv("older", "uuid-2", "/home/u/work/alpha", 100.0);
        let panes = json!({"type":"pane_list","panes":[{
            "pane_id":"w1:p1","terminal_id":"term_a","tab_id":"w1:t1","focused":true,"agent_status":"working","agent":"omp"
        }]});
        let argvs = json!({"process_info":{"foreground_processes":[{"argv":["omp","--resume","uuid-1"]}]}});
        let c = ctx(Arc::new(Fixed(vec![a.clone(), b.clone()])), herdr_with(panes, argvs));
        let (conn, _rx) = conn();
        let result = dispatch(&req("w", "worktree.ps", json!({})), &c, &conn).unwrap()["result"].clone();
        assert_eq!(result["totalCount"], 2);
        let row = &result["worktrees"][0];
        assert_eq!(row["worktreeId"], a.worktree_id().as_str());
        assert_eq!(row["worktreeId"], a.worktree_id().as_str(), "stable id");
        assert_eq!(row["repoId"], catalog::repo_id("/home/u/work/alpha").as_str());
        assert_eq!(row["repo"], "alpha");
        assert_eq!(row["displayName"], "fix the login");
        assert_eq!(row["status"], "working");
        assert_eq!(row["liveTerminalCount"], 1);
        assert_eq!(row["hasAttachedPty"], true);
        assert_eq!(row["agents"][0]["paneKey"], "term_a");
        assert_eq!(row["agents"][0]["state"], "working");
        assert_eq!(result["worktrees"][1]["status"], "inactive", "no pane runs the older conversation");
        assert_eq!(result["worktrees"][1]["agents"], json!([]));
        for key in ["worktreeId", "repoId", "repo", "path", "branch", "displayName", "workspaceStatus", "sortOrder",
                    "linkedIssue", "linkedPR", "comment", "isPinned", "isActive", "unread", "liveTerminalCount",
                    "hasAttachedPty", "lastOutputAt", "preview", "status", "agents"] {
            assert!(row.get(key).is_some(), "row is missing {key}");
        }
    }

    #[test]
    fn worktree_list_still_loads_when_herdr_is_down() {
        let c = ctx(Arc::new(Fixed(vec![conv("t", "u", "/w", 1.0)])), Arc::new(Canned {
            answers: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        }));
        let (conn, _rx) = conn();
        let result = dispatch(&req("w", "worktree.ps", json!({})), &c, &conn).unwrap()["result"].clone();
        assert_eq!(result["totalCount"], 1);
        assert_eq!(result["worktrees"][0]["status"], "inactive");
    }

    #[test]
    fn a_catalog_failure_is_an_error_reply_not_an_empty_list() {
        let c = ctx(Arc::new(Broken), herdr_with(json!({}), json!({})));
        let (conn, _rx) = conn();
        let reply = dispatch(&req("w", "worktree.ps", json!({})), &c, &conn).unwrap();
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], "unavailable");
    }

    #[test]
    fn repo_list_groups_the_conversations_by_directory() {
        let c = ctx(
            Arc::new(Fixed(vec![conv("a", "u1", "/w/alpha", 2.0), conv("b", "u2", "/w/alpha", 1.0)])),
            herdr_with(json!({}), json!({})),
        );
        let (conn, _rx) = conn();
        let repos = dispatch(&req("r", "repo.list", json!({})), &c, &conn).unwrap()["result"]["repos"].clone();
        assert_eq!(repos.as_array().unwrap().len(), 1);
        assert_eq!(repos[0]["id"], catalog::repo_id("/w/alpha").as_str());
        assert_eq!(repos[0]["displayName"], "alpha");
    }

    #[test]
    fn tabs_are_the_panes_of_the_selected_conversation() {
        let c_ = conv("fix the login", "uuid-1", "/w/a", 1.0);
        let panes = json!({"type":"pane_list","panes":[
            {"pane_id":"w1:p1","terminal_id":"term_a","tab_id":"w1:t1","focused":false,"agent_status":"idle"},
            {"pane_id":"w1:p2","terminal_id":"term_b","tab_id":"w1:t2","focused":true,"agent_status":"idle"}
        ]});
        let argvs = json!({"process_info":{"foreground_processes":[{"argv":["omp","--resume","uuid-1"]}]}});
        let h = herdr_with(panes, argvs);
        let c = ctx(Arc::new(Fixed(vec![c_.clone()])), h);
        let (conn, _rx) = conn();
        let selector = format!("id:{}", c_.worktree_id());
        let result = dispatch(&req("t", "session.tabs.list", json!({"worktree": selector})), &c, &conn).unwrap()["result"].clone();
        assert_eq!(result["tabs"].as_array().unwrap().len(), 2);
        assert_eq!(result["activeTabId"], "term_b");
        assert_eq!(result["worktree"], c_.worktree_id().as_str());
        assert!(result["snapshotVersion"].as_u64().unwrap() > 0);
    }

    #[test]
    fn an_unknown_worktree_is_refused_and_a_missing_selector_is_a_bad_request() {
        let c = ctx(Arc::new(Fixed(vec![])), herdr_with(json!({}), json!({})));
        let (conn, _rx) = conn();
        let missing = dispatch(&req("t", "session.tabs.list", json!({"worktree": "id:nope::conv-x"})), &c, &conn).unwrap();
        assert_eq!(missing["error"]["code"], "worktree_not_found");
        let empty = dispatch(&req("t", "session.tabs.list", json!({})), &c, &conn).unwrap();
        assert_eq!(empty["error"]["code"], "bad_request");
    }

    #[test]
    fn terminal_list_rows_are_the_panes_with_their_handles() {
        let c_ = conv("t", "uuid-1", "/w/a", 1.0);
        let panes = json!({"type":"pane_list","panes":[
            {"pane_id":"w1:p1","terminal_id":"term_a","tab_id":"w1:t1","focused":true,"agent_status":"idle","agent":"omp"}
        ]});
        let argvs = json!({"process_info":{"foreground_processes":[{"argv":["omp","--resume","uuid-1"]}]}});
        let c = ctx(Arc::new(Fixed(vec![c_.clone()])), herdr_with(panes, argvs));
        let (conn, _rx) = conn();
        let all = dispatch(&req("l", "terminal.list", json!({})), &c, &conn).unwrap()["result"].clone();
        assert_eq!(all["terminals"][0]["handle"], "term_a");
        assert_eq!(all["terminals"][0]["worktreeId"], c_.worktree_id().as_str());
        let filtered = dispatch(&req("l", "terminal.list", json!({"handles": ["term_zzz"]})), &c, &conn).unwrap();
        assert_eq!(filtered["result"]["terminals"], json!([]));
    }

    #[test]
    fn a_text_send_reports_the_bytes_it_wrote() {
        let panes = json!({"type":"pane_list","panes":[
            {"pane_id":"w1:p1","terminal_id":"term_a","tab_id":"w1:t1","focused":true,"agent_status":"idle"}
        ]});
        let h = herdr_with(panes, json!({"process_info":{"foreground_processes":[]}}));
        let c = ctx(Arc::new(Fixed(vec![])), h.clone());
        let (conn, _rx) = conn();
        let reply = dispatch(&req("s", "terminal.send", json!({"terminal": "term_a", "text": "ls"})), &c, &conn).unwrap();
        assert_eq!(reply["result"]["send"]["accepted"], true);
        assert_eq!(reply["result"]["send"]["bytesWritten"], 2);
        let sent: Vec<String> = h.calls.lock().iter().map(|(m, _)| m.clone()).collect();
        assert!(sent.contains(&"pane.send_text".to_string()));
        assert!(!sent.contains(&"pane.send_keys".to_string()), "no Enter without enter:true");
    }

    #[test]
    fn a_submission_into_a_conversation_pane_is_refused_while_its_claim_is_held() {
        let c_ = conv("t", "uuid-1", "/w/a", 1.0);
        let panes = json!({"type":"pane_list","panes":[
            {"pane_id":"w1:p1","terminal_id":"term_a","tab_id":"w1:t1","focused":true,"agent_status":"idle","agent":"omp"}
        ]});
        let argvs = json!({"process_info":{"foreground_processes":[{"argv":["omp","--resume","uuid-1"]}]}});
        let h = herdr_with(panes, argvs);
        let dir = std::env::temp_dir().join(format!("sesync-rpc-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("sesync-lock");
        std::fs::write(&tool, "#!/bin/sh\ncase \"$1\" in claim) echo 'claude 4242'; exit 2;; esac\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut c = ctx(Arc::new(Fixed(vec![c_])), h.clone());
        c.locks = LockTools { sesync_lock: tool, lock_dir: dir.clone() };
        let (conn, _rx) = conn();
        let reply = dispatch(&req("s", "terminal.send", json!({"terminal": "term_a", "text": "fix it", "enter": true})), &c, &conn).unwrap();
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], "conversation_locked");
        assert!(reply["error"]["message"].as_str().unwrap().contains("claude 4242"));
        let sent: Vec<String> = h.calls.lock().iter().map(|(m, _)| m.clone()).collect();
        assert!(!sent.iter().any(|m| m.starts_with("agent.") || m == "pane.send_text"), "nothing is typed into a locked conversation");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
