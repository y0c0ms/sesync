//! process carries one of the conversation's session ids: the rule `herdr_open` uses for "open in
//! herdr", so the phone and the TUI agree on which pane is running a session. A terminal's handle is
//! herdr's terminal id, which stays with the terminal when its pane moves.
//!
//! Everything here is a pure mapping over herdr's answers, so the unit tests drive it with canned
//! pane lists.

use super::approvals;
use super::catalog::Conversation;
use super::herdr::{self, Herdr, Pane};
use crate::herdr_open;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// How many recent screen lines are read to find a blocked pane's prompt.
const SCREEN_LINES: u32 = 60;

/// A herdr pane with the argument lists of its foreground processes. `prompt` is set only when herdr
/// reports the pane blocked on a prompt the bridge recognises: the app's `interactivePrompt` envelope.
/// `owner` is the worktree id the bridge opened the pane for (see `remember_pane`).
#[derive(Debug, Clone, PartialEq)]
pub struct Running {
    pub pane: Pane,
    pub argvs: Vec<Vec<String>>,
    pub prompt: Option<String>,
    pub owner: Option<String>,
}

/// Panes the bridge opened, by herdr terminal id, with the worktree id they were opened for. A pane
/// opened without a resume runs no session id yet, so its argv cannot place it; this does. Entries of
/// panes herdr no longer lists are dropped on the next listing.
static OPENED: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Record that the pane `terminal_id` belongs to the worktree `worktree_id`.
pub fn remember_pane(terminal_id: &str, worktree_id: &str) {
    OPENED.lock().insert(terminal_id.to_string(), worktree_id.to_string());
}

/// The worktree id a pane was opened for, when the bridge opened it.
pub fn owner_of(terminal_id: &str) -> Option<String> {
    OPENED.lock().get(terminal_id).cloned()
}

/// Every pane herdr has, with its foreground processes. Fails closed: a pane whose processes cannot
/// be read fails the listing, so no conversation is reported idle while one of its panes is unknown.
pub fn running_panes(h: &dyn Herdr) -> Result<Vec<Running>, String> {
    let panes = herdr::list_panes(h)?;
    let owners: HashMap<String, String> = {
        let mut opened = OPENED.lock();
        opened.retain(|terminal, _| panes.iter().any(|p| p.terminal_id == *terminal));
        opened.clone()
    };
    panes
        .into_iter()
        .map(|pane| {
            let argvs = herdr::foreground_argvs(h, &pane.pane_id)
                .map_err(|e| format!("could not read the processes of pane {}: {e}", pane.pane_id))?;
            let prompt = blocked_prompt(h, &pane);
            let owner = owners.get(&pane.terminal_id).cloned();
            Ok(Running { pane, argvs, prompt, owner })
        })
        .collect()
}

/// The prompt a blocked pane is waiting on, read from its recent screen. Best effort: a pane whose
/// screen cannot be read shows no card rather than failing the listing.
fn blocked_prompt(h: &dyn Herdr, pane: &Pane) -> Option<String> {
    if pane.agent_status != "blocked" {
        return None;
    }
    let screen = herdr::read_recent_text(h, &pane.pane_id, SCREEN_LINES).ok()?;
    approvals::interactive_prompt(&screen, pane.agent.as_deref().unwrap_or(""))
}

/// The panes that run `conv`, in herdr's order.
pub fn panes_of<'a>(panes: &'a [Running], conv: &Conversation) -> Vec<&'a Running> {
    panes.iter().filter(|r| runs(r, conv)).collect()
}

/// The first conversation that claims `pane`. Conversations are newest first, so a pane both could
/// match goes to the newer one.
pub fn conversation_of<'a>(pane: &Running, conversations: &'a [Conversation]) -> Option<&'a Conversation> {
    conversations.iter().find(|c| runs(pane, c))
}

fn runs(r: &Running, conv: &Conversation) -> bool {
    r.owner.as_deref() == Some(conv.worktree_id().as_str())
        || conv.idents.iter().any(|ident| herdr_open::argv_carries(&r.argvs, ident))
}

/// The label a terminal shows: the harness and the conversation's title, as "open in herdr" names a tab.
pub fn label(conv: &Conversation) -> String {
    herdr_open::tab_label(&conv.harness, &conv.title)
}

