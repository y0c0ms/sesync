//! Test doubles the bridge's unit tests share: a fixed conversation list, a herdr that answers nothing,
//! an in-memory herdr that answers both the socket API and the CLI, and a request context over a
//! directory. Compiled only for tests.

use super::catalog::{Catalog, Conversation};
use super::herdr::Herdr;
use super::rpc::Ctx;
use super::dictation::{Dictation, Whisper};
use super::{notify, start, transfer, uploads, usage};
use super::turns::{LockTools, Turns};
use crate::harness::Locations;
use crate::herdr_open;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A catalog that lists the conversations it was given.
pub struct Fixed(pub Vec<Conversation>);

impl Catalog for Fixed {
    fn conversations(&self) -> Result<Vec<Conversation>, String> {
        Ok(self.0.clone())
    }
}

/// A herdr that refuses every call, as when no herdr is running.
pub struct NoHerdr;

impl Herdr for NoHerdr {
    fn call(&self, method: &str, _params: Value) -> Result<Value, String> {
        Err(format!("no herdr for {method}"))
    }
}

/// One omp conversation whose project directory is `dir`.
pub fn conversation(title: &str, dir: &str) -> Conversation {
    Conversation {
        harness: "omp".into(),
        key: format!("/s/{title}.jsonl"),
        title: title.into(),
        dir: dir.into(),
        updated: 1_700_000_000.0,
        idents: vec![format!("ident-{title}")],
    }
}

/// A request context that lists `conversations`, has no herdr, and keeps its state under `state_dir`.
pub fn ctx_with(conversations: Vec<Conversation>, state_dir: &Path) -> Ctx {
    ctx_on(conversations, Arc::new(NoHerdr), start::Launch::default(), state_dir)
}

/// A request context that lists `conversations` and calls `herdr`, starting sessions as `launch` says.
pub fn ctx_on(conversations: Vec<Conversation>, herdr: Arc<dyn Herdr>, launch: start::Launch, state_dir: &Path) -> Ctx {
    Ctx {
        catalog: Arc::new(Fixed(conversations)),
        herdr,
        host: super::rpc::Host { machine_name: "box".into(), platform: "linux" },
        locks: LockTools { sesync_lock: "/nonexistent/sesync-lock".into(), lock_dir: "/nonexistent".into() },
        turns: Turns::default(),
        dir: state_dir.to_path_buf(),
        machines: state_dir.join("machines.json"),
        notify: notify::Hub::new(None),
        usage: usage::Store::default(),
        uploads: uploads::Store::default(),
        transfers: transfer::Store::default(),
        dictation: Dictation::new(Whisper::from_env()),
        launch,
    }
}

