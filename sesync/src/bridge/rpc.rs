//! The RPC subset the phone uses. A sesync conversation is a worktree, its herdr panes are its terminals
//! and tabs, and the host is this machine (see docs/BRIDGE.md, "Mapping").
//!   * `status.get`: what this host is, for the app's compatibility gate;
//!   * `worktree.ps` and `worktree.set`: the home list, one row per conversation, and its pin;
//!   * `repo.list`: the conversations' project directories, for grouping;
//!   * `session.tabs.list` / `session.tabs.subscribe` / `session.tabs.unsubscribe`: the panes running a
//!     conversation;
//!   * `terminal.list`, `terminal.read`, `terminal.send`, `terminal.clearBuffer`, `terminal.subscribe`, `terminal.unsubscribe`;
//!   * `agentSession.subscribeStatus`: the agent statuses of those panes;
//!   * `files.*` and `git.*`: read-only, confined to the project directory (files.rs, git.rs);
//!   * `notifications.testPush`: a synthetic in-app notification.
//!   * `speech.models.list` and `speech.dictation.*`: phone dictation, transcribed by a local whisper server (dictation.rs).
//!   * `machines.*` and `devices.revoke`: which machines see each other (grants.rs); a machine device is refused them.
//! Every other method answers `method_not_found`. The app reads that as "not available here" and skips
//! the read, so the rest of its screens degrade rather than fail.

use super::catalog::{self, Catalog, Conversation, Project};
use super::herdr::{self, Herdr};
use super::notify;
use super::dictation::Dictation;
use super::{confine, files, git, pins};
use super::devices::Kind;
use super::grants;
use super::start;
use super::stream::{self, Conn};
use super::terminals::{self, Running};
use super::transcript;
use super::turns::{LockTools, Turns};
use super::uploads;
use super::usage;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
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
    /// The bridge's state directory (`<sesync config>/bridge`). Owner-only bridge files live under it.
    pub dir: PathBuf,
    /// In-app notifications: the live stream, the recent history and the optional ntfy push.
    pub notify: notify::Hub,
    /// The latest plan usage per account, from `omp usage --json`, and its subscribers.
    pub usage: usage::Store,
    /// Image uploads in progress, by upload id.
    pub uploads: uploads::Store,
    /// Phone dictation: the one dictation in progress, and the whisper server it transcribes through.
    pub dictation: Dictation,
    /// How this host starts agent sessions (`preflight.detectAgents`, `agentSession.create`).
    pub launch: start::Launch,
    /// The machines this one may reach: `machines.json` in the sesync config directory, the file the `sesync machine`
    /// commands read. The `machines.*` RPCs keep it (`grants.rs`).
    pub machines: PathBuf,
}