/// One `session.tabs` entry for a pane running the conversation. Only terminal tabs: sesync has no
/// other tab kinds to show.
pub fn tab(r: &Running, conv: &Conversation) -> Value {
    json!({
        "type": "terminal",
        "id": r.pane.terminal_id,
        "title": label(conv),
        "isActive": r.pane.focused,
        "terminal": r.pane.terminal_id,
        "status": "ready",
        "leafId": r.pane.terminal_id,
        "agentStatus": agent_status(r, conv),
    })
}

/// The `agentStatus` of a tab row (src/shared/agent-status-types.ts). `state` is herdr's status in the
/// app's four states, idle and unknown both `waiting`. A card opens when the state is `blocked` or
/// `waiting` and `interactivePrompt` is set. `providerSession.id` is the conversation's journal session
/// id, the one `nativeChat.readSession` reads by, not the herdr pane id.
pub fn agent_status(r: &Running, conv: &Conversation) -> Value {
    let state = agent_state(&r.pane.agent_status);
    let since = state_since(&r.pane.terminal_id, state);
    let session_id = super::transcript::session_id(conv);
    let mut status = json!({
        "state": state,
        "prompt": "",
        "updatedAt": since,
        "stateStartedAt": since,
        "paneKey": format!("{}:{}", r.pane.tab_id, r.pane.terminal_id),
        "terminalHandle": r.pane.terminal_id,
        "agentType": conv.harness,
        "stateHistory": [],
        "providerSession": { "key": "session_id", "id": session_id },
    });
    if let Some(prompt) = &r.prompt {
        status["interactivePrompt"] = json!(prompt);
    }
    status
}

/// When each pane's current state began, as this bridge first saw it. Kept across listings, so the tab
/// rows do not change on every poll.
static STATE_SINCE: LazyLock<Mutex<HashMap<String, (&'static str, f64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn state_since(terminal_id: &str, state: &'static str) -> f64 {
    let mut seen = STATE_SINCE.lock();
    match seen.get(terminal_id) {
        Some((previous, since)) if *previous == state => *since,
        _ => {
            let now = now_ms();
            seen.insert(terminal_id.to_string(), (state, now));
            now
        }
    }
}

/// The `session.tabs.list` result: the conversation's terminals, with the focused one active.
/// `worktree` is the id the phone asked for, echoed back as the app expects.
pub fn tabs_result(worktree: &str, conv: &Conversation, panes: &[&Running], epoch: &str, version: u64) -> Value {
    let tabs: Vec<Value> = panes.iter().map(|r| tab(r, conv)).collect();
    let active = panes.iter().find(|r| r.pane.focused).or(panes.first()).map(|r| r.pane.terminal_id.clone());
    json!({
        "worktree": worktree,
        "publicationEpoch": epoch,
        "snapshotVersion": version,
        "tabs": tabs,
        "activeTabId": active,
        "activeTabType": active.as_ref().map(|_| "terminal"),
    })
}

/// One `terminal.list` row for a pane running the conversation.
pub fn terminal_row(r: &Running, conv: &Conversation) -> Value {
    let mut row = json!({
        "handle": r.pane.terminal_id,
        "title": label(conv),
        "connected": true,
        "writable": true,
        "orphaned": false,
        "worktreeId": conv.worktree_id(),
        "worktreePath": conv.dir,
        "branch": "",
        "tabId": r.pane.tab_id,
        "leafId": r.pane.terminal_id,
        "ptyId": Value::Null,
        "lastOutputAt": Value::Null,
        "preview": "",
    });
    if let Some(agent) = &r.pane.agent {
        row["agentIdentity"] = json!(agent);
    }
    row
}

/// The phone's agent state for one herdr status: `working`, `blocked`, `done`, or `waiting` for an
/// idle or unclassified agent.
pub fn agent_state(herdr_status: &str) -> &'static str {
    match herdr_status {
        "working" => "working",
        "blocked" => "blocked",
        "done" => "done",
        _ => "waiting",
    }
}

/// The structured-session status for one herdr status. None when herdr has not classified the agent.
pub fn session_status(herdr_status: &str) -> Option<&'static str> {
    match herdr_status {
        "working" => Some("working"),
        "blocked" => Some("attention"),
        "idle" | "done" => Some("idle"),
        _ => None,
    }
}