/// A fresh, empty directory under the system temp dir, unique to `name` and this process.
pub fn sandbox(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sesync-bridge-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One pane a [`FakeHerdr`] holds: its ids, its workspace and tab, the tab's label, the directory it opened
/// in, the foreground process its command started (none until one does), and herdr's agent status.
#[derive(Debug, Clone, PartialEq)]
pub struct FakePane {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub label: String,
    pub cwd: String,
    pub argv: Vec<String>,
    pub agent: Option<String>,
    pub status: String,
    /// Listings since the agent started, while its status is still unknown.
    pub polls: u32,
    /// The pane's size as herdr's layout reports it; None when herdr has no layout for the pane.
    pub size: Option<(u32, u32)>,
    /// The visible screen `pane.read` returns.
    pub screen: String,
    /// The rows above the screen: `pane.read` "recent" returns them, then the screen.
    pub history: Vec<String>,
}

/// Ids are unique across every fake herdr of the process: the bridge's owner records are process-wide.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// The workspace a tab opens in when no workspace is named.
const DEFAULT_WORKSPACE: &str = "w-main";

/// A herdr held in memory. It answers the socket calls the bridge makes and the CLI calls "Open in herdr"
/// makes, over one set of panes, so a create's whole path runs against the panes it made. A command that
/// starts (the CLI's `pane run`, the socket's `agent.start`) makes its program the pane's foreground process,
/// as herdr reports it; `tab.close` removes the tab. Every call is recorded in order: a socket call by its
/// method, a CLI call as `herdr <args>`. Typed input is recorded as (target, text).
pub struct FakeHerdr {
    /// This fake's server name: the owner records of one fake never touch another's.
    name: String,
    state: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    panes: Vec<FakePane>,
    calls: Vec<String>,
    sends: Vec<(String, String)>,
    starts: bool,
    /// herdr has no workspace: a tab cannot be created until a workspace is.
    no_workspace: bool,
}

impl FakeHerdr {
    /// A herdr whose commands start their programs.
    pub fn new() -> Arc<FakeHerdr> {
        Arc::new(FakeHerdr { name: server_name(), state: Mutex::new(FakeState { starts: true, ..FakeState::default() }) })
    }

    /// A herdr with no workspace left (the last tab of the last workspace was closed): tab create refuses.
    pub fn without_workspace() -> Arc<FakeHerdr> {
        Arc::new(FakeHerdr { name: server_name(), state: Mutex::new(FakeState { starts: true, no_workspace: true, ..FakeState::default() }) })
    }

    /// A herdr whose commands never start: a pane stays at its shell.
    pub fn idle() -> Arc<FakeHerdr> {
        Arc::new(FakeHerdr { name: server_name(), state: Mutex::new(FakeState::default()) })
    }

    /// A tab labelled `label` in `workspace` and `cwd`, whose root pane already runs `argv` (a session left
    /// open, or a shell when `argv` is empty).
    pub fn open_tab(&self, workspace: &str, label: &str, cwd: &str, argv: &[&str]) -> FakePane {
        let mut st = self.state.lock();
        let pane = FakePane::new(next_ids(), workspace, label, cwd);
        st.panes.push(pane.clone());
        st.set_argv(&pane.pane_id, argv.iter().map(|a| a.to_string()).collect());
        st.pane(&pane.pane_id).cloned().unwrap()
    }

    /// The agent in the pane `terminal_id` quits: its shell stays, with no foreground process.
    pub fn quit_agent(&self, terminal_id: &str) {
        let mut st = self.state.lock();
        if let Some(pane) = st.panes.iter_mut().find(|p| p.terminal_id == terminal_id) {
            pane.argv.clear();
            pane.agent = None;
        }
    }

    /// The pane `terminal_id` still runs its command, but herdr has not named an agent in it yet.
    pub fn forget_agent(&self, terminal_id: &str) {
        let mut st = self.state.lock();
        if let Some(pane) = st.panes.iter_mut().find(|p| p.terminal_id == terminal_id) {
            pane.agent = None;
        }
    }

    /// herdr's agent status for the pane `terminal_id` (`idle`, `working`, `blocked`, ...).
    pub fn set_status(&self, terminal_id: &str, status: &str) {
        let mut st = self.state.lock();
        if let Some(pane) = st.panes.iter_mut().find(|p| p.terminal_id == terminal_id) {
            pane.status = status.into();
        }
    }

    /// herdr restarts: every pane is gone. Ids minted afterwards are new, unless a test recycles one.
    pub fn restart(&self) {
        self.state.lock().panes.clear();
    }

    /// A pane that takes over the terminal id `terminal_id` after a restart (herdr may reuse one), running
    /// `argv` in a tab of `workspace`.
    pub fn recycled_pane(&self, terminal_id: &str, workspace: &str, cwd: &str, argv: &[&str]) -> FakePane {
        let mut st = self.state.lock();
        let mut pane = FakePane::new(next_ids(), workspace, "recycled", cwd);
        pane.terminal_id = terminal_id.into();
        st.panes.push(pane.clone());
        st.set_argv(&pane.pane_id, argv.iter().map(|a| a.to_string()).collect());
        st.pane(&pane.pane_id).cloned().unwrap()
    }

    /// The pane `terminal_id` as herdr holds it now.
    pub fn pane_of(&self, terminal_id: &str) -> Option<FakePane> {
        self.state.lock().panes.iter().find(|p| p.terminal_id == terminal_id).cloned()
    }

    pub fn panes(&self) -> Vec<FakePane> {
        self.state.lock().panes.clone()
    }

    pub fn calls(&self) -> Vec<String> {
        self.state.lock().calls.clone()
    }

    pub fn sends(&self) -> Vec<(String, String)> {
        self.state.lock().sends.clone()
    }

    /// Set the pane `terminal_id`'s visible screen and size in one step, so a read sees both or neither.
    pub fn set_pane(&self, terminal_id: &str, screen: &str, size: (u32, u32)) {
        let mut st = self.state.lock();
        if let Some(pane) = st.panes.iter_mut().find(|p| p.terminal_id == terminal_id) {
            pane.screen = screen.into();
            pane.size = Some(size);
        }
    }

    /// Set the rows above the screen of the pane `terminal_id`, oldest first.
    pub fn set_history(&self, terminal_id: &str, history: &[&str]) {
        let mut st = self.state.lock();
        if let Some(pane) = st.panes.iter_mut().find(|p| p.terminal_id == terminal_id) {
            pane.history = history.iter().map(|r| r.to_string()).collect();
        }
    }

    /// herdr reports no layout for the pane `terminal_id`: its `pane.layout` refuses.
    pub fn hide_layout(&self, terminal_id: &str) {
        let mut st = self.state.lock();
        if let Some(pane) = st.panes.iter_mut().find(|p| p.terminal_id == terminal_id) {
            pane.size = None;
        }
    }
}

/// The ids of one new pane: a number unique to the process, used in the pane, tab and terminal ids.
fn next_ids() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed) + 1
}

