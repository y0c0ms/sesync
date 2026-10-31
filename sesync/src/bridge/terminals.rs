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
use crate::session_cols::display_safe;
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
/// `owner` is the worktree id the bridge opened the pane for (see `remember_pane`). `fallback` is the
/// worktree id the pane runs by the tag and title rules, set only when no argument list or owner
/// places the pane (see `assign_fallbacks`). `reported` is the session the pane's agent reported to herdr, kept only
/// when no foreground argument carries any conversation's session id: the pane's identity then (see `runs_directly`).
#[derive(Debug, Clone, PartialEq)]
pub struct Running {
    pub pane: Pane,
    pub argvs: Vec<Vec<String>>,
    pub prompt: Option<String>,
    pub owner: Option<String>,
    pub fallback: Option<String>,
    pub reported: Option<String>,
}

/// Panes the bridge opened, by herdr server and terminal id, with the worktree id they were opened for. A pane
/// opened for a project needs no argv to place it; this does. Entries of panes a server no longer lists are dropped
/// on that server's next listing, so one server's listing never drops another's records.
static OPENED: LazyLock<Mutex<HashMap<(String, String), String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Record that the pane `terminal_id` belongs to the worktree `worktree_id`. Only a project's fresh tab is
/// recorded here, under its project id, which no conversation has. A conversation's pane is never recorded:
/// it is listed while its foreground argv carries the session's id (or its tag names it), so it stops being
/// the conversation when its agent exits, and a terminal id that herdr reuses after a restart belongs to no
/// conversation. A record would outlive the agent and send the phone's prompts into a bare shell.
pub fn remember_pane(herdr: &dyn Herdr, terminal_id: &str, worktree_id: &str) {
    OPENED.lock().insert((herdr.scope(), terminal_id.to_string()), worktree_id.to_string());
}

/// Whether a tab label carries a session's tag (`herdr_open::tab_label`): a tab "open in herdr" opened for a
/// conversation. Such a tab stays the conversation's tab after its agent quits and its shell is left.
pub fn is_session_tab(label: &str) -> bool {
    let tag: String = label.chars().rev().take(8).collect::<String>().chars().rev().collect();
    tag.len() == 8 && tag.chars().all(|c| c.is_ascii_hexdigit()) && label.ends_with(&herdr_open::tag_suffix(&tag))
}

/// The worktree id a pane was opened for, when the bridge opened it.
pub fn owner_of(herdr: &dyn Herdr, terminal_id: &str) -> Option<String> {
    OPENED.lock().get(&(herdr.scope(), terminal_id.to_string())).cloned()
}

/// Every pane herdr has, with its foreground processes and the conversation it runs by the fallback
/// rules. Fails closed: a pane whose processes cannot be read fails the listing, so no conversation is
/// reported idle while one of its panes is unknown. `conversations` is what the fallback rules match.
pub fn running_panes(h: &dyn Herdr, conversations: &[Conversation]) -> Result<Vec<Running>, String> {
    let panes = herdr::list_panes(h)?;
    running_listed(h, panes, conversations).map(|(running, _labels)| running)
}