/// The worktree status the home list shows, from the panes running the conversation: the busiest
/// state wins, and no pane means `inactive`.
pub fn worktree_status(panes: &[&Running]) -> &'static str {
    let has = |status: &str| panes.iter().any(|r| r.pane.agent_status == status);
    if has("working") {
        "working"
    } else if has("blocked") {
        "permission"
    } else if has("done") {
        "done"
    } else if panes.is_empty() {
        "inactive"
    } else {
        "active"
    }
}

/// One `RuntimeWorktreeAgentRow` for a pane running the conversation. The prompt and the last
/// message are not read from herdr, so they are sent empty.
pub fn agent_row(r: &Running, conv: &Conversation, now_ms: f64) -> Value {
    json!({
        "paneKey": r.pane.terminal_id,
        "parentPaneKey": Value::Null,
        "state": agent_state(&r.pane.agent_status),
        "agentType": r.pane.agent.clone().unwrap_or_else(|| conv.harness.clone()),
        "prompt": "",
        "taskTitle": Value::Null,
        "displayName": label(conv),
        "lastAssistantMessage": Value::Null,
        "toolName": Value::Null,
        "toolInput": Value::Null,
        "interrupted": false,
        "stateStartedAt": now_ms,
        "updatedAt": now_ms,
    })
}

/// One `AgentSessionStatusSummary` for a pane running the conversation. The session is the pane.
pub fn agent_session(r: &Running, conv: &Conversation, now_ms: f64) -> Value {
    json!({
        "sessionId": r.pane.terminal_id,
        "workspaceId": conv.worktree_id(),
        "agent": r.pane.agent.clone().unwrap_or_else(|| conv.harness.clone()),
        "status": session_status(&r.pane.agent_status),
        "latestPrompt": "",
        "updatedAt": now_ms,
        "conversationName": conv.title,
    })
}

/// The id a `session.tabs` or `worktree` selector names: the app sends `id:<worktree id>`.
pub fn worktree_from_selector(selector: &str) -> &str {
    selector.strip_prefix("id:").unwrap_or(selector)
}

/// Milliseconds since the epoch, as the phone's timestamps are.
pub fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64)
}

/// Identifies this bridge process's snapshots. A restart changes it, so the phone never compares a
/// version from one run with a version from another.
static EPOCH: LazyLock<String> = LazyLock::new(|| format!("bridge-{}", now_ms() as u64));

pub fn publication_epoch() -> &'static str {
    EPOCH.as_str()
}

static VERSION: AtomicU64 = AtomicU64::new(0);

