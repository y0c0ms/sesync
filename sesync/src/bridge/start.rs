//! Starting agent sessions from the phone. Three answers, all from this machine:
//!   * `preflight.detectAgents`: the app's agent ids whose executable is on `PATH` now, as a bare array;
//!   * `settings.get`: the settings object the new-tab sheet reads (this host keeps none);
//!   * `session.tabs.createTerminal`: a herdr tab for the worktree, in one of two ways.
//!     - A conversation's worktree is opened the way "Open in herdr" opens it (`herdr_open::open`): a pane
//!       already running the session is focused and reused; otherwise a new tab runs the harness's resume
//!       command for the session, so the conversation's own journal grows. The agent named must be the
//!       conversation's harness or absent; any other agent is refused before anything opens. Never a fresh
//!       session. The reply is the pane herdr lists as running the conversation, once it lists one.
//!     - A project's worktree (no conversation) gets a new tab in the project's directory with the chosen
//!       agent started fresh through herdr's `agent.start`, or a plain shell. Never a resume: no session id
//!       reaches the agent's argv, and the pane is recorded against the project, never against a conversation.
//!
//! herdr's socket calls (`tab.create`, `agent.start`, `tab.close`, and the reads a resume waits on) go through
//! `Ctx::herdr`. A resume also runs herdr's CLI, through `Launch::resume` (see [`Resume`]). The tab label is
//! `herdr_open::tab_label`, the same label "open in herdr" uses.

use super::catalog::{self, Conversation};
use super::herdr::Herdr;
use super::rpc::{Ctx, Failure};
use super::terminals;
use crate::harness::Locations;
use crate::herdr_open;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use parking_lot::{Mutex, MutexGuard};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// An agent this host can start. `id` is the app's `TuiAgent` id, `herdr_kind` is herdr's
/// `agent start --kind`, and `program` is the executable that must be on `PATH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentKind {
    pub id: &'static str,
    pub herdr_kind: &'static str,
    pub program: &'static str,
}

/// The agents this host starts. Each id matches sesync's harness id, which is also the app's `TuiAgent` id.
pub const AGENTS: &[AgentKind] = &[
    AgentKind { id: "claude", herdr_kind: "claude", program: "claude" },
    AgentKind { id: "omp", herdr_kind: "omp", program: "omp" },
    AgentKind { id: "opencode", herdr_kind: "opencode", program: "opencode" },
    AgentKind { id: "pi", herdr_kind: "pi", program: "pi" },
    AgentKind { id: "antigravity", herdr_kind: "agy", program: "agy" },
];

/// How many created tabs are remembered for replay by `clientMutationId`.
const REPLAY_LIMIT: usize = 64;

/// How long a resumed session may take to show as running in its tab before the create fails.
const RESUME_WAIT: Duration = Duration::from_secs(10);
/// How often a resumed session's tab is looked at while the create waits for it.
const RESUME_POLL: Duration = Duration::from_millis(200);

/// How this host starts sessions. Cheap to clone; the replay memory is shared between clones.
#[derive(Clone, Default)]
pub struct Launch {
    /// The herdr workspace new tabs open in. None leaves the choice to herdr.
    pub workspace_id: Option<String>,
    /// What resuming a conversation takes. None when this host cannot resume one: the create is refused.
    pub resume: Option<Arc<Resume>>,
    /// Replies to recent creates by `clientMutationId`, so a retried create returns the tab it made.
    replies: Arc<Mutex<VecDeque<(String, Value)>>>,
}

/// What resuming a conversation takes from this host: where each harness keeps its sessions and the home
/// directory (the inputs of `herdr_open::plan`), and the herdr CLI "Open in herdr" runs through.
pub struct Resume {
    pub locations: Locations,
    pub home: PathBuf,
    pub herdr: herdr_open::Herdr,
    /// How long a resumed session may take to show as running in its tab before the create fails.
    pub start_wait: Duration,
}

impl Resume {
    /// The resume set-up for the herdr server at `socket`, the one the bridge's API calls reach. The CLI runs
    /// `$HERDR_BIN_PATH` (else `herdr`) with every other herdr variable dropped, so it cannot reach another
    /// server. New tabs open in `workspace_id`.
    pub fn new(locations: Locations, home: PathBuf, socket: &Path, workspace_id: Option<String>) -> Resume {
        let bin = std::env::var("HERDR_BIN_PATH").ok().filter(|b| !b.trim().is_empty()).unwrap_or_else(|| "herdr".into());
        let env = vec![("HERDR_SOCKET_PATH".to_string(), socket.display().to_string())];
        let herdr = herdr_open::Herdr::new(bin, workspace_id, Box::new(herdr_open::Isolated { env }));
        Resume { locations, home, herdr, start_wait: RESUME_WAIT }
    }
}

impl Launch {
    /// Launch settings that resume conversations through `resume`.
    pub fn resuming(resume: Resume) -> Launch {
        Launch { resume: Some(Arc::new(resume)), ..Launch::default() }
    }

    /// This process's launch settings: new tabs open in the workspace `$HERDR_ACTIVE_WORKSPACE_ID` names, and
    /// conversations resume through the herdr server at `socket`.
    pub fn from_env(locations: Locations, home: PathBuf, socket: &Path) -> Launch {
        let workspace_id = std::env::var("HERDR_ACTIVE_WORKSPACE_ID").ok().filter(|v| !v.trim().is_empty());
        let resume = Resume::new(locations, home, socket, workspace_id.clone());
        Launch { workspace_id, ..Launch::resuming(resume) }
    }
}

/// `preflight.detectAgents`: a bare array of the app's agent ids whose executable is on `PATH` now.
pub fn detect_agents(_params: &Value, _ctx: &Ctx) -> Result<Value, Failure> {
    let path = path_var();
    Ok(json!(installed(|program| on_path(&path, program))))
}

/// The ids of the agents whose program `is_installed` reports, in the table's order.
pub fn installed(is_installed: impl Fn(&str) -> bool) -> Vec<&'static str> {
    AGENTS.iter().filter(|kind| is_installed(kind.program)).map(|kind| kind.id).collect()
}