/// A failed request: the error code the phone sees, and its message.
pub type Failure = (&'static str, String);

/// Answer one decrypted request frame, as a device of kind `caller`: the server passes the kind of the device the
/// frame was authenticated with (`server.rs`). The reply carries the request's `id`, as the app matches on it.
/// `None` means a stream took the request over: its first frame was already sent on `conn`.
pub fn dispatch(request: &Value, ctx: &Ctx, conn: &Conn, caller: Kind) -> Option<Value> {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let outcome = match method {
        "session.tabs.subscribe" => return stream_or_error(&id, tabs_subscribe(&params, ctx, conn, &id)),
        "terminal.subscribe" => return stream_or_error(&id, terminal_subscribe(&params, ctx, conn, &id)),
        "agentSession.subscribeStatus" => return stream_or_error(&id, status_subscribe(ctx, conn, &id)),
        "nativeChat.subscribe" => return stream_or_error(&id, transcript::subscribe(&params, ctx, conn, &id)),
        "notifications.subscribe" => return stream_or_error(&id, notify::subscribe(&params, ctx, conn, &id)),
        "accounts.subscribe" => return stream_or_error(&id, usage::accounts_subscribe(&params, ctx, conn, &id)),
        "status.get" => Ok(status(&ctx.host)),
        "worktree.ps" => worktree_ps(ctx),
        "repo.list" => repo_list(ctx),
        "session.tabs.list" => tabs_list(&params, ctx),
        "session.tabs.unsubscribe" | "terminal.unsubscribe" | "nativeChat.unsubscribe"
        | "notifications.unsubscribe" | "accounts.unsubscribe" => Ok(unsubscribe(&params, conn)),
        "terminal.list" => terminal_list(&params, ctx),
        "terminal.read" => terminal_read(&params, ctx),
        "terminal.send" => terminal_send(&params, ctx),
        "nativeChat.readSession" => transcript::read_session(&params, ctx),
        "clipboard.startImageUpload" => uploads::start(&params, ctx),
        "clipboard.appendImageUploadChunk" => uploads::append(&params, ctx),
        "clipboard.commitImageUpload" => uploads::commit(&params, ctx),
        "clipboard.abortImageUpload" => uploads::abort(&params, ctx),
        "clipboard.saveImageAsTempFile" => uploads::save_temp_file(&params, ctx),
        "preflight.detectAgents" => start::detect_agents(&params, ctx),
        "settings.get" => start::settings_get(&params, ctx),
        "session.tabs.createTerminal" => start::create_terminal_tab(&params, ctx),
        "notifications.getMissedSince" => notify::get_missed_since(&params, ctx),
        "notifications.registerPush" | "notifications.unregisterPush" => Ok(json!({})),
        "accounts.list" => usage::accounts_list(&params, ctx),
        "worktree.set" => pins::set(&params, ctx),
        "terminal.clearBuffer" => terminal_clear(&params, ctx),
        "notifications.testPush" => notify::test_push(&params, ctx),
        "machines.grant" | "machines.accept" | "machines.list" | "machines.forget" | "devices.revoke" if caller == Kind::Machine => {
            return Some(error(&id, "forbidden", "a machine cannot change which machines see each other; only a phone can"));
        }
        "machines.grant" => grants::grant(&params, ctx),
        "machines.accept" => grants::accept(&params, ctx),
        "machines.list" => grants::list(ctx),
        "machines.forget" => grants::forget(&params, ctx),
        "devices.revoke" => grants::revoke_device(&params, ctx),
        "files.readDir" => files::read_dir(&params, ctx),
        "files.read" => files::read(&params, ctx),
        "files.readPreview" => files::read_preview(&params, ctx),
        "files.resolveTerminalPath" => files::resolve_terminal_path(&params, ctx),
        "files.searchPaths" => files::search_paths(&params, ctx),
        "git.status" => git::status(&params, ctx),
        "git.diff" => git::diff(&params, ctx),
        "git.history" => git::history(&params, ctx),
        "git.commitCompare" => git::commit_compare(&params, ctx),
        "git.branchCompare" => git::branch_compare(&params, ctx),
        "speech.models.list" => Ok(ctx.dictation.models()),
        "speech.dictation.start" => ctx.dictation.start(&params, conn.id()),
        "speech.dictation.chunk" => ctx.dictation.chunk(&params, conn.id()),
        "speech.dictation.finish" => ctx.dictation.finish(&params, conn.id()),
        "speech.dictation.cancel" => ctx.dictation.cancel(&params, conn.id()),
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
    let conversations = ctx.catalog.conversations().map_err(|m| ("unavailable", m))?;
    let conversation = conversations
        .iter()
        .find(|c| c.worktree_id() == worktree)
        .cloned()
        .ok_or_else(|| ("worktree_not_found", format!("no conversation {worktree}")))?;
    let panes = terminals::running_panes(ctx.herdr.as_ref(), &conversations).map_err(|m| ("herdr_unavailable", m))?;
    Ok((conversation, panes))
}

/// The conversation a request's `worktree` selector names. Fails when it is not listed.
pub fn conversation_of_params(ctx: &Ctx, params: &Value) -> Result<Conversation, Failure> {
    let worktree = worktree_param(params)?;
    ctx.catalog
        .conversations()
        .map_err(|m| ("unavailable", m))?
        .into_iter()
        .find(|c| c.worktree_id() == worktree)
        .ok_or_else(|| ("worktree_not_found", format!("no conversation {worktree}")))
}

/// The project directory a request's `worktree` selector names, canonical. The file and git methods
/// resolve every path under it (confine.rs).
pub fn project_root(ctx: &Ctx, params: &Value) -> Result<PathBuf, Failure> {
    confine::root_of(&conversation_of_params(ctx, params)?.dir)
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
    let panes = terminals::running_panes(ctx.herdr.as_ref(), &conversations).unwrap_or_default();
    let now = terminals::now_ms();
    let pinned = pins::load(&ctx.dir).unwrap_or_else(|m| {
        eprintln!("sesync bridge: {m}");
        Default::default()
    });
    let rows: Vec<Value> = conversations
        .iter()
        .map(|c| {
            let mut row = worktree_row(c, &terminals::panes_of(&panes, c), now);
            row["isPinned"] = json!(pinned.contains(&c.worktree_id()));
            row
        })
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
        "conversationAgent": c.harness,
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
            let panes = terminals::running_panes(ctx.herdr.as_ref(), &conversations).map_err(|m| ("herdr_unavailable", m))?;
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

/// `terminal.clearBuffer {terminal}`: Ctrl-L into the pane. A shell clears its screen and a full-screen
/// agent redraws. Like `interrupt`, it is not a prompt turn, so it claims no conversation.
fn terminal_clear(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let handle = handle_param(params)?;
    let pane = find_terminal(ctx, &handle)?;
    herdr::send_keys(ctx.herdr.as_ref(), &pane.pane_id, &["ctrl+l"]).map_err(|m| ("send_failed", m))?;
    Ok(json!({ "terminal": { "handle": handle, "cleared": true } }))
}

/// A prompt submission. When the pane runs a conversation, the conversation is claimed in sesync-lock
/// first and released when the agent's turn ends. A pane with a detected agent gets the prompt through
/// herdr's agent API, which orders the text before Enter; any other pane gets the text, then Enter.
/// A conversation's pane that no longer runs its agent (the agent quit, and the shell is left) takes no
/// prompt, and a pane waiting on a prompt is answered in the pane itself, never by a prompt the phone sends.
fn submit(ctx: &Ctx, pane: &herdr::Pane, text: &str) -> Result<(), Failure> {
    let h = ctx.herdr.as_ref();
    let conversations = ctx.catalog.conversations().map_err(|m| ("unavailable", m))?;
    let panes = terminals::running_panes(h, &conversations).map_err(|m| ("herdr_unavailable", m))?;
    let conv = panes
        .iter()
        .find(|r| r.pane.terminal_id == pane.terminal_id)
        .and_then(|r| terminals::conversation_of(r, &conversations))
        .cloned();
    if conv.is_none() && pane.agent.is_none() {
        let labels = herdr::tab_labels(h).map_err(|m| ("herdr_unavailable", m))?;
        if labels.get(&pane.tab_id).is_some_and(|label| terminals::is_session_tab(label)) {
            return Err(("not_running_agent", "that conversation's pane no longer runs its agent, so the message was not typed into its shell".into()));
        }
    }
    if pane.agent_status == "blocked" {
        return Err(("agent_waiting_on_prompt", "that pane is waiting on a prompt; answer it in the pane first".into()));
    }
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
    // The pane's real size, as herdr lays it out: the phone shows the pane as it is. Only when herdr cannot
    // give a size is the phone's viewport announced, in auto mode.
    let (cols, rows, display_mode) = match herdr::pane_size(ctx.herdr.as_ref(), &pane.pane_id) {
        Some((cols, rows)) => (cols, rows, stream::DESKTOP),
        None => {
            let cols = params["viewport"]["cols"].as_u64().unwrap_or(80).clamp(1, 1000) as u32;
            let rows = params["viewport"]["rows"].as_u64().or(pane.rows).unwrap_or(24).clamp(1, 500) as u32;
            (cols, rows, stream::AUTO)
        }
    };
    let shown = stream::screen_text(
        &herdr::read_visible(ctx.herdr.as_ref(), &pane.pane_id).map_err(|m| ("herdr_unavailable", m))?,
    );
    let stream_id = conn.next_stream_id();
    let stop = conn.register(key, id.clone());
    let head = if binary {
        json!({ "type": "subscribed", "streamId": stream_id, "lines": [], "truncated": false,
                "cols": cols, "rows": rows, "displayMode": display_mode, "seq": 0 })
    } else {
        json!({ "type": "scrollback", "lines": [], "truncated": false, "serialized": shown.clone(),
                "cols": cols, "rows": rows, "displayMode": display_mode, "seq": 0 })
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
        display_mode,
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
    let panes = terminals::running_panes(ctx.herdr.as_ref(), &conversations).map_err(|m| ("herdr_unavailable", m))?;
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
        herdr_sized(pane_list, argvs, Some((57, 27)))
    }

    /// `herdr_with`, with `pane.layout` answering the pane at `size` (None: herdr has no layout for it).
    fn herdr_sized(pane_list: Value, argvs: Value, size: Option<(u64, u64)>) -> Arc<Canned> {
        let mut answers = HashMap::new();
        answers.insert("pane.list", pane_list);
        answers.insert("pane.process_info", argvs);
        answers.insert("pane.get", json!({"type":"pane_info","pane":{"pane_id":"w1:p1","terminal_id":"term_a","tab_id":"w1:t1","agent_status":"working"}}));
        answers.insert("pane.read", json!({"type":"pane_read","read":{"text":"hello\r\n","pane_id":"w1:p1"}}));
        answers.insert("pane.send_text", json!({"type":"ok"}));
        answers.insert("pane.send_keys", json!({"type":"ok"}));
        answers.insert("agent.prompt", json!({"type":"agent_prompted"}));
        if let Some((width, height)) = size {
            let rect = json!({"x": 0, "y": 0, "width": width, "height": height});
            answers.insert("pane.layout", json!({"type":"pane_layout","layout":{"focused_pane_id":"w1:p1","panes":[{"pane_id":"w1:p1","focused":true,"rect":rect}],"splits":[]}}));
        }
        Arc::new(Canned { answers, calls: Mutex::new(Vec::new()) })
    }

    fn ctx(catalog: Arc<dyn Catalog>, herdr: Arc<dyn Herdr>) -> Ctx {
        Ctx {
            catalog,
            herdr,
            host: Host { machine_name: "box".into(), platform: "linux" },
            locks: LockTools { sesync_lock: "/nonexistent/sesync-lock".into(), lock_dir: "/nonexistent".into() },
            turns: Turns::default(),
            dir: std::env::temp_dir().join("sesync-rpc-unit"),
            machines: std::env::temp_dir().join("sesync-rpc-unit").join("machines.json"),
            notify: notify::Hub::new(None),
            usage: usage::Store::default(),
            uploads: uploads::Store::default(),
            dictation: crate::bridge::dictation::Dictation::new(crate::bridge::dictation::Whisper::from_env()),
            launch: start::Launch::default(),
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

    /// Answers as a paired phone. The machine cases call `super::dispatch` with `Kind::Machine`.
    fn dispatch(request: &Value, ctx: &Ctx, conn: &Conn) -> Option<Value> {
        super::dispatch(request, ctx, conn, Kind::Phone)
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
    fn a_picker_started_pane_is_the_tab_of_the_conversation_its_title_and_directory_name() {
        let main = conv("Main agent", "uuid-1", "/w/a", 1.0);
        let other = conv("Main agent", "uuid-2", "/w/b", 2.0);
        let panes = json!({"type":"pane_list","panes":[
            {"pane_id":"w1:p1","terminal_id":"term_a","tab_id":"w1:t1","focused":true,"agent_status":"idle",
             "agent":"omp","terminal_title":"π ⠴ Main agent","foreground_cwd":"/w/a"}
        ]});
        let argvs = json!({"process_info":{"foreground_processes":[{"argv":["omp","--resume"]}]}});
        let c = ctx(Arc::new(Fixed(vec![main.clone(), other.clone()])), herdr_with(panes, argvs));
        let (conn, _rx) = conn();
        let tabs_of = |wanted: &Conversation| {
            let request = req("t", "session.tabs.list", json!({"worktree": format!("id:{}", wanted.worktree_id())}));
            dispatch(&request, &c, &conn).unwrap()["result"]["tabs"].clone()
        };
        let tabs = tabs_of(&main);
        assert_eq!(tabs.as_array().unwrap().len(), 1, "{tabs}");
        assert_eq!(tabs[0]["terminal"], "term_a");
        assert!(tabs_of(&other).as_array().unwrap().is_empty(), "the other directory's conversation shows no tab");
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
        crate::fake_cli::stage(&tool, "#!/bin/sh\ncase \"$1\" in claim) echo 'claude 4242'; exit 2;; esac\nexit 1\n");
        let mut c = ctx(Arc::new(Fixed(vec![c_])), h.clone());
        c.locks = LockTools { sesync_lock: tool, lock_dir: dir.clone() };
        let (conn, _rx) = conn();
        let reply = dispatch(&req("s", "terminal.send", json!({"terminal": "term_a", "text": "fix it", "enter": true})), &c, &conn).unwrap();
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], "conversation_locked");
        assert!(reply["error"]["message"].as_str().unwrap().contains("claude 4242"));
        let sent: Vec<String> = h.calls.lock().iter().map(|(m, _)| m.clone()).collect();
        assert!(
            !sent.iter().any(|m| matches!(m.as_str(), "agent.prompt" | "pane.send_text" | "pane.send_keys")),
            "nothing is typed into a locked conversation: {sent:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_buffer_sends_ctrl_l_into_the_pane() {
        let panes = json!({"type":"pane_list","panes":[
            {"pane_id":"w1:p1","terminal_id":"term_a","tab_id":"w1:t1","focused":true,"agent_status":"idle"}
        ]});
        let h = herdr_with(panes, json!({"process_info":{"foreground_processes":[]}}));
        let c = ctx(Arc::new(Fixed(vec![])), h.clone());
        let (conn, _rx) = conn();
        let reply = dispatch(&req("c", "terminal.clearBuffer", json!({"terminal": "term_a"})), &c, &conn).unwrap();
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["result"]["terminal"]["cleared"], true);
        let keys: Vec<Value> = h.calls.lock().iter().filter(|(m, _)| m == "pane.send_keys").map(|(_, p)| p["keys"].clone()).collect();
        assert_eq!(keys, vec![json!(["ctrl+l"])]);
    }

    #[test]
    fn clear_buffer_on_a_pane_that_is_gone_is_terminal_not_found() {
        let c = ctx(Arc::new(Fixed(vec![])), herdr_with(json!({"type":"pane_list","panes":[]}), json!({})));
        let (conn, _rx) = conn();
        let reply = dispatch(&req("c", "terminal.clearBuffer", json!({"terminal": "term_a"})), &c, &conn).unwrap();
        assert_eq!(reply["error"]["code"], "terminal_not_found");
    }

    /// The first message a connection sent, opened: the stream's head, its JSON `result`.
    fn first_event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<tokio_tungstenite::tungstenite::Message>) -> Value {
        let tokio_tungstenite::tungstenite::Message::Text(text) = rx.try_recv().expect("the subscribe reply") else {
            panic!("the subscribe reply is a JSON frame");
        };
        let channel = crate::bridge::e2ee::Channel::new(
            &crypto_box::SecretKey::from_slice(&[1u8; 32]).unwrap().public_key(),
            &crypto_box::SecretKey::from_slice(&[2u8; 32]).unwrap(),
        );
        let plain = channel.open(text.as_str()).expect("a sealed frame");
        serde_json::from_str::<Value>(&plain).unwrap()["result"].clone()
    }

    fn one_pane() -> Value {
        json!({"type":"pane_list","panes":[{"pane_id":"w1:p1","terminal_id":"term_a","tab_id":"w1:t1","focused":true,"agent_status":"idle"}]})
    }

    #[test]
    fn a_subscribe_announces_the_pane_size_in_desktop_mode_not_the_phones_viewport() {
        let c = ctx(Arc::new(Fixed(vec![])), herdr_with(one_pane(), json!({"process_info":{"foreground_processes":[]}})));
        let (conn, mut rx) = conn();
        let _ = dispatch(&req("sub", "terminal.subscribe", json!({"terminal": "term_a", "client": {"id": "phone"}, "viewport": {"cols": 51, "rows": 30}})), &c, &conn);
        let head = first_event(&mut rx);
        assert_eq!(head["type"], "scrollback", "{head}");
        assert_eq!((head["cols"].as_u64(), head["rows"].as_u64()), (Some(57), Some(27)), "{head}");
        assert_eq!(head["displayMode"], "desktop", "{head}");
        assert_eq!(head["serialized"], "hello\r\n");
    }

    #[test]
    fn a_binary_subscribe_announces_the_pane_size_in_desktop_mode() {
        let c = ctx(Arc::new(Fixed(vec![])), herdr_with(one_pane(), json!({"process_info":{"foreground_processes":[]}})));
        let (conn, mut rx) = conn();
        let _ = dispatch(&req("sub", "terminal.subscribe", json!({"terminal": "term_a", "client": {"id": "phone"}, "viewport": {"cols": 51, "rows": 30}, "capabilities": {"terminalBinaryStream": 1}})), &c, &conn);
        let head = first_event(&mut rx);
        assert_eq!(head["type"], "subscribed", "{head}");
        assert_eq!((head["cols"].as_u64(), head["rows"].as_u64()), (Some(57), Some(27)), "{head}");
        assert_eq!(head["displayMode"], "desktop", "{head}");
    }

    #[test]
    fn without_a_layout_the_phones_viewport_is_announced_in_auto_mode() {
        let c = ctx(Arc::new(Fixed(vec![])), herdr_sized(one_pane(), json!({"process_info":{"foreground_processes":[]}}), None));
        let (conn, mut rx) = conn();
        let _ = dispatch(&req("sub", "terminal.subscribe", json!({"terminal": "term_a", "client": {"id": "phone"}, "viewport": {"cols": 51, "rows": 30}})), &c, &conn);
        let head = first_event(&mut rx);
        assert_eq!((head["cols"].as_u64(), head["rows"].as_u64()), (Some(51), Some(30)), "{head}");
        assert_eq!(head["displayMode"], "auto", "{head}");
    }

    #[test]
    fn a_pin_shows_on_the_worktree_row_until_it_is_removed() {
        let dir = crate::bridge::testkit::sandbox("rpc-pin");
        let project = conv("alpha", "uuid-pin", dir.join("project").to_str().unwrap(), 1.0);
        let worktree = format!("id:{}", project.worktree_id());
        let c = crate::bridge::testkit::ctx_with(vec![project], &dir.join("bridge"));
        let (conn, _rx) = conn();
        let pinned = |c: &Ctx| {
            let reply = dispatch(&req("p", "worktree.ps", json!({})), c, &conn).unwrap();
            reply["result"]["worktrees"][0]["isPinned"].clone()
        };
        assert_eq!(pinned(&c), json!(false));
        let set = dispatch(&req("s", "worktree.set", json!({"worktree": worktree, "isPinned": true})), &c, &conn).unwrap();
        assert_eq!(set["ok"], true);
        assert_eq!(pinned(&c), json!(true));
        dispatch(&req("u", "worktree.set", json!({"worktree": worktree, "isPinned": false})), &c, &conn).unwrap();
        assert_eq!(pinned(&c), json!(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_verbs_and_unserved_methods_stay_refused() {
        let c = ctx(Arc::new(Fixed(vec![])), herdr_with(json!({}), json!({})));
        let (conn, _rx) = conn();
        for method in ["git.commit", "git.push", "git.stage", "files.createFile", "files.writeTerminalArtifact", "accounts.selectClaude", "worktree.create"] {
            let reply = dispatch(&req("w", method, json!({})), &c, &conn).unwrap();
            assert_eq!(reply["error"]["code"], "method_not_found", "{method}");
        }
    }

    #[test]
    fn every_read_method_is_routed_to_its_module() {
        let c = ctx(Arc::new(Fixed(vec![])), herdr_with(json!({}), json!({})));
        let (conn, _rx) = conn();
        for method in [
            "files.readDir", "files.read", "files.readPreview", "files.resolveTerminalPath", "files.searchPaths",
            "git.status", "git.diff", "git.history", "git.commitCompare", "git.branchCompare",
            "worktree.set", "terminal.clearBuffer", "notifications.testPush",
        ] {
            let reply = dispatch(&req("r", method, json!({})), &c, &conn).unwrap();
            assert_ne!(reply["error"]["code"], "method_not_found", "{method} is not routed");
        }
    }

    #[test]
    fn a_live_repository_is_read_through_dispatch_and_confined() {
        let dir = crate::bridge::testkit::sandbox("rpc-live-git");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(&project)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(project.join("a.txt"), "one\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "first"]);
        std::fs::write(project.join("a.txt"), "two\n").unwrap();
        std::fs::write(project.join("new.txt"), "fresh\n").unwrap();
        let project_conv = conv("alpha", "uuid-live", project.to_str().unwrap(), 1.0);
        let worktree = format!("id:{}", project_conv.worktree_id());
        let c = crate::bridge::testkit::ctx_with(vec![project_conv], &dir.join("bridge"));
        let (conn, _rx) = conn();

        let status = dispatch(&req("g", "git.status", json!({"worktree": worktree})), &c, &conn).unwrap();
        let entries = status["result"]["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| e["path"] == "a.txt" && e["area"] == "unstaged" && e["status"] == "modified"));
        assert!(entries.iter().any(|e| e["path"] == "new.txt" && e["area"] == "untracked"));

        let tree = dispatch(&req("d", "files.readDir", json!({"worktree": worktree, "relativePath": ""})), &c, &conn).unwrap();
        let names: Vec<&str> = tree["result"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["a.txt", "new.txt"], "the .git directory is never listed");

        for escape in ["../secret", ".git/config", "/etc/passwd"] {
            let refused = dispatch(&req("e", "files.read", json!({"worktree": worktree, "relativePath": escape})), &c, &conn).unwrap();
            assert_eq!(refused["error"]["code"], "invalid_relative_path", "{escape}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A context whose conversations resume through the in-memory herdr `herdr`, and whose lock tool grants
    /// every claim, so a send into a conversation's pane is not refused for a held turn.
    fn resuming_ctx(conversations: Vec<Conversation>, herdr: &Arc<crate::bridge::testkit::FakeHerdr>) -> Ctx {
        // One directory per call: tests run in parallel, and two staging the same fake tool path race on its symlink.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sesync-rpc-send-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("sesync-lock");
        crate::fake_cli::stage(&tool, "#!/bin/sh\nexit 0\n");
        let launch = crate::bridge::testkit::resume_launch(herdr, crate::harness::Locations::default(), dir.join("home"), std::time::Duration::from_secs(1));
        let mut c = crate::bridge::testkit::ctx_on(conversations, herdr.clone(), launch, &dir);
        c.locks = LockTools { sesync_lock: tool, lock_dir: dir };
        c
    }

    /// The tab label `herdr_open` gives a conversation's tab: its harness, its title, and its session tag.
    fn session_label(conv: &Conversation) -> String {
        crate::herdr_open::tab_label(&conv.harness, &conv.title, &crate::herdr_open::session_tag(&conv.key))
    }

    #[test]
    fn a_prompt_to_a_running_conversation_reaches_its_agent_pane() {
        let c_ = conv("a", "/s/a.jsonl", "/work/a", 1.0);
        let herdr = crate::bridge::testkit::FakeHerdr::new();
        let pane = herdr.open_tab("w-main", &session_label(&c_), "/work/a", &["omp", "--resume", "/s/a.jsonl"]);
        let c = resuming_ctx(vec![c_], &herdr);
        let (conn, _rx) = conn();
        let reply = dispatch(&req("s", "terminal.send", json!({"terminal": pane.terminal_id, "text": "hello", "enter": true})), &c, &conn).unwrap();
        assert_eq!(reply["result"]["send"]["accepted"], true, "{reply}");
        assert_eq!(herdr.sends(), vec![(pane.pane_id.clone(), "hello".to_string())], "the agent's pane got the prompt");
    }

    #[test]
    fn a_prompt_to_a_conversation_whose_agent_quit_is_refused_and_types_nothing() {
        let c_ = conv("a", "/s/a.jsonl", "/work/a", 1.0);
        let herdr = crate::bridge::testkit::FakeHerdr::new();
        let pane = herdr.open_tab("w-main", &session_label(&c_), "/work/a", &["omp", "--resume", "/s/a.jsonl"]);
        herdr.quit_agent(&pane.terminal_id);
        let c = resuming_ctx(vec![c_], &herdr);
        let (conn, _rx) = conn();
        let reply = dispatch(&req("s", "terminal.send", json!({"terminal": pane.terminal_id, "text": "hello", "enter": true})), &c, &conn).unwrap();
        assert_eq!(reply["error"]["code"], "not_running_agent", "{reply}");
        assert!(herdr.sends().is_empty(), "nothing was typed into the bare shell: {:?}", herdr.calls());
    }

    #[test]
    fn a_prompt_to_a_pane_waiting_on_a_prompt_is_refused_so_it_does_not_answer_it() {
        let c_ = conv("a", "/s/a.jsonl", "/work/a", 1.0);
        let herdr = crate::bridge::testkit::FakeHerdr::new();
        let pane = herdr.open_tab("w-main", &session_label(&c_), "/work/a", &["omp", "--resume", "/s/a.jsonl"]);
        herdr.set_status(&pane.terminal_id, "blocked");
        let c = resuming_ctx(vec![c_], &herdr);
        let (conn, _rx) = conn();
        let reply = dispatch(&req("s", "terminal.send", json!({"terminal": pane.terminal_id, "text": "hello", "enter": true})), &c, &conn).unwrap();
        assert_eq!(reply["error"]["code"], "agent_waiting_on_prompt", "{reply}");
        assert!(herdr.sends().is_empty(), "the prompt's answer was not typed");
    }

    #[test]
    fn a_prompt_to_a_plain_shell_tab_is_unchanged() {
        let herdr = crate::bridge::testkit::FakeHerdr::new();
        let pane = herdr.open_tab("w-main", "terminal: proj", "/work/proj", &[]);
        let c = resuming_ctx(vec![], &herdr);
        let (conn, _rx) = conn();
        let reply = dispatch(&req("s", "terminal.send", json!({"terminal": pane.terminal_id, "text": "ls", "enter": true})), &c, &conn).unwrap();
        assert_eq!(reply["result"]["send"]["accepted"], true, "{reply}");
        assert_eq!(herdr.sends(), vec![(pane.pane_id.clone(), "ls".to_string())]);
    }

    #[test]
    fn worktree_rows_name_the_harness_of_their_conversation() {
        let c_ = conv("a", "/s/a.jsonl", "/work/a", 1.0);
        let herdr = crate::bridge::testkit::FakeHerdr::new();
        let c = resuming_ctx(vec![c_], &herdr);
        let (conn, _rx) = conn();
        let reply = dispatch(&req("w", "worktree.ps", json!({})), &c, &conn).unwrap();
        assert_eq!(reply["result"]["worktrees"][0]["conversationAgent"], "omp", "{reply}");
    }

    #[test]
    fn a_machine_device_is_refused_the_grant_methods_and_a_phone_is_not() {
        let dir = crate::bridge::testkit::sandbox("rpc-grant-kinds");
        let c = crate::bridge::testkit::ctx_with(vec![], &dir);
        let (conn, _rx) = conn();
        let key = "zo060cy2M+x7cMF4FKXHbs0CloUFDTRHRboFhw5YfVk=";
        let calls = [
            ("machines.grant", json!({ "label": "A" })),
            ("machines.accept", json!({ "name": "b", "endpoint": "ws://127.0.0.1:9", "deviceToken": "t", "publicKeyB64": key, "grantDeviceId": "g" })),
            ("machines.list", json!({})),
            ("machines.forget", json!({ "name": "b" })),
            ("devices.revoke", json!({ "deviceId": "x" })),
        ];
        for (method, params) in &calls {
            let reply = super::dispatch(&req("m", method, params.clone()), &c, &conn, Kind::Machine).unwrap();
            assert_eq!(reply["id"], "m");
            assert_eq!(reply["error"]["code"], "forbidden", "{method}: {reply}");
        }
        // The same requests from a phone are answered by their own outcomes.
        let granted = dispatch(&req("g", "machines.grant", json!({ "label": "A" })), &c, &conn).unwrap();
        assert_eq!(granted["ok"], true, "{granted}");
        let listed = dispatch(&req("l", "machines.list", json!({})), &c, &conn).unwrap();
        assert_eq!(listed["result"], json!({ "machines": [] }), "{listed}");
        let forgot = dispatch(&req("f", "machines.forget", json!({ "name": "b" })), &c, &conn).unwrap();
        assert_eq!(forgot["error"]["code"], "not_found", "{forgot}");
        let revoked = dispatch(&req("r", "devices.revoke", json!({ "deviceId": "x" })), &c, &conn).unwrap();
        assert_eq!(revoked["error"]["code"], "not_found", "{revoked}");
        let accepted = dispatch(&req("a", "machines.accept", json!({ "name": "b" })), &c, &conn).unwrap();
        assert_eq!(accepted["error"]["code"], "bad_request", "{accepted}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