/// The next snapshot version: increasing within this process.
pub fn next_version() -> u64 {
    VERSION.fetch_add(1, Ordering::Relaxed) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(harness: &str, title: &str, ident: &str, dir: &str) -> Conversation {
        Conversation {
            harness: harness.into(),
            key: format!("/s/{title}"),
            title: title.into(),
            dir: dir.into(),
            updated: 1.0,
            idents: vec![ident.into()],
        }
    }

    fn pane(id: &str, term: &str, status: &str, focused: bool, agent: Option<&str>, argv: &[&str]) -> Running {
        Running {
            pane: Pane {
                pane_id: id.into(),
                terminal_id: term.into(),
                tab_id: "w1:t1".into(),
                focused,
                agent_status: status.into(),
                agent: agent.map(str::to_string),
                title: None,
                rows: Some(40),
            },
            argvs: vec![argv.iter().map(|s| s.to_string()).collect()],
            prompt: None,
            owner: None,
        }
    }

    #[test]
    fn a_pane_runs_a_conversation_only_when_an_argument_carries_its_session_id() {
        let c = conv("claude", "fix login", "uuid-1", "/work/a");
        let running = pane("w1:p1", "term_a", "working", true, Some("claude"), &["claude", "--resume", "uuid-1"]);
        let bare = pane("w1:p2", "term_b", "idle", false, None, &["claude"]);
        let mentions = pane("w1:p3", "term_c", "idle", false, None, &["cat", "uuid-1"]);
        let all = vec![running.clone(), bare, mentions];
        let found = panes_of(&all, &c);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pane.terminal_id, "term_a");
    }

    #[test]
    fn a_pane_goes_to_the_first_conversation_that_claims_it() {
        let newer = conv("omp", "newer", "uuid-1", "/w");
        let older = conv("omp", "older", "uuid-1", "/w");
        let r = pane("w1:p1", "term_a", "idle", false, None, &["omp", "--resume", "uuid-1"]);
        let list = vec![newer, older];
        assert_eq!(conversation_of(&r, &list).unwrap().title, "newer");
    }

    #[test]
    fn tabs_and_terminals_are_panes_with_the_focused_one_active() {
        let c = conv("omp", "fix login", "uuid-1", "/work/a");
        let a = pane("w1:p1", "term_a", "idle", false, None, &["omp", "--resume", "uuid-1"]);
        let b = pane("w1:p2", "term_b", "working", true, Some("omp"), &["omp", "--resume", "uuid-1"]);
        let panes = vec![&a, &b];
        let result = tabs_result("sesync-x::conv-y", &c, &panes, "bridge-1", 7);
        assert_eq!(result["worktree"], "sesync-x::conv-y");
        assert_eq!(result["snapshotVersion"], 7);
        assert_eq!(result["activeTabId"], "term_b");
        assert_eq!(result["activeTabType"], "terminal");
        assert_eq!(result["tabs"][0]["id"], "term_a");
        assert_eq!(result["tabs"][0]["terminal"], "term_a");
        assert_eq!(result["tabs"][0]["title"], "omp: fix login");
        assert_eq!(result["tabs"][1]["isActive"], true);

        let row = terminal_row(&b, &c);
        assert_eq!(row["handle"], "term_b");
        assert_eq!(row["agentIdentity"], "omp");
        assert_eq!(row["worktreeId"], c.worktree_id().as_str());
        assert!(terminal_row(&a, &c).get("agentIdentity").is_none(), "no agent, no identity");
    }

    #[test]
    fn no_pane_means_an_empty_tab_list_and_no_active_tab() {
        let c = conv("omp", "t", "uuid-1", "/w");
        let result = tabs_result("w", &c, &[], "e", 1);
        assert_eq!(result["tabs"], json!([]));
        assert_eq!(result["activeTabId"], Value::Null);
        assert_eq!(result["activeTabType"], Value::Null);
    }

    #[test]
    fn herdr_statuses_map_to_the_phone_vocabularies() {
        assert_eq!(agent_state("working"), "working");
        assert_eq!(agent_state("blocked"), "blocked");
        assert_eq!(agent_state("done"), "done");
        assert_eq!(agent_state("idle"), "waiting");
        assert_eq!(agent_state("unknown"), "waiting");
        assert_eq!(session_status("working"), Some("working"));
        assert_eq!(session_status("blocked"), Some("attention"));
        assert_eq!(session_status("done"), Some("idle"));
        assert_eq!(session_status("idle"), Some("idle"));
        assert_eq!(session_status("unknown"), None);
    }

    #[test]
    fn the_worktree_status_is_the_busiest_pane_state() {
        let working = pane("w1:p1", "t1", "working", false, None, &["x"]);
        let blocked = pane("w1:p2", "t2", "blocked", false, None, &["x"]);
        let done = pane("w1:p3", "t3", "done", false, None, &["x"]);
        let idle = pane("w1:p4", "t4", "idle", false, None, &["x"]);
        assert_eq!(worktree_status(&[]), "inactive");
        assert_eq!(worktree_status(&[&idle]), "active");
        assert_eq!(worktree_status(&[&idle, &done]), "done");
        assert_eq!(worktree_status(&[&done, &blocked]), "permission");
        assert_eq!(worktree_status(&[&blocked, &working]), "working");
    }

    #[test]
    fn the_selector_prefix_is_removed_once() {
        assert_eq!(worktree_from_selector("id:sesync-a::conv-b"), "sesync-a::conv-b");
        assert_eq!(worktree_from_selector("sesync-a::conv-b"), "sesync-a::conv-b");
    }

    #[test]
    fn a_session_summary_names_its_pane_and_conversation() {
        let c = conv("claude", "t", "uuid-1", "/w");
        let r = pane("w1:p1", "term_a", "blocked", false, Some("claude"), &["claude", "--resume", "uuid-1"]);
        let s = agent_session(&r, &c, 5.0);
        assert_eq!(s["sessionId"], "term_a");
        assert_eq!(s["workspaceId"], c.worktree_id().as_str());
        assert_eq!(s["agent"], "claude");
        assert_eq!(s["status"], "attention");
        assert_eq!(s["updatedAt"], 5.0);
        let row = agent_row(&r, &c, 5.0);
        assert_eq!(row["paneKey"], "term_a");
        assert_eq!(row["state"], "blocked");
    }
}