/// `running_panes` for a pane list already read. Also returns the tab labels it read (by tab id), so the
/// watcher can place a pane again when its title changes (`reassign_fallbacks`) without another read.
pub fn running_listed(
    h: &dyn Herdr,
    panes: Vec<herdr::Pane>,
    conversations: &[Conversation],
) -> Result<(Vec<Running>, HashMap<String, String>), String> {
    let scope = h.scope();
    let owners: HashMap<String, String> = {
        let mut opened = OPENED.lock();
        opened.retain(|(server, terminal), _| *server != scope || panes.iter().any(|p| p.terminal_id == *terminal));
        opened
            .iter()
            .filter(|((server, _), _)| *server == scope)
            .map(|((_, terminal), worktree)| (terminal.clone(), worktree.clone()))
            .collect()
    };
    // The session each agent reported to herdr: places a pane whose agent moved its session to a new file on every
    // platform (`herdr::with_reported_session`). Best effort, and read only when some pane runs an agent.
    let agents = agent_key(&panes);
    let reported = if panes.iter().any(|p| p.agent.is_some()) {
        match h.process_memo() {
            Some(memo) => memo.sessions(&agents, || herdr::agent_sessions(h)),
            None => herdr::agent_sessions(h),
        }
        .unwrap_or_default()
    } else {
        HashMap::new()
    };
    let mut running: Vec<Running> = panes
        .into_iter()
        .map(|pane| {
            let read = || herdr::foreground_argvs(h, &pane.pane_id);
            let argvs = match h.process_memo() {
                Some(memo) => memo.argvs(&pane, read),
                None => read(),
            }
            .map_err(|e| format!("could not read the processes of pane {}: {e}", pane.pane_id))?;
            let reported_session = reported.get(&pane.pane_id).map(String::as_str);
            let argvs = herdr::with_reported_session(argvs, reported_session);
            let named = conversations.iter().any(|c| c.idents.iter().any(|i| herdr_open::argv_carries(&argvs, i)));
            let reported = if named { None } else { reported_session.map(str::to_string) };
            let prompt = blocked_prompt(h, &pane);
            let owner = owners.get(&pane.terminal_id).cloned();
            Ok(Running { pane, argvs, prompt, owner, fallback: None, reported })
        })
        .collect::<Result<_, String>>()?;
    // Tab labels cost one herdr call, so they are read only when some pane runs an agent.
    let labels = if running.iter().any(|r| r.pane.agent.is_some()) {
        match h.process_memo() {
            Some(memo) => memo.labels(&agents, || herdr::tab_labels(h)),
            None => herdr::tab_labels(h),
        }
        .unwrap_or_default()
    } else {
        HashMap::new()
    };
    if let Some(memo) = h.process_memo() {
        let ids: Vec<String> = running.iter().map(|r| r.pane.pane_id.clone()).collect();
        memo.retain(&ids);
    }
    assign_fallbacks(&mut running, conversations, &labels);
    Ok((running, labels))
}

/// What the agent reads of a listing depend on: the panes that run an agent, and the tab and state each is in.
fn agent_key(panes: &[herdr::Pane]) -> String {
    let mut parts: Vec<String> = panes
        .iter()
        .filter(|p| p.agent.is_some())
        .map(|p| format!("{}|{}|{:?}|{}", p.pane_id, p.tab_id, p.agent, p.agent_status))
        .collect();
    parts.sort();
    parts.join(";")
}

/// Give each pane its fallback conversation again, after a pane's title or agent changed: the same rules as
/// `running_panes`, over the argument lists and tab labels already read.
pub fn reassign_fallbacks(panes: &mut [Running], conversations: &[Conversation], labels: &HashMap<String, String>) {
    assign_fallbacks(panes, conversations, labels);
}

/// Give a fallback conversation to each agent pane that no argument list or owner places. The rules, in
/// order: the Open-in-herdr tag in the pane's tab label names the conversation; else the pane's agent and
/// the title its terminal shows (`pane_title_name`) name one conversation, or, when several share that
/// title, exactly one of them lives in the pane's foreground directory. Never ambiguous: a conversation
/// that a pane already runs directly takes no fallback, and one that two panes both fit goes to neither.
fn assign_fallbacks(panes: &mut [Running], conversations: &[Conversation], labels: &HashMap<String, String>) {
    let ids: Vec<String> = conversations.iter().map(Conversation::worktree_id).collect();
    let direct = |r: &Running, i: usize| runs_directly(r, &conversations[i], &ids[i]);
    let carried: Vec<bool> = (0..conversations.len()).map(|i| panes.iter().any(|r| direct(r, i))).collect();
    let claims: Vec<Option<usize>> = panes
        .iter()
        .map(|r| {
            if r.pane.agent.is_none() || (0..conversations.len()).any(|i| direct(r, i)) {
                return None;
            }
            fallback_conversation(r, conversations, labels).filter(|&i| !carried[i])
        })
        .collect();
    let mut claimants = vec![0usize; conversations.len()];
    for i in claims.iter().flatten() {
        claimants[*i] += 1;
    }
    for (r, claim) in panes.iter_mut().zip(claims) {
        r.fallback = claim.filter(|&i| claimants[i] == 1).map(|i| ids[i].clone());
    }
}

