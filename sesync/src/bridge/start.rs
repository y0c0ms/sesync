//! Starting agent sessions from the phone. Three answers, all from this machine:
//!   * `preflight.detectAgents`: the app's agent ids whose executable is on `PATH` now, as a bare array;
//!   * `settings.get`: the settings object the new-tab sheet reads (this host keeps none);
//!   * `session.tabs.createTerminal`: a new herdr tab in the worktree's directory, with the chosen agent
//!     started in it through herdr's `agent.start`. Never a resume: no session id reaches the agent's argv.
//!
//! herdr is called through `Ctx::herdr` (`tab.create`, `agent.start`, `tab.close`). The tab label is
//! `herdr_open::tab_label`, the same label "open in herdr" uses.

use super::catalog::{self, Conversation};
use super::herdr::Herdr;
use super::rpc::{Ctx, Failure};
use super::terminals;
use crate::herdr_open;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use parking_lot::{Mutex, MutexGuard};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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

/// How this host starts sessions. Cheap to clone; the replay memory is shared between clones.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    /// The herdr workspace new tabs open in. None leaves the choice to herdr.
    pub workspace_id: Option<String>,
    /// Replies to recent creates by `clientMutationId`, so a retried create returns the tab it made.
    replies: Arc<Mutex<VecDeque<(String, Value)>>>,
}

impl Launch {
    /// The workspace the herdr that runs this process uses: `$HERDR_ACTIVE_WORKSPACE_ID`, when set.
    pub fn from_env() -> Launch {
        let workspace_id = std::env::var("HERDR_ACTIVE_WORKSPACE_ID").ok().filter(|v| !v.trim().is_empty());
        Launch { workspace_id, ..Launch::default() }
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

/// `session.tabs.createTerminal`: open a herdr tab in the worktree's directory and start the named agent
/// in it, or a plain shell when no agent is named. The pane is recorded as owned by the worktree, so it
/// is listed for that worktree without any session id in its argv.
pub fn create_terminal_tab(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    refuse_unsupported(params)?;
    let agent = requested_agent(params)?;
    let selector = worktree_selector(params)?;
    let key = params["clientMutationId"].as_str().filter(|k| !k.is_empty());
    let mut replies = lock_replies(&ctx.launch);
    if let Some(key) = key {
        if let Some((_, reply)) = replies.iter().find(|(known, _)| known.as_str() == key) {
            return Ok(reply.clone());
        }
    }
    let project = worktree_dir(&selector, ctx)?;
    let cwd = cwd_param(params)?.unwrap_or(project);
    check_dir(&cwd)?;
    if let Some(kind) = agent {
        if !on_path(&path_var(), kind.program) {
            return Err(("bad_request", format!("{} is not installed on this machine", kind.id)));
        }
    }
    let opened = open_terminal(ctx.herdr.as_ref(), &ctx.launch, &cwd, agent)?;
    terminals::remember_pane(&opened.terminal_id, &selector);
    let reply = tab_reply(&opened);
    if let Some(key) = key {
        remember(&mut replies, key, reply.clone());
    }
    Ok(reply)
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

/// The project directory of the worktree a selector names.
fn worktree_dir(selector: &str, ctx: &Ctx) -> Result<PathBuf, Failure> {
    let conversations = ctx.catalog.conversations().map_err(|m| ("unavailable", m))?;
    let dir = project_dir_of(&conversations, selector)
        .ok_or_else(|| ("worktree_not_found", format!("no worktree {selector}")))?;
    if dir.is_empty() {
        return Err(("bad_request", format!("worktree {selector} records no project directory")));
    }
    Ok(PathBuf::from(dir))
}

/// The directory of the worktree `selector` names: a conversation's worktree id, or the repo id that
/// `repo.list` reports for a directory. None when no conversation matches.
pub fn project_dir_of(conversations: &[Conversation], selector: &str) -> Option<String> {
    conversations
        .iter()
        .find(|c| c.worktree_id() == selector)
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
    use std::sync::atomic::AtomicUsize;

    /// Answers each herdr call with `answer` and records every call, in order.
    struct Fake {
        calls: Mutex<Vec<(String, Value)>>,
        answer: Box<dyn Fn(&str) -> Result<Value, String> + Send + Sync>,
    }

    impl Fake {
        fn new(answer: impl Fn(&str) -> Result<Value, String> + Send + Sync + 'static) -> Fake {
            Fake { calls: Mutex::new(Vec::new()), answer: Box::new(answer) }
        }

        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().clone()
        }
    }

    impl Herdr for Fake {
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
        let fake = Fake::new(answer_ok);
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
        let fake = Fake::new(answer_ok);
        let opened = open_terminal(&fake, &Launch::default(), Path::new("/tmp/proj"), None).unwrap();
        assert_eq!(opened.title, "terminal: proj");
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.get("workspace_id").is_none());
    }

    #[test]
    fn a_failed_agent_start_closes_the_tab_it_made() {
        let fake = Fake::new(|method| match method {
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
        let fake = Fake::new(|_| Err("herdr is not reachable".to_string()));
        let err = open_terminal(&fake, &Launch::default(), Path::new("/tmp/proj"), None).unwrap_err();
        assert_eq!(err.0, "herdr_unavailable");
    }

    #[test]
    fn a_tab_without_a_terminal_id_is_closed_and_refused() {
        let fake = Fake::new(|method| match method {
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
}