/// A name for one fake herdr server, unique to the process.
fn server_name() -> String {
    format!("fake-herdr-{}", next_ids())
}

impl FakePane {
    fn new(n: u64, workspace: &str, label: &str, cwd: &str) -> FakePane {
        FakePane {
            pane_id: format!("w{n}:p1"),
            terminal_id: format!("term_{n}"),
            workspace_id: workspace.into(),
            tab_id: format!("w{n}:t1"),
            label: label.into(),
            cwd: cwd.into(),
            argv: Vec::new(),
            agent: None,
            status: "idle".into(),
            polls: 0,
            size: Some((80, 24)),
            screen: String::new(),
            history: Vec::new(),
        }
    }
}

impl FakeState {
    /// A new tab in `workspace` (the default when none is named), with its root pane.
    fn new_tab(&mut self, workspace: Option<&str>, label: &str, cwd: &str) -> FakePane {
        let pane = FakePane::new(next_ids(), workspace.unwrap_or(DEFAULT_WORKSPACE), label, cwd);
        self.panes.push(pane.clone());
        pane
    }

    /// Make `argv` the foreground process of `pane_id`, when commands start here.
    fn set_argv(&mut self, pane_id: &str, argv: Vec<String>) {
        if !self.starts {
            return;
        }
        if let Some(pane) = self.panes.iter_mut().find(|p| p.pane_id == pane_id) {
            pane.agent = argv.first().cloned();
            pane.argv = argv;
            // herdr reports a started agent as unknown until its TUI is up; the next two listings see it ready.
            pane.status = "unknown".into();
            pane.polls = 0;
        }
    }

    fn close(&mut self, tab_id: &str) {
        self.panes.retain(|p| p.tab_id != tab_id);
    }

    /// Each tab once, with its label, in creation order.
    fn tabs(&self) -> Vec<(String, String)> {
        let mut tabs: Vec<(String, String)> = Vec::new();
        for p in &self.panes {
            if !tabs.iter().any(|(tab, _)| *tab == p.tab_id) {
                tabs.push((p.tab_id.clone(), p.label.clone()));
            }
        }
        tabs
    }

    fn pane(&self, pane_id: &str) -> Option<&FakePane> {
        self.panes.iter().find(|p| p.pane_id == pane_id)
    }
}