/// `settings.get`: the settings object the new-tab sheet reads. This host keeps no agent settings.
pub fn settings_get(_params: &Value, _ctx: &Ctx) -> Result<Value, Failure> {
    Ok(settings_value())
}

fn settings_value() -> Value {
    json!({ "settings": {} })
}

/// `session.tabs.createTerminal`: a herdr tab for the worktree, as the module doc describes. A create whose
/// `clientMutationId` was already answered returns that answer again.
pub fn create_terminal_tab(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    create_terminal_tab_on(params, ctx, &path_var())
}

/// `create_terminal_tab`, with the agent programs looked up on `path` rather than this process's `PATH`.
fn create_terminal_tab_on(params: &Value, ctx: &Ctx, path: &OsStr) -> Result<Value, Failure> {
    refuse_unsupported(params)?;
    let agent = requested_agent(params)?;
    let selector = worktree_selector(params)?;
    let key = params["clientMutationId"].as_str().filter(|k| !k.is_empty());
    // Held for the whole create: two creates for one conversation (a double tap, two phones) run one after the
    // other, so the second finds the pane the first made instead of opening another.
    let mut replies = lock_replies(&ctx.launch);
    if let Some(key) = key {
        if let Some((_, reply)) = replies.iter().find(|(known, _)| known.as_str() == key) {
            return Ok(reply.clone());
        }
    }
    let conversations = ctx.catalog.conversations().map_err(|m| ("unavailable", m))?;
    let reply = match conversation_of_selector(&conversations, &selector) {
        Some(conv) => resume_conversation(params, ctx, &conversations, conv, agent, path)?,
        None => fresh_agent_tab(params, ctx, &conversations, &selector, agent, path)?,
    };
    if let Some(key) = key {
        remember(&mut replies, key, reply.clone());
    }
    Ok(reply)
}

/// A conversation's tab: open the conversation as "Open in herdr" does, and reply with the pane herdr lists
/// as running it.
fn resume_conversation(
    params: &Value,
    ctx: &Ctx,
    conversations: &[Conversation],
    conv: &Conversation,
    agent: Option<&AgentKind>,
    path: &OsStr,
) -> Result<Value, Failure> {
    if let Some(kind) = agent.filter(|kind| kind.id != conv.harness) {
        return Err((
            "bad_request",
            format!("this conversation is a {} session, and it can only be opened in {}, not {}", conv.harness, conv.harness, kind.id),
        ));
    }
    if cwd_param(params)?.is_some() {
        return Err(("bad_request", "a conversation opens in its own project directory, so cwd does not apply".to_string()));
    }
    let resume = ctx
        .launch
        .resume
        .as_deref()
        .ok_or_else(|| ("herdr_unavailable", "this host has no herdr to resume a session in".to_string()))?;
    let plan = herdr_open::plan(&resume.locations, &resume.home, &conv.harness, &conv.key, &conv.title)
        .map_err(|m| ("bad_request", m))?;
    let program = plan.argv.first().map_or("", String::as_str);
    if !on_path(path, program) {
        return Err(("bad_request", format!("{} is not installed on this machine", conv.harness)));
    }
    let shell = herdr_open::PaneShell::current(&resume.home);
    let command = herdr_open::shell_line(&plan.argv, shell).map_err(|m| ("bad_request", m))?;
    // A phone's resume never moves the desktop's focus.
    let opened = match herdr_open::open_with_focus(&resume.herdr, &plan, shell, herdr_open::Detect::platform(), false) {
        Ok(opened) => opened,
        // herdr opens a tab only in a workspace, and none is left once the last tab of the last one is closed: open a
        // workspace in the conversation's directory, and run the resume in its first pane.
        Err(m) if m.contains("no active workspace") => {
            let (tab_id, pane_id) = resume.herdr.create_workspace(&plan.cwd, &plan.label).map_err(|m| ("herdr_unavailable", m))?;
            resume
                .herdr
                .run_in_pane(&pane_id, &command)
                .map_err(|m| ("herdr_unavailable", format!("could not start the resume command in herdr: {m}")))?;
            herdr_open::Opened::Created { tab_id }
        }
        Err(m) => return Err(open_failure(m)),
    };
    let (tab_id, created) = match &opened {
        herdr_open::Opened::Created { tab_id } => (tab_id.as_str(), true),
        herdr_open::Opened::Focused { tab_id } | herdr_open::Opened::Resumed { tab_id } => (tab_id.as_str(), false),
    };
    // A tab herdr still shows for the conversation whose agent has quit: its shell is idle, so the resume command is
    // typed into it again rather than waiting for an agent that will not come.
    if matches!(opened, herdr_open::Opened::Focused { .. }) && !listed_in_tab(ctx, conversations, conv, tab_id)? {
        herdr_open::refuse_second_agent(&plan).map_err(|m| ("conversation_running_elsewhere", m))?;
        rerun_in_idle_shell(ctx, &resume.herdr, tab_id, &command)?;
    }
    match wait_until_listed(ctx, conversations, conv, tab_id, resume.start_wait)? {
        Some(terminal_id) => Ok(tab_reply(&Opened { terminal_id, title: terminals::label(conv) })),
        None => {
            if created {
                let _ = resume.herdr.close_tab(tab_id);
            }
            Err(("agent_not_started", format!("{} did not start the session in its herdr tab", conv.harness)))
        }
    }
}

/// Whether herdr lists a pane of `tab_id` as running `conv`.
fn listed_in_tab(ctx: &Ctx, conversations: &[Conversation], conv: &Conversation, tab_id: &str) -> Result<bool, Failure> {
    let panes = terminals::running_panes(ctx.herdr.as_ref(), conversations).map_err(|m| ("herdr_unavailable", m))?;
    Ok(terminals::panes_of(&panes, conv).iter().any(|r| r.pane.tab_id == tab_id))
}

/// Types `command` into the tab's idle shell: a pane with no foreground process. A tab whose pane runs something else
/// is left alone.
fn rerun_in_idle_shell(ctx: &Ctx, herdr: &herdr_open::Herdr, tab_id: &str, command: &str) -> Result<(), Failure> {
    let panes = super::herdr::list_panes(ctx.herdr.as_ref()).map_err(|m| ("herdr_unavailable", m))?;
    for pane in panes.iter().filter(|p| p.tab_id == tab_id) {
        let argvs = super::herdr::foreground_argvs(ctx.herdr.as_ref(), &pane.pane_id).map_err(|m| ("herdr_unavailable", m))?;
        if argvs.is_empty() {
            return herdr
                .run_in_pane(&pane.pane_id, command)
                .map_err(|m| ("herdr_unavailable", format!("could not start the resume command in herdr: {m}")));
        }
    }
    Ok(())
}