/// The one conversation a pane's tag or title names, when exactly one fits.
fn fallback_conversation(r: &Running, conversations: &[Conversation], labels: &HashMap<String, String>) -> Option<usize> {
    let agent = r.pane.agent.as_deref()?;
    if let Some(label) = labels.get(&r.pane.tab_id) {
        let tagged: Vec<usize> = conversations
            .iter()
            .enumerate()
            .filter(|(_, c)| label.ends_with(&herdr_open::tag_suffix(&herdr_open::session_tag(&c.key))))
            .map(|(i, _)| i)
            .collect();
        match tagged.as_slice() {
            [only] => return Some(*only),
            [] => {}
            _ => return None,
        }
    }
    let name = pane_title_name(agent, r.pane.title.as_deref()?)?;
    let same: Vec<usize> = conversations
        .iter()
        .enumerate()
        .filter(|(_, c)| c.harness == agent && display_safe(&c.title) == name)
        .map(|(i, _)| i)
        .collect();
    match same.as_slice() {
        [only] => Some(*only),
        [] => None,
        several => {
            let dir = r.pane.foreground_cwd.as_deref()?;
            let mut in_dir = several.iter().filter(|&&i| same_dir(&conversations[i].dir, dir));
            match (in_dir.next(), in_dir.next()) {
                (Some(&i), None) => Some(i),
                _ => None,
            }
        }
    }
}