impl Herdr for FakeHerdr {
    fn scope(&self) -> String {
        self.name.clone()
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut st = self.state.lock();
        st.calls.push(method.to_string());
        let text = |key: &str| params[key].as_str().unwrap_or("").to_string();
        match method {
            "pane.list" => {
                for pane in st.panes.iter_mut().filter(|p| p.status == "unknown" && !p.argv.is_empty()) {
                    pane.polls += 1;
                    if pane.polls >= 2 {
                        pane.status = "idle".into();
                    }
                }
                let panes: Vec<Value> = st.panes.iter().map(socket_pane).collect();
                Ok(json!({ "type": "pane_list", "panes": panes }))
            }
            "tab.list" => {
                let tabs: Vec<Value> = st.tabs().iter().map(|(tab, label)| json!({ "tab_id": tab, "label": label })).collect();
                Ok(json!({ "type": "tab_list", "tabs": tabs }))
            }
            "pane.process_info" => {
                let info = process_info(st.pane(&text("pane_id")));
                Ok(json!({ "type": "pane_process_info", "process_info": info }))
            }
            "tab.create" => {
                let workspace = params["workspace_id"].as_str();
                let pane = st.new_tab(workspace, &text("label"), &text("cwd"));
                Ok(json!({
                    "type": "tab_created",
                    "tab": { "tab_id": pane.tab_id },
                    "root_pane": { "pane_id": pane.pane_id, "terminal_id": pane.terminal_id },
                }))
            }
            "agent.start" => {
                st.set_argv(&text("pane_id"), vec![text("kind")]);
                Ok(json!({ "type": "agent_started" }))
            }
            "tab.close" => {
                st.close(&text("tab_id"));
                Ok(json!({ "type": "ok" }))
            }
            "pane.send_text" => {
                st.sends.push((text("pane_id"), text("text")));
                Ok(json!({ "type": "ok" }))
            }
            "agent.prompt" => {
                st.sends.push((text("target"), text("text")));
                Ok(json!({ "type": "agent_prompted" }))
            }
            "pane.send_keys" => Ok(json!({ "type": "ok" })),
            "pane.read" => {
                let pane_id = text("pane_id");
                let pane = st.pane(&pane_id).ok_or_else(|| format!("no pane {pane_id}"))?;
                let screen = if params["source"] == "recent" {
                    let rows: Vec<&str> = pane.history.iter().map(String::as_str).chain(pane.screen.split('\n')).collect();
                    let lines = params["lines"].as_u64().map_or(rows.len(), |n| n as usize);
                    rows[rows.len().saturating_sub(lines)..].join("\n")
                } else {
                    pane.screen.clone()
                };
                Ok(json!({ "type": "pane_read", "read": { "text": screen, "pane_id": pane_id } }))
            }
            "pane.layout" => {
                let pane_id = text("pane_id");
                let pane = st.pane(&pane_id).ok_or_else(|| format!("no pane {pane_id}"))?;
                let (cols, rows) = pane.size.ok_or_else(|| "herdr has no layout for that pane".to_string())?;
                let rect = json!({ "x": 0, "y": 0, "width": cols, "height": rows });
                Ok(json!({ "type": "pane_layout", "layout": {
                    "focused_pane_id": pane_id,
                    "panes": [{ "pane_id": pane_id, "focused": true, "rect": rect }],
                    "splits": [],
                } }))
            }
            other => Err(format!("fake herdr: no answer for {other}")),
        }
    }
}