/// herdr's refusal to open a tab, phrased for the phone. herdr opens a tab only in a workspace, and once the last
/// tab of the last workspace is closed there is none.
fn open_failure(message: String) -> Failure {
    if message.contains("no active workspace") {
        ("herdr_unavailable", "herdr has no workspace to open the session in; open a workspace in herdr and try again".into())
    } else {
        ("herdr_unavailable", message)
    }
}

/// The terminal of the pane in `tab_id` that herdr lists as running `conv`, polled until one does or `wait`
/// passes. None when none does: the resumed command did not start the session.
fn wait_until_listed(
    ctx: &Ctx,
    conversations: &[Conversation],
    conv: &Conversation,
    tab_id: &str,
    wait: Duration,
) -> Result<Option<String>, Failure> {
    let deadline = Instant::now() + wait;
    loop {
        let panes = terminals::running_panes(ctx.herdr.as_ref(), conversations).map_err(|m| ("herdr_unavailable", m))?;
        let listed = terminals::panes_of(&panes, conv).into_iter().find(|r| r.pane.tab_id == tab_id);
        // herdr reports an agent's state once its TUI is up. Before that a prompt typed into the pane is lost, so
        // the reply waits for the state; past the wait, the listed pane is still the conversation's.
        if let Some(running) = listed.filter(|r| r.pane.agent_status != "unknown" || Instant::now() >= deadline) {
            return Ok(Some(running.pane.terminal_id.clone()));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(RESUME_POLL);
    }
}

/// A project's tab: a new tab in the project's directory, with `agent` started fresh (or a plain shell).
fn fresh_agent_tab(
    params: &Value,
    ctx: &Ctx,
    conversations: &[Conversation],
    selector: &str,
    agent: Option<&AgentKind>,
    path: &OsStr,
) -> Result<Value, Failure> {
    let project = project_dir_of(conversations, selector).ok_or_else(|| missing_worktree(selector))?;
    if project.is_empty() {
        return Err(("bad_request", format!("worktree {selector} records no project directory")));
    }
    let cwd = cwd_param(params)?.unwrap_or_else(|| PathBuf::from(project));
    check_dir(&cwd)?;
    if let Some(kind) = agent {
        if !on_path(path, kind.program) {
            return Err(("bad_request", format!("{} is not installed on this machine", kind.id)));
        }
    }
    let opened = open_terminal(ctx.herdr.as_ref(), &ctx.launch, &cwd, agent)?;
    // The selector is a project's id, which no conversation has, so no conversation lists this pane.
    terminals::remember_pane(ctx.herdr.as_ref(), &opened.terminal_id, selector);
    Ok(tab_reply(&opened))
}

/// The refusal for a worktree no local catalog lists. A conversation id this machine does not list lives on
/// another host (or was never imported): opening it as a fresh session would be wrong, so the create says so.
fn missing_worktree(selector: &str) -> Failure {
    if selector.contains("::conv-") {
        ("worktree_not_found", format!("conversation {selector} is not on this machine, so it cannot be opened here"))
    } else {
        ("worktree_not_found", format!("no worktree {selector}"))
    }
}

/// A terminal tab a create opened: herdr's terminal id (the tab's id and handle) and its title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub terminal_id: String,
    pub title: String,
}

/// Open a herdr tab in `cwd` and start `agent` in its root pane, or leave a shell when `agent` is None.
/// The agent starts fresh: no argument names a session to resume. A failure to start the agent closes
/// the tab this call made, so nothing is left running that the phone was not told about.
pub fn open_terminal(
    herdr: &dyn Herdr,
    launch: &Launch,
    cwd: &Path,
    agent: Option<&AgentKind>,
) -> Result<Opened, Failure> {
    let title = cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let label = herdr_open::tab_label(agent.map_or("terminal", |kind| kind.id), &title, "");
    let mut tab_params = json!({ "cwd": cwd.display().to_string(), "label": label, "focus": false });
    if let Some(workspace) = &launch.workspace_id {
        tab_params["workspace_id"] = json!(workspace);
    }
    let created = herdr.call("tab.create", tab_params).map_err(|m| ("herdr_unavailable", m))?;
    let tab_id = created["tab"]["tab_id"].as_str().map(str::to_string);
    let pane_id = created["root_pane"]["pane_id"].as_str().map(str::to_string);
    let terminal_id = created["root_pane"]["terminal_id"].as_str().map(str::to_string);
    let (Some(tab_id), Some(pane_id), Some(terminal_id)) = (tab_id.clone(), pane_id, terminal_id) else {
        if let Some(tab_id) = tab_id {
            let _ = herdr.call("tab.close", json!({ "tab_id": tab_id }));
        }
        return Err(("herdr_unavailable", "herdr's new tab has no tab, pane or terminal id".to_string()));
    };
    if let Some(kind) = agent {
        let started = herdr.call(
            "agent.start",
            json!({ "name": agent_name(kind), "kind": kind.herdr_kind, "pane_id": pane_id }),
        );
        if let Err(message) = started {
            let _ = herdr.call("tab.close", json!({ "tab_id": tab_id }));
            return Err(("agent_not_started", format!("could not start {} in herdr: {message}", kind.id)));
        }
    }
    Ok(Opened { terminal_id, title: label })
}

/// The `session.tabs.createTerminal` reply: the new terminal tab. Its id and handle are herdr's terminal
/// id, the same id `session.tabs.list` and `terminal.list` report for the pane.
pub fn tab_reply(opened: &Opened) -> Value {
    json!({
        "tab": {
            "type": "terminal",
            "id": opened.terminal_id,
            "title": opened.title,
            "terminal": opened.terminal_id,
            "leafId": opened.terminal_id,
            "status": "ready",
            "isActive": false,
        }
    })
}