fn same_dir(a: &str, b: &str) -> bool {
    !a.is_empty() && a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// The conversation title a pane's terminal title shows, normalised as tab labels are (`display_safe`),
/// for the agents that title their terminal with it. omp writes `π`, one status glyph (`>` idle, a spinner while
/// it works, `!` while a question or approval waits on the user), a space, then the title; Claude Code writes a
/// glyph (`✳`, or a spinner while it works), then the title; OpenCode writes `OC | ` then the title. None for any
/// other agent, or a title that does not have that shape.
///
/// The omp glyph is any one non-alphanumeric character: matching only `>` and spinners lost the pane while omp
/// waited on a question (`π ! Main agent`), the phone showed the conversation as not running, and its Resume
/// started a second omp on the same session (2026-10-29).
pub fn pane_title_name(agent: &str, title: &str) -> Option<String> {
    let rest = match agent {
        "omp" => {
            let rest = title.strip_prefix('π')?.trim_start();
            let first = rest.chars().next()?;
            let after = &rest[first.len_utf8()..];
            if first.is_alphanumeric() || !after.starts_with(char::is_whitespace) {
                return None;
            }
            after
        }
        "claude" => title.trim_start_matches(|c: char| c.is_whitespace() || is_claude_glyph(c)),
        "opencode" => title.strip_prefix("OC | ")?,
        _ => return None,
    };
    let name = display_safe(rest);
    (!name.is_empty()).then_some(name)
}

/// A Braille pattern: the spinner glyphs terminal titles animate with.
fn is_spinner(c: char) -> bool {
    ('\u{2800}'..='\u{28FF}').contains(&c)
}

/// The glyphs Claude Code puts before a title: its idle mark and working spinner (Dingbats and the middle dot).
fn is_claude_glyph(c: char) -> bool {
    c == '·' || ('\u{2700}'..='\u{27BF}').contains(&c) || is_spinner(c)
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

/// Whether `r` runs `conv`: directly (see `runs_directly`), or by the fallback rules.
fn runs(r: &Running, conv: &Conversation) -> bool {
    let worktree = conv.worktree_id();
    runs_directly(r, conv, &worktree) || r.fallback.as_deref() == Some(worktree.as_str())
}

/// Whether `r` runs `conv` by what herdr reports: the bridge opened the pane for `worktree`, a foreground argument
/// carries one of `conv`'s session ids, or, when no argument names a session, the agent reported one of them to herdr.
fn runs_directly(r: &Running, conv: &Conversation, worktree: &str) -> bool {
    r.owner.as_deref() == Some(worktree)
        || conv.idents.iter().any(|ident| {
            herdr_open::argv_carries(&r.argvs, ident) || r.reported.as_deref().is_some_and(|s| herdr::same_session(s, ident))
        })
}

/// The label a terminal shows: the harness and the conversation's title, as "open in herdr" names a tab.
pub fn label(conv: &Conversation) -> String {
    herdr_open::tab_label(&conv.harness, &conv.title, "")
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
                foreground_cwd: None,
                rows: Some(40),
            },
            argvs: vec![argv.iter().map(|s| s.to_string()).collect()],
            prompt: None,
            owner: None,
            fallback: None,
            reported: None,
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

    /// A pane running `agent` with the given terminal title and foreground directory.
    fn titled(id: &str, term: &str, agent: &str, title: &str, cwd: &str, argv: &[&str]) -> Running {
        let mut r = pane(id, term, "idle", false, Some(agent), argv);
        r.pane.title = Some(title.into());
        r.pane.foreground_cwd = Some(cwd.into());
        r
    }

    fn ids(panes: &[&Running]) -> Vec<String> {
        panes.iter().map(|r| r.pane.terminal_id.clone()).collect()
    }

    #[test]
    fn a_picker_started_omp_pane_runs_the_conversation_its_title_and_directory_name() {
        let main = conv("omp", "Main agent", "uuid-1", "/home/u/SkillsOutlie");
        let mut panes = vec![titled("w3:p1", "term_p1", "omp", "π ⠴ Main agent", "/home/u/SkillsOutlie", &["omp", "--resume"])];
        assign_fallbacks(&mut panes, std::slice::from_ref(&main), &HashMap::new());
        assert_eq!(ids(&panes_of(&panes, &main)), ["term_p1"]);
    }

    #[test]
    fn an_omp_pane_keeps_its_conversation_whatever_status_glyph_its_title_shows() {
        for glyph in [">", "⠴", "!", "?", "✓"] {
            assert_eq!(pane_title_name("omp", &format!("π {glyph} Main agent")).as_deref(), Some("Main agent"), "{glyph}");
        }
        // Not omp's shape: no glyph, or a glyph glued to the title.
        assert_eq!(pane_title_name("omp", "π Main agent"), None);
        assert_eq!(pane_title_name("omp", "π !Main agent"), None);
        assert_eq!(pane_title_name("omp", "Main agent"), None);
    }

    #[test]
    fn an_omp_pane_waiting_on_a_question_still_runs_its_conversation() {
        let main = conv("omp", "Main agent", "uuid-1", "/home/u/SkillsOutlie");
        let mut panes = vec![titled("w2:p1", "term_p1", "omp", "π ! Main agent", "/home/u/SkillsOutlie", &["omp", "--resume"])];
        assign_fallbacks(&mut panes, std::slice::from_ref(&main), &HashMap::new());
        assert_eq!(ids(&panes_of(&panes, &main)), ["term_p1"]);
    }

    #[test]
    fn two_conversations_with_one_title_are_told_apart_by_the_pane_directory() {
        let here = conv("omp", "Main agent", "uuid-1", "/work/alpha");
        let there = conv("omp", "Main agent", "uuid-2", "/work/beta");
        let convs = vec![here.clone(), there.clone()];
        let mut panes = vec![titled("w1:p1", "term_a", "omp", "π > Main agent", "/work/beta/", &["omp", "--resume"])];
        assign_fallbacks(&mut panes, &convs, &HashMap::new());
        assert_eq!(ids(&panes_of(&panes, &there)), ["term_a"]);
        assert!(panes_of(&panes, &here).is_empty());
    }

    #[test]
    fn a_title_several_conversations_share_matches_none_unless_the_directory_settles_it() {
        let a = conv("omp", "Main agent", "uuid-1", "/work/alpha");
        let b = conv("omp", "Main agent", "uuid-2", "/work/beta");
        let convs = vec![a.clone(), b.clone()];
        let elsewhere = titled("w1:p1", "term_a", "omp", "π > Main agent", "/work/gamma", &["omp"]);
        let mut unknown_dir = titled("w1:p2", "term_b", "omp", "π > Main agent", "/work/alpha", &["omp"]);
        unknown_dir.pane.foreground_cwd = None;
        for pane in [elsewhere, unknown_dir] {
            let mut panes = vec![pane];
            assign_fallbacks(&mut panes, &convs, &HashMap::new());
            assert!(panes_of(&panes, &a).is_empty() && panes_of(&panes, &b).is_empty(), "no match: ambiguous");
        }
    }

    #[test]
    fn two_panes_that_fit_one_conversation_both_go_to_neither() {
        let main = conv("omp", "Main agent", "uuid-1", "/work/alpha");
        let mut panes = vec![
            titled("w1:p1", "term_a", "omp", "π > Main agent", "/work/alpha", &["omp"]),
            titled("w1:p2", "term_b", "omp", "π ⠴ Main agent", "/work/alpha", &["omp"]),
        ];
        assign_fallbacks(&mut panes, std::slice::from_ref(&main), &HashMap::new());
        assert!(panes_of(&panes, &main).is_empty());
    }

    #[test]
    fn a_conversation_a_pane_runs_directly_takes_no_title_fallback() {
        let main = conv("omp", "Main agent", "uuid-1", "/work/alpha");
        let mut panes = vec![
            pane("w1:p1", "term_a", "working", true, Some("omp"), &["omp", "--resume", "uuid-1"]),
            titled("w1:p2", "term_b", "omp", "π > Main agent", "/work/alpha", &["omp", "--resume"]),
        ];
        assign_fallbacks(&mut panes, std::slice::from_ref(&main), &HashMap::new());
        assert_eq!(ids(&panes_of(&panes, &main)), ["term_a"]);
    }

    #[test]
    fn the_open_in_herdr_tag_in_the_tab_label_names_the_conversation_before_any_title() {
        let a = conv("omp", "Main agent", "uuid-1", "/work/alpha");
        let b = Conversation { key: "/s/beta".into(), ..conv("omp", "Main agent", "uuid-2", "/work/alpha") };
        let convs = vec![a.clone(), b.clone()];
        let mut panes = vec![titled("w1:p1", "term_a", "omp", "π > Main agent", "/work/alpha", &["omp", "--resume"])];
        let tag = crate::herdr_open::session_tag(&b.key);
        let labels = HashMap::from([("w1:t1".to_string(), format!("omp: Main agent · {tag}"))]);
        assign_fallbacks(&mut panes, &convs, &labels);
        assert!(panes_of(&panes, &a).is_empty());
        assert_eq!(ids(&panes_of(&panes, &b)), ["term_a"]);
    }

    #[test]
    fn claude_and_opencode_panes_match_their_own_title_shape_and_harness_only() {
        let claude = conv("claude", "TestFromOPENCODE", "uuid-c", "/work/c");
        let opencode = conv("opencode", "DoppleClankerMain", "ses_o", "/work/o");
        let convs = vec![claude.clone(), opencode.clone()];
        let mut panes = vec![
            titled("w3:pG", "term_g", "claude", "✳ TestFromOPENCODE", "/work/c", &["claude"]),
            titled("w3:pF", "term_f", "opencode", "OC | DoppleClankerMain", "/work/o", &["opencode"]),
            titled("w3:pX", "term_x", "claude", "OC | DoppleClankerMain", "/work/o", &["claude"]),
        ];
        assign_fallbacks(&mut panes, &convs, &HashMap::new());
        assert_eq!(ids(&panes_of(&panes, &claude)), ["term_g"]);
        assert_eq!(ids(&panes_of(&panes, &opencode)), ["term_f"]);
    }

    #[test]
    fn a_pane_must_run_the_harness_its_conversation_has() {
        let main = conv("omp", "Main agent", "uuid-1", "/work/alpha");
        let mut panes = vec![titled("w1:p1", "term_a", "claude", "✳ Main agent", "/work/alpha", &["claude"])];
        assign_fallbacks(&mut panes, std::slice::from_ref(&main), &HashMap::new());
        assert!(panes_of(&panes, &main).is_empty());
    }

    #[test]
    fn an_emoji_in_a_title_matches_on_both_sides_as_tab_labels_show_it() {
        let mod_session = conv("omp", "📚 MOD: Deploy ModularStudy locally", "uuid-m", "/work/mod");
        let mut panes = vec![titled("w3:pH", "term_h", "omp", "π > 📚 MOD: Deploy ModularStudy locally", "/work/mod", &["omp"])];
        assign_fallbacks(&mut panes, std::slice::from_ref(&mod_session), &HashMap::new());
        assert_eq!(ids(&panes_of(&panes, &mod_session)), ["term_h"]);
    }

    #[test]
    fn a_terminal_title_names_its_conversation_by_each_harness_shape() {
        assert_eq!(pane_title_name("omp", "π > Main agent").as_deref(), Some("Main agent"));
        assert_eq!(pane_title_name("omp", "π ⠴ Main agent").as_deref(), Some("Main agent"));
        assert_eq!(pane_title_name("omp", "yocoms@yocomsMain:~/projects"), None);
        assert_eq!(pane_title_name("claude", "✳ TestFromOPENCODE").as_deref(), Some("TestFromOPENCODE"));
        assert_eq!(pane_title_name("opencode", "OC | DoppleClankerMain").as_deref(), Some("DoppleClankerMain"));
        assert_eq!(pane_title_name("opencode", "DoppleClankerMain"), None);
        assert_eq!(pane_title_name("pi", "π > Main agent"), None);
    }

    /// herdr answering a pane listing, each pane's foreground argv, and the sessions agents reported.
    struct Reports {
        panes: Vec<(&'static str, Vec<&'static str>)>,
        sessions: Vec<(&'static str, &'static str)>,
    }

    impl Herdr for Reports {
        fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            match method {
                "pane.list" => Ok(json!({ "panes": self.panes.iter().map(|(id, _)| json!({
                    "pane_id": id, "terminal_id": format!("term_{id}"), "tab_id": "w1:t1", "focused": false,
                    "agent_status": "idle", "agent": "omp",
                })).collect::<Vec<_>>() })),
                "pane.process_info" => {
                    let (_, argv) = self.panes.iter().find(|(id, _)| params["pane_id"] == *id).ok_or("no pane")?;
                    Ok(json!({ "process_info": { "foreground_processes": [{ "argv": argv }] } }))
                }
                "agent.list" => Ok(json!({ "agents": self.sessions.iter().map(|(pane, value)| json!({
                    "pane_id": pane, "agent": "omp", "agent_session": { "kind": "path", "value": value },
                })).collect::<Vec<_>>() })),
                "tab.list" => Ok(json!({ "tabs": [] })),
                other => Err(format!("no answer for {other}")),
            }
        }
    }

    #[test]
    fn a_pane_whose_command_names_no_session_runs_the_session_its_agent_reported() {
        let main = conv("omp", "OTmain", "/s/-a/main.jsonl", "/a");
        let other = conv("omp", "Other", "/s/-b/other.jsonl", "/b");
        let herdr = Reports {
            panes: vec![
                // Started from omp's picker: the command line names no session (work PC, 2026-10-30).
                ("w6:p3", vec!["omp", "--resume"]),
                // Its command line names `other`; a report naming `main` (another directory, so not a move) does not
                // make it a second `main` pane.
                ("w7:p1", vec!["omp", "--session", "/s/-b/other.jsonl"]),
                ("w8:p1", vec!["omp", "--resume"]),
            ],
            sessions: vec![("w6:p3", "/s/-a/main.jsonl"), ("w7:p1", "/s/-a/main.jsonl"), ("w8:p1", "/s/-a/gone.jsonl")],
        };
        let convs = vec![main, other];
        let panes = running_panes(&herdr, &convs).unwrap();
        let runs = |id: &str| panes.iter().find(|r| r.pane.pane_id == id).and_then(|r| conversation_of(r, &convs)).map(|c| c.title.clone());
        assert_eq!(runs("w6:p3").as_deref(), Some("OTmain"));
        assert_eq!(runs("w7:p1").as_deref(), Some("Other"), "the command line wins over the report");
        assert_eq!(runs("w8:p1"), None, "a reported session no conversation has places nothing");
    }
}