/// The CLI face of a [`FakeHerdr`], for `herdr_open`: the calls `open` makes, answered as herdr's CLI does.
impl herdr_open::Runner for Arc<FakeHerdr> {
    fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
        let mut st = self.state.lock();
        st.calls.push(format!("{program} {}", args.join(" ")));
        let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let answer = match argv.as_slice() {
            ["tab", "list"] => {
                let tabs: Vec<Value> = st.tabs().iter().map(|(tab, label)| json!({ "tab_id": tab, "label": label })).collect();
                json!({ "result": { "tabs": tabs } })
            }
            ["pane", "list"] => {
                let panes: Vec<Value> = st.panes.iter().map(|p| json!({ "pane_id": p.pane_id, "tab_id": p.tab_id, "cwd": p.cwd })).collect();
                json!({ "result": { "panes": panes } })
            }
            ["pane", "process-info", "--pane", pane] => {
                json!({ "result": { "process_info": process_info(st.pane(pane)) } })
            }
            ["tab", "create", ..] => {
                if st.no_workspace {
                    return Ok(json!({ "error": { "code": "workspace_not_found", "message": "no active workspace" } }).to_string());
                }
                let pane = st.new_tab(flag("--workspace").as_deref(), &flag("--label").unwrap_or_default(), &flag("--cwd").unwrap_or_default());
                json!({ "result": { "tab": { "tab_id": pane.tab_id }, "root_pane": { "pane_id": pane.pane_id } } })
            }
            ["workspace", "create", ..] => {
                st.no_workspace = false;
                let pane = st.new_tab(None, &flag("--label").unwrap_or_default(), &flag("--cwd").unwrap_or_default());
                json!({ "result": { "type": "workspace_created", "workspace": { "workspace_id": "w-new" }, "tab": { "tab_id": pane.tab_id }, "root_pane": { "pane_id": pane.pane_id } } })
            }
            ["pane", "run", pane, command] => {
                st.set_argv(pane, split_posix(command));
                return Ok(String::new());
            }
            ["tab", "focus", _] => return Ok(String::new()),
            ["tab", "close", tab] => {
                st.close(tab);
                return Ok(String::new());
            }
            _ => return Err(format!("fake herdr: no CLI answer for {args:?}")),
        };
        Ok(answer.to_string())
    }
}

/// A pane as herdr's socket `pane.list` reports it.
fn socket_pane(p: &FakePane) -> Value {
    json!({
        "pane_id": p.pane_id,
        "terminal_id": p.terminal_id,
        "tab_id": p.tab_id,
        "focused": false,
        "agent_status": p.status,
        "agent": p.agent,
        "title": null,
        "foreground_cwd": p.cwd,
        "scroll": { "viewport_rows": 40 },
    })
}

/// A pane's process info as herdr reports it: its foreground process, when one runs.
fn process_info(pane: Option<&FakePane>) -> Value {
    let foreground: Vec<Value> = match pane {
        Some(p) if !p.argv.is_empty() => vec![json!({ "argv": p.argv })],
        _ => Vec::new(),
    };
    json!({ "foreground_processes": foreground, "shell_pid": null })
}

/// The words of a POSIX shell line as `herdr_open::shell_line` writes them: bare words, and single-quoted
/// words with `'\''` for a quote.
fn split_posix(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if quoted {
            if c == '\'' {
                quoted = false;
            } else {
                word.push(c);
            }
        } else if c == '\'' {
            quoted = true;
            in_word = true;
        } else if c == '\\' && chars.get(i + 1) == Some(&'\'') {
            word.push('\'');
            in_word = true;
            i += 1;
        } else if c == ' ' {
            if in_word {
                words.push(std::mem::take(&mut word));
                in_word = false;
            }
        } else {
            word.push(c);
            in_word = true;
        }
        i += 1;
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Launch settings that resume conversations through `herdr`'s CLI, with `start_wait` for a resumed session to
/// show as running in its tab.
pub fn resume_launch(herdr: &Arc<FakeHerdr>, locations: Locations, home: PathBuf, start_wait: Duration) -> start::Launch {
    let cli = herdr_open::Herdr::new("herdr", None, Box::new(Arc::clone(herdr)));
    start::Launch::resuming(start::Resume { locations, home, herdr: cli, start_wait })
}

/// A directory holding an executable file for each of `programs`, and the PATH that names only it.
pub fn path_with(dir: &Path, programs: &[&str]) -> OsString {
    std::fs::create_dir_all(dir).unwrap();
    for program in programs {
        let file = dir.join(program);
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    std::env::join_paths([dir]).unwrap()
}