/// A herdr agent name: the app's id plus a short suffix that differs per start. herdr accepts lowercase
/// letters, digits, `-` and `_`, starting with a letter, at most 32 characters.
fn agent_name(kind: &AgentKind) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tick = NEXT.fetch_add(1, Ordering::Relaxed) as u32;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    format!("{}-{:08x}", kind.id, nanos ^ tick.rotate_left(13))
}

/// Fields this bridge cannot honour. They are refused before anything opens, so the create fails cleanly.
fn refuse_unsupported(params: &Value) -> Result<(), Failure> {
    for field in ["command", "agentPrompt", "resumeProviderSession", "sourceChatFork"] {
        if !params[field].is_null() {
            return Err(("bad_request", format!("{field} is not supported by the phone bridge")));
        }
    }
    Ok(())
}

/// The agent a create names (`agent`, else `launchAgent`); None for a plain shell.
fn requested_agent(params: &Value) -> Result<Option<&'static AgentKind>, Failure> {
    let named = params["agent"].as_str().or_else(|| params["launchAgent"].as_str()).filter(|a| !a.is_empty());
    match named {
        None => Ok(None),
        Some(id) => AGENTS
            .iter()
            .find(|kind| kind.id == id)
            .map(Some)
            .ok_or_else(|| ("bad_request", format!("this host cannot start the agent '{id}'"))),
    }
}

/// The worktree id a create names, with the `id:` selector prefix removed.
fn worktree_selector(params: &Value) -> Result<String, Failure> {
    params["worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| terminals::worktree_from_selector(s).to_string())
        .ok_or_else(|| ("bad_request", "missing worktree selector".to_string()))
}

/// The conversation a worktree selector names: its worktree id, exactly. A repo id names a project instead.
pub fn conversation_of_selector<'a>(conversations: &'a [Conversation], selector: &str) -> Option<&'a Conversation> {
    conversations.iter().find(|c| c.worktree_id() == selector)
}

/// The directory of the worktree `selector` names: a conversation's worktree id, or the repo id that
/// `repo.list` reports for a directory. None when no conversation matches.
pub fn project_dir_of(conversations: &[Conversation], selector: &str) -> Option<String> {
    conversation_of_selector(conversations, selector)
        .map(|c| c.dir.clone())
        .or_else(|| conversations.iter().find(|c| catalog::repo_id(&c.dir) == selector).map(|c| c.dir.clone()))
}

/// The `cwd` override: an absolute path. Absent gives None.
fn cwd_param(params: &Value) -> Result<Option<PathBuf>, Failure> {
    match params["cwd"].as_str().filter(|c| !c.is_empty()) {
        None => Ok(None),
        Some(cwd) if Path::new(cwd).is_absolute() => Ok(Some(PathBuf::from(cwd))),
        Some(cwd) => Err(("bad_request", format!("cwd {cwd} is not an absolute path"))),
    }
}

/// The directory must exist and be readable by this process. Nothing is created.
fn check_dir(dir: &Path) -> Result<(), Failure> {
    match std::fs::read_dir(dir) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(("bad_request", format!("no permission to open {}", dir.display())))
        }
        Err(_) => Err(("bad_request", format!("no such directory {}", dir.display()))),
    }
}

fn path_var() -> OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

/// Whether `program` is an executable file in one of the directories of `path_var`.
fn on_path(path_var: &OsStr, program: &str) -> bool {
    std::env::split_paths(path_var).any(|dir| is_executable_file(&dir.join(program)))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn lock_replies(launch: &Launch) -> MutexGuard<'_, VecDeque<(String, Value)>> {
    launch.replies.lock()
}

fn remember(replies: &mut VecDeque<(String, Value)>, key: &str, reply: Value) {
    if replies.len() == REPLAY_LIMIT {
        replies.pop_front();
    }
    replies.push_back((key.to_string(), reply));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::testkit::{self, FakeHerdr};
    use std::sync::atomic::AtomicUsize;

    fn scratch_dir(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("p2-start-{tag}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn conversation(dir: &str) -> Conversation {
        Conversation {
            harness: "omp".into(),
            key: "/home/u/.omp/agent/sessions/s.jsonl".into(),
            title: "t".into(),
            dir: dir.into(),
            updated: 0.0,
            idents: Vec::new(),
        }
    }

    /// A conversation of `harness` whose parent session is `key`, in `dir`, resumed by `ident`.
    fn conversation_at(key: &Path, dir: &Path, harness: &str, ident: &str) -> Conversation {
        Conversation {
            harness: harness.into(),
            key: key.display().to_string(),
            title: "fix login".into(),
            dir: dir.display().to_string(),
            updated: 1.0,
            idents: vec![ident.into()],
        }
    }

    /// A project directory, and an omp session file whose first line names that directory, under a scratch
    /// directory: the conversation the tests open.
    fn omp_setup(tag: &str) -> (PathBuf, PathBuf, Conversation) {
        let root = scratch_dir(tag);
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let key = root.join("s.jsonl");
        std::fs::write(&key, format!("{}\n", json!({ "type": "session", "cwd": proj.display().to_string() }))).unwrap();
        let conv = conversation_at(&key, &proj, "omp", &key.display().to_string());
        (root, proj, conv)
    }

    /// A context that lists `conversations` and resumes through `herdr`, whose commands take `start_wait`.
    fn bridge_ctx(root: &Path, conversations: Vec<Conversation>, herdr: &Arc<FakeHerdr>, start_wait: Duration) -> Ctx {
        let launch = testkit::resume_launch(herdr, Locations::default(), root.join("home"), start_wait);
        testkit::ctx_on(conversations, herdr.clone(), launch, root)
    }

    fn create(ctx: &Ctx, path: &OsStr, params: Value) -> Result<Value, Failure> {
        create_terminal_tab_on(&params, ctx, path)
    }

    fn terminal_of(reply: &Value) -> String {
        reply["tab"]["id"].as_str().expect("the reply names a tab").to_string()
    }

    /// The terminals herdr lists as running `conv`, by the bridge's own rules (what `session.tabs.list` shows).
    fn listed_for(ctx: &Ctx, conv: &Conversation) -> Vec<String> {
        let panes = terminals::running_panes(ctx.herdr.as_ref(), std::slice::from_ref(conv)).unwrap();
        terminals::panes_of(&panes, conv).iter().map(|r| r.pane.terminal_id.clone()).collect()
    }

    fn resume_argv(conv: &Conversation) -> Vec<String> {
        vec!["omp".into(), "--resume".into(), conv.key.clone()]
    }

    #[test]
    fn opening_a_closed_conversation_resumes_it_instead_of_a_fresh_session() {
        let (root, proj, conv) = omp_setup("closed");
        let herdr = FakeHerdr::new();
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(1));
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let reply = create(&ctx, &path, json!({ "worktree": conv.worktree_id(), "agent": "omp", "clientMutationId": "m1" })).unwrap();
        let terminal = terminal_of(&reply);
        let pane = herdr.pane_of(&terminal).expect("the reply names a pane herdr holds");
        assert_eq!(pane.argv, resume_argv(&conv), "the pane runs the conversation's resume command");
        assert_eq!(pane.cwd, proj.display().to_string(), "it starts in the conversation's project");
        assert!(!herdr.calls().iter().any(|c| c == "agent.start"), "no fresh agent starts: {:?}", herdr.calls());
        assert_eq!(listed_for(&ctx, &conv), vec![terminal], "session.tabs.list shows the resumed pane");
    }

    #[test]
    fn a_conversation_already_open_is_focused_and_reused_not_duplicated() {
        let (root, proj, conv) = omp_setup("reuse");
        let herdr = FakeHerdr::new();
        let label = herdr_open::tab_label("omp", &conv.title, &herdr_open::session_tag(&conv.key));
        let open = herdr.open_tab("w-main", &label, &proj.display().to_string(), &[
            "omp",
            "--resume",
            conv.key.as_str(),
        ]);
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(1));
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let reply = create(&ctx, &path, json!({ "worktree": conv.worktree_id(), "agent": "omp", "clientMutationId": "m1" })).unwrap();
        assert_eq!(terminal_of(&reply), open.terminal_id);
        assert_eq!(herdr.panes().len(), 1, "no second pane");
        assert!(!herdr.calls().iter().any(|c| c.starts_with("herdr tab focus")), "a phone's resume never moves the desktop's focus: {:?}", herdr.calls());
        assert!(!herdr.calls().iter().any(|c| c.starts_with("herdr tab create") || c.starts_with("herdr pane run")));
    }

    #[test]
    fn a_conversation_open_in_another_workspace_is_reused_not_duplicated() {
        let (root, proj, conv) = omp_setup("workspace");
        let herdr = FakeHerdr::new();
        let open = herdr.open_tab("w-other", "omp: fix login", &proj.display().to_string(), &[
            "omp",
            "--resume",
            conv.key.as_str(),
        ]);
        // The bridge's own workspace is w-main: the open pane lives in another, and is still the conversation.
        let cli = herdr_open::Herdr::new("herdr", Some("w-main".into()), Box::new(Arc::clone(&herdr)));
        let resume = Resume { locations: Locations::default(), home: root.join("home"), herdr: cli, start_wait: Duration::from_secs(1) };
        let launch = Launch { workspace_id: Some("w-main".into()), ..Launch::resuming(resume) };
        let ctx = testkit::ctx_on(vec![conv.clone()], herdr.clone(), launch, &root);
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let reply = create(&ctx, &path, json!({ "worktree": conv.worktree_id(), "agent": "omp", "clientMutationId": "m1" })).unwrap();
        assert_eq!(terminal_of(&reply), open.terminal_id);
        assert_eq!(herdr.panes().len(), 1, "no duplicate in w-main");
    }

    #[test]
    fn two_creates_for_one_conversation_at_once_share_one_pane() {
        let (root, _proj, conv) = omp_setup("double-tap");
        let herdr = FakeHerdr::new();
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(1));
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let tap = |id: &str| json!({ "worktree": conv.worktree_id(), "agent": "omp", "clientMutationId": id });
        let (first, second) = std::thread::scope(|s| {
            let first = s.spawn(|| create(&ctx, &path, tap("tap-1")));
            let second = s.spawn(|| create(&ctx, &path, tap("tap-2")));
            (first.join().unwrap(), second.join().unwrap())
        });
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(terminal_of(&first), terminal_of(&second), "both taps name the same pane");
        assert_eq!(herdr.panes().len(), 1);
        assert_eq!(herdr.calls().iter().filter(|c| c.starts_with("herdr pane run")).count(), 1, "one resume command typed");
    }

    #[test]
    fn a_resume_after_the_last_tab_was_closed_opens_a_workspace_and_runs_there() {
        let (root, proj, conv) = omp_setup("no-workspace");
        let herdr = FakeHerdr::without_workspace();
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(1));
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let terminal = terminal_of(&create(&ctx, &path, json!({ "worktree": conv.worktree_id(), "agent": "omp" })).unwrap());
        assert!(herdr.calls().iter().any(|c| c.starts_with("herdr workspace create")), "{:?}", herdr.calls());
        let pane = herdr.pane_of(&terminal).expect("the reply names the new workspace's pane");
        assert_eq!(pane.argv, resume_argv(&conv));
        assert_eq!(pane.cwd, proj.display().to_string());
        assert_eq!(listed_for(&ctx, &conv), vec![terminal]);
    }

    #[test]
    fn a_tab_left_with_an_idle_shell_gets_the_resume_command_again() {
        let (root, proj, conv) = omp_setup("idle-shell");
        let herdr = FakeHerdr::new();
        let label = herdr_open::tab_label("omp", &conv.title, &herdr_open::session_tag(&conv.key));
        let shell = herdr.open_tab("w-main", &label, &proj.display().to_string(), &[]);
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(5));
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let terminal = terminal_of(&create(&ctx, &path, json!({ "worktree": conv.worktree_id(), "agent": "omp" })).unwrap());
        assert_eq!(terminal, shell.terminal_id, "the reply is the kept tab's own pane");
        assert_eq!(herdr.panes().len(), 1, "no second tab");
        let typed = format!("herdr pane run {} omp --resume {}", shell.pane_id, conv.key);
        assert!(herdr.calls().iter().any(|c| c == &typed), "the resume command was typed into the idle shell: {:?}", herdr.calls());
        assert_eq!(listed_for(&ctx, &conv), vec![terminal]);
    }

    #[test]
    fn a_resume_replies_once_herdr_reports_its_agent_up_so_a_prompt_is_not_lost_in_its_start_up() {
        let (root, _proj, conv) = omp_setup("ready");
        let herdr = FakeHerdr::new();
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(5));
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let terminal = terminal_of(&create(&ctx, &path, json!({ "worktree": conv.worktree_id(), "agent": "omp" })).unwrap());
        assert_eq!(herdr.pane_of(&terminal).unwrap().status, "idle", "the reply waited for the agent's state");
    }

    #[test]
    fn a_resumed_pane_is_found_again_before_herdr_has_named_its_agent() {
        let (root, _proj, conv) = omp_setup("announce");
        let herdr = FakeHerdr::new();
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(1));
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let tap = |id: &str| json!({ "worktree": conv.worktree_id(), "agent": "omp", "clientMutationId": id });
        let first = terminal_of(&create(&ctx, &path, tap("a")).unwrap());
        // The command is running (its argv is in the pane) but herdr has not detected the agent yet.
        herdr.forget_agent(&first);
        let second = terminal_of(&create(&ctx, &path, tap("b")).unwrap());
        assert_eq!(second, first);
        assert_eq!(herdr.panes().len(), 1);
    }

    #[test]
    fn a_conversation_is_refused_for_another_agent_or_a_cwd_before_anything_opens() {
        let (root, _proj, conv) = omp_setup("refused");
        let herdr = FakeHerdr::new();
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(1));
        let path = testkit::path_with(&root.join("bin"), &["omp", "claude"]);
        let other_agent = create(&ctx, &path, json!({ "worktree": conv.worktree_id(), "agent": "claude" })).unwrap_err();
        assert_eq!(other_agent.0, "bad_request");
        assert!(other_agent.1.contains("only be opened in omp"), "{}", other_agent.1);
        let with_cwd = create(&ctx, &path, json!({ "worktree": conv.worktree_id(), "agent": "omp", "cwd": "/tmp" })).unwrap_err();
        assert_eq!(with_cwd.0, "bad_request");
        assert!(herdr.calls().is_empty(), "nothing was asked of herdr: {:?}", herdr.calls());
        assert!(herdr.panes().is_empty());
    }

    #[test]
    fn a_claude_conversation_resumes_under_its_session_id() {
        let root = scratch_dir("claude");
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let uuid = "bbbbbbbb-1111-2222-3333-444444444444";
        let key = root.join(format!("{uuid}.jsonl"));
        std::fs::write(&key, format!("{}\n", json!({ "type": "session", "cwd": proj.display().to_string() }))).unwrap();
        let conv = conversation_at(&key, &proj, "claude", uuid);
        let herdr = FakeHerdr::new();
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(1));
        let path = testkit::path_with(&root.join("bin"), &["claude"]);
        let reply = create(&ctx, &path, json!({ "worktree": conv.worktree_id() })).unwrap();
        let terminal = terminal_of(&reply);
        assert_eq!(herdr.pane_of(&terminal).unwrap().argv, vec!["claude".to_string(), "--resume".into(), uuid.into()]);
        assert_eq!(listed_for(&ctx, &conv), vec![terminal]);
    }

    #[test]
    fn a_conversation_whose_program_is_not_installed_is_refused_before_anything_opens() {
        let (root, _proj, conv) = omp_setup("missing");
        let herdr = FakeHerdr::new();
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(1));
        let path = testkit::path_with(&root.join("bin"), &[]);
        let err = create(&ctx, &path, json!({ "worktree": conv.worktree_id() })).unwrap_err();
        assert_eq!(err.0, "bad_request");
        assert!(err.1.contains("omp is not installed"), "{}", err.1);
        assert!(herdr.calls().is_empty());
    }

    #[test]
    fn a_project_tab_starts_a_fresh_agent_and_no_conversation_owns_its_pane() {
        let (root, proj, conv) = omp_setup("project");
        let herdr = FakeHerdr::new();
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(1));
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let repo = catalog::repo_id(&proj.display().to_string());
        let reply = create(&ctx, &path, json!({ "worktree": repo, "agent": "omp" })).unwrap();
        let terminal = terminal_of(&reply);
        let calls = herdr.calls();
        assert!(calls.iter().any(|c| c == "agent.start"), "a fresh agent starts: {calls:?}");
        assert!(!calls.iter().any(|c| c.starts_with("herdr pane run")), "a project's agent is never resumed: {calls:?}");
        assert_eq!(terminals::owner_of(&*herdr, &terminal), Some(repo.clone()), "the pane belongs to the project");
        assert_ne!(terminals::owner_of(&*herdr, &terminal), Some(conv.worktree_id()), "no conversation owns it");
        assert!(listed_for(&ctx, &conv).is_empty(), "the conversation does not list the fresh pane");
    }

    #[test]
    fn a_resume_that_never_starts_closes_the_tab_it_made() {
        let (root, _proj, conv) = omp_setup("idle");
        let herdr = FakeHerdr::idle();
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_millis(50));
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let err = create(&ctx, &path, json!({ "worktree": conv.worktree_id(), "agent": "omp" })).unwrap_err();
        assert_eq!(err.0, "agent_not_started");
        let closed = herdr.calls().iter().filter(|c| c.starts_with("herdr tab close ")).count();
        assert_eq!(closed, 1, "{:?}", herdr.calls());
        assert!(herdr.panes().is_empty(), "the tab it made is gone");
    }

    #[test]
    fn a_conversation_on_another_machine_is_refused_and_never_started_fresh() {
        let (root, proj, local) = omp_setup("remote");
        // Another session in the same project, which this machine does not list: a copy on another host.
        let remote = conversation_at(&root.join("remote.jsonl"), &proj, "omp", "remote-ident");
        let herdr = FakeHerdr::new();
        let ctx = bridge_ctx(&root, vec![local], &herdr, Duration::from_secs(1));
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let err = create(&ctx, &path, json!({ "worktree": remote.worktree_id(), "agent": "omp" })).unwrap_err();
        assert_eq!(err.0, "worktree_not_found");
        assert!(err.1.contains("not on this machine"), "{}", err.1);
        assert!(herdr.calls().is_empty(), "{:?}", herdr.calls());
    }

    #[test]
    fn a_terminal_id_reused_after_a_herdr_restart_is_not_the_conversation() {
        let (root, _proj, conv) = omp_setup("restart");
        let herdr = FakeHerdr::new();
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(1));
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let terminal = terminal_of(&create(&ctx, &path, json!({ "worktree": conv.worktree_id(), "agent": "omp" })).unwrap());
        assert_eq!(listed_for(&ctx, &conv), vec![terminal.clone()]);
        herdr.restart();
        herdr.recycled_pane(&terminal, "w-main", "/home/u", &[]);
        assert!(listed_for(&ctx, &conv).is_empty(), "an unrelated shell with the recycled id is not the conversation");
        assert_eq!(terminals::owner_of(&*herdr, &terminal), None, "no record binds the id to the conversation");
    }

    #[test]
    fn an_agent_that_quit_leaves_its_shell_unlisted_for_the_conversation() {
        let (root, _proj, conv) = omp_setup("quit");
        let herdr = FakeHerdr::new();
        let ctx = bridge_ctx(&root, vec![conv.clone()], &herdr, Duration::from_secs(1));
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let terminal = terminal_of(&create(&ctx, &path, json!({ "worktree": conv.worktree_id(), "agent": "omp" })).unwrap());
        herdr.quit_agent(&terminal);
        assert!(herdr.pane_of(&terminal).is_some(), "the shell stays");
        assert!(listed_for(&ctx, &conv).is_empty(), "the shell is no longer the conversation");
    }

    #[test]
    fn a_host_without_a_herdr_cli_refuses_to_resume() {
        let (root, _proj, conv) = omp_setup("no-cli");
        let herdr = FakeHerdr::new();
        let ctx = testkit::ctx_on(vec![conv.clone()], herdr.clone(), Launch::default(), &root);
        let path = testkit::path_with(&root.join("bin"), &["omp"]);
        let err = create(&ctx, &path, json!({ "worktree": conv.worktree_id(), "agent": "omp" })).unwrap_err();
        assert_eq!(err.0, "herdr_unavailable");
        assert!(herdr.calls().is_empty());
    }

    #[test]
    fn installed_lists_the_agents_whose_program_is_present_in_table_order() {
        assert_eq!(installed(|p| p == "omp" || p == "agy"), vec!["omp", "antigravity"]);
        assert!(installed(|_| false).is_empty());
        assert_eq!(installed(|_| true), vec!["claude", "omp", "opencode", "pi", "antigravity"]);
    }

    #[test]
    fn the_detection_reply_is_a_bare_array_of_ids() {
        let reply = json!(installed(|p| p == "claude"));
        assert_eq!(reply, json!(["claude"]));
        assert!(reply.is_array());
    }

    #[test]
    fn on_path_needs_an_executable_file_in_a_listed_directory() {
        let dir = scratch_dir("path");
        std::fs::write(dir.join("fake-agent"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("not-exec"), "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("fake-agent"), std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::set_permissions(dir.join("not-exec"), std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let path = std::env::join_paths([&dir]).unwrap();
        assert!(on_path(&path, "fake-agent"));
        assert!(!on_path(&path, "not-exec"));
        assert!(!on_path(&path, "missing"));
        assert!(!on_path(OsStr::new(""), "fake-agent"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_reply_is_the_settings_object_the_sheet_reads() {
        assert_eq!(settings_value(), json!({ "settings": {} }));
    }

    #[test]
    fn agent_names_fit_herdr_rules() {
        for kind in AGENTS {
            let name = agent_name(kind);
            assert!(name.starts_with(kind.id) && name.len() <= 32, "{name}");
            assert!(name.chars().next().unwrap().is_ascii_lowercase(), "{name}");
            assert!(name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'), "{name}");
        }
        assert_ne!(agent_name(&AGENTS[1]), agent_name(&AGENTS[1]));
    }

    #[test]
    fn project_dir_matches_a_conversation_worktree_id_or_its_repo_id() {
        let conv = conversation("/home/u/proj");
        let convs = vec![conv.clone()];
        assert_eq!(project_dir_of(&convs, &conv.worktree_id()).as_deref(), Some("/home/u/proj"));
        assert_eq!(project_dir_of(&convs, &catalog::repo_id("/home/u/proj")).as_deref(), Some("/home/u/proj"));
        assert_eq!(project_dir_of(&convs, "sesync-nope::conv-nope"), None);
    }

    #[test]
    fn a_conversation_selector_is_its_worktree_id_exactly() {
        let conv = conversation("/work/a");
        let convs = vec![conv.clone()];
        assert!(conversation_of_selector(&convs, &conv.worktree_id()).is_some());
        assert!(conversation_of_selector(&convs, &catalog::repo_id("/work/a")).is_none());
    }

    #[test]
    fn a_repo_with_no_directory_resolves_to_an_empty_one_that_is_refused_later() {
        let convs = vec![conversation("")];
        assert_eq!(project_dir_of(&convs, &catalog::repo_id("")).as_deref(), Some(""));
    }

    #[test]
    fn requested_agent_names_a_table_entry_or_none() {
        let id = |v: Value| requested_agent(&v).map(|k| k.map(|k| k.id));
        assert_eq!(id(json!({ "agent": "omp" })), Ok(Some("omp")));
        assert_eq!(id(json!({ "launchAgent": "pi" })), Ok(Some("pi")));
        assert_eq!(id(json!({})), Ok(None));
        assert_eq!(id(json!({ "agent": "" })), Ok(None));
        assert_eq!(id(json!({ "agent": "codex" })).unwrap_err().0, "bad_request");
    }

    #[test]
    fn unsupported_fields_are_refused_before_anything_opens() {
        assert_eq!(refuse_unsupported(&json!({ "command": "ls" })).unwrap_err().0, "bad_request");
        assert!(refuse_unsupported(&json!({ "agentPrompt": "hi" })).is_err());
        assert!(refuse_unsupported(&json!({ "resumeProviderSession": {} })).is_err());
        assert!(refuse_unsupported(&json!({ "worktree": "id:x", "agent": "omp", "clientMutationId": "m" })).is_ok());
    }

    #[test]
    fn the_worktree_selector_loses_its_id_prefix() {
        assert_eq!(worktree_selector(&json!({ "worktree": "id:sesync-a::conv-b" })).unwrap(), "sesync-a::conv-b");
        assert_eq!(worktree_selector(&json!({})).unwrap_err().0, "bad_request");
    }

    #[test]
    fn a_cwd_override_must_be_absolute() {
        assert_eq!(cwd_param(&json!({})).unwrap(), None);
        assert_eq!(cwd_param(&json!({ "cwd": "/tmp" })).unwrap(), Some(PathBuf::from("/tmp")));
        assert_eq!(cwd_param(&json!({ "cwd": "relative" })).unwrap_err().0, "bad_request");
    }

    #[test]
    fn a_missing_directory_is_refused_and_an_existing_one_passes() {
        let dir = scratch_dir("dir");
        assert!(check_dir(&dir).is_ok());
        assert_eq!(check_dir(&dir.join("gone")).unwrap_err().0, "bad_request");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_agent_starts_fresh_with_no_resume_argument() {
        let fake = FakeAnswers::new(answer_ok);
        let launch = Launch { workspace_id: Some("w1".into()), ..Launch::default() };
        let opened = open_terminal(&fake, &launch, Path::new("/tmp/proj"), Some(&AGENTS[1])).unwrap();
        assert_eq!(opened, Opened { terminal_id: "term_a".into(), title: "omp: proj".into() });
        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "tab.create");
        assert_eq!(
            calls[0].1,
            json!({ "cwd": "/tmp/proj", "label": "omp: proj", "focus": false, "workspace_id": "w1" })
        );
        assert_eq!(calls[1].0, "agent.start");
        assert_eq!(calls[1].1["kind"], "omp");
        assert_eq!(calls[1].1["pane_id"], "w1:p2");
        assert!(calls[1].1["name"].as_str().unwrap().starts_with("omp-"));
        assert!(calls[1].1.get("args").is_none());
        assert!(!calls.iter().any(|(_, p)| p.to_string().contains("resume")));
    }

    #[test]
    fn a_plain_shell_tab_starts_no_agent() {
        let fake = FakeAnswers::new(answer_ok);
        let opened = open_terminal(&fake, &Launch::default(), Path::new("/tmp/proj"), None).unwrap();
        assert_eq!(opened.title, "terminal: proj");
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.get("workspace_id").is_none());
    }

    #[test]
    fn a_failed_agent_start_closes_the_tab_it_made() {
        let fake = FakeAnswers::new(|method| match method {
            "agent.start" => Err("agent refused".to_string()),
            other => answer_ok(other),
        });
        let err = open_terminal(&fake, &Launch::default(), Path::new("/tmp/proj"), Some(&AGENTS[0])).unwrap_err();
        assert_eq!(err.0, "agent_not_started");
        let calls = fake.calls();
        let last = calls.last().unwrap();
        assert_eq!(last.0, "tab.close");
        assert_eq!(last.1, json!({ "tab_id": "w1:t2" }));
    }

    #[test]
    fn a_refused_tab_create_is_herdr_unavailable() {
        let fake = FakeAnswers::new(|_| Err("herdr is not reachable".to_string()));
        let err = open_terminal(&fake, &Launch::default(), Path::new("/tmp/proj"), None).unwrap_err();
        assert_eq!(err.0, "herdr_unavailable");
    }

    #[test]
    fn a_tab_without_a_terminal_id_is_closed_and_refused() {
        let fake = FakeAnswers::new(|method| match method {
            "tab.create" => Ok(json!({ "tab": { "tab_id": "w1:t9" }, "root_pane": { "pane_id": "w1:p9" } })),
            other => answer_ok(other),
        });
        let err = open_terminal(&fake, &Launch::default(), Path::new("/tmp/proj"), None).unwrap_err();
        assert_eq!(err.0, "herdr_unavailable");
        assert_eq!(fake.calls().last().unwrap().0, "tab.close");
    }

    #[test]
    fn the_reply_names_the_terminal_as_its_own_id_and_handle() {
        let reply = tab_reply(&Opened { terminal_id: "term_a".into(), title: "omp: proj".into() });
        let tab = &reply["tab"];
        assert_eq!(tab["type"], "terminal");
        assert_eq!(tab["id"], "term_a");
        assert_eq!(tab["terminal"], "term_a");
        assert_eq!(tab["title"], "omp: proj");
        assert_eq!(tab["isActive"], false);
    }

    #[test]
    fn the_replay_memory_keeps_the_newest_replies_only() {
        let mut replies = VecDeque::new();
        for i in 0..REPLAY_LIMIT + 3 {
            remember(&mut replies, &format!("m{i}"), json!(i));
        }
        assert_eq!(replies.len(), REPLAY_LIMIT);
        assert_eq!(replies.front().unwrap().0, "m3");
        assert_eq!(replies.back().unwrap().0, format!("m{}", REPLAY_LIMIT + 2));
    }

    /// Answers each herdr socket call with `answer` and records every call, in order.
    struct FakeAnswers {
        calls: Mutex<Vec<(String, Value)>>,
        answer: Box<dyn Fn(&str) -> Result<Value, String> + Send + Sync>,
    }

    impl FakeAnswers {
        fn new(answer: impl Fn(&str) -> Result<Value, String> + Send + Sync + 'static) -> FakeAnswers {
            FakeAnswers { calls: Mutex::new(Vec::new()), answer: Box::new(answer) }
        }

        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().clone()
        }
    }

    impl Herdr for FakeAnswers {
        fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            self.calls.lock().push((method.to_string(), params));
            (self.answer)(method)
        }
    }

    fn tab_created() -> Value {
        json!({
            "type": "tab_created",
            "tab": { "tab_id": "w1:t2" },
            "root_pane": { "pane_id": "w1:p2", "terminal_id": "term_a" }
        })
    }

    fn answer_ok(method: &str) -> Result<Value, String> {
        match method {
            "tab.create" => Ok(tab_created()),
            "agent.start" => Ok(json!({ "type": "agent_started" })),
            _ => Ok(json!({ "type": "ok" })),
        }
    }
}
