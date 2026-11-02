//! Open a session in herdr, the terminal multiplexer for coding agents.
//!
//! The TUI's "Open in herdr" (session menu row, or `o`) runs inside a herdr popup started by a
//! `[[keys.command]]` entry (`sesync --tui`). It does one of two things: focuses the tab whose pane
//! already runs the session, or opens a tab in the session's directory and types the harness's
//! resume command into it. Everything goes through the herdr CLI, one JSON answer per call. The
//! process runner is a parameter, so tests never start herdr.
//!
//! A pane counts as running the session only when a foreground process has the session's
//! identifier as a flag's value (`omp --resume <path>`, `claude --resume <uuid>`,
//! `opencode --session <id>`). A bare picker, or a program that merely mentions the file, does
//! not match, so a second tab opens in that case.

use sha2::{Digest, Sha256};
use crate::harness::{self, Locations};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How long one herdr call may run. herdr answers over a local socket in milliseconds; a call that
/// hangs must not freeze the list.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest tab label, in characters.
const LABEL_MAX: usize = 40;

/// Runs one program and returns its standard output, or why it failed. The phone bridge shares one
/// client across its request threads, so a runner must be thread-safe.
pub trait Runner: Send + Sync {
    fn run(&self, program: &str, args: &[String]) -> Result<String, String>;
}

/// Runs real processes and kills one that outlives [`CALL_TIMEOUT`].
pub struct Process;

impl Runner for Process {
    fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
        let mut command = Command::new(program);
        command.args(args);
        run_command(command, program)
    }
}

/// Like [`Process`], for a herdr other than the one this process may run under: every inherited
/// `HERDR_*` variable is dropped (inside a herdr pane they name that herdr's server), then `env` is
/// set.
pub struct Isolated {
    pub env: Vec<(String, String)>,
}

impl Isolated {
    /// A command for `program` with this environment.
    pub fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("HERDR_") {
                command.env_remove(&key);
            }
        }
        command.envs(self.env.iter().map(|(k, v)| (k, v)));
        command
    }
}

impl Runner for Isolated {
    fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
        let mut command = self.command(program);
        command.args(args);
        run_command(command, program)
    }
}

fn run_command(mut command: Command, program: &str) -> Result<String, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    // Both pipes are read while the child runs: a full pipe would stop it, and the wait
    // would end in a timeout.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_reader = std::thread::spawn(move || read_pipe(stdout));
    let err_reader = std::thread::spawn(move || read_pipe(stderr));
    let deadline = Instant::now() + CALL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} did not answer within {}s", CALL_TIMEOUT.as_secs()));
            }
            Err(e) => return Err(format!("cannot wait for {program}: {e}")),
        }
    };
    let stdout = out_reader.join().unwrap_or_default();
    if status.success() {
        return Ok(stdout);
    }
    let stderr = err_reader.join().unwrap_or_default();
    Err(failure_text(&stdout, &stderr, status))
}

fn read_pipe<R: Read>(pipe: Option<R>) -> String {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Why a herdr call failed: herdr's own message when it gave one, else stderr, else the status.
fn failure_text(stdout: &str, stderr: &str, status: ExitStatus) -> String {
    let herdr_says = serde_json::from_str::<Value>(stdout.trim())
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string));
    if let Some(message) = herdr_says {
        return message;
    }
    let stderr = stderr.trim();
    if stderr.is_empty() { format!("exited with {status}") } else { stderr.to_string() }
}

/// A pane herdr lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub pane_id: String,
    pub tab_id: String,
    /// The directory herdr reports for the pane, when it reports one.
    pub cwd: Option<String>,
}

/// A herdr client: the binary to run, the workspace new tabs open in, and the runner calls go through.
pub struct Herdr {
    bin: String,
    workspace: Option<String>,
    runner: Box<dyn Runner>,
}

impl Herdr {
    pub fn new(bin: impl Into<String>, workspace: Option<String>, runner: Box<dyn Runner>) -> Herdr {
        Herdr { bin: bin.into(), workspace, runner }
    }

    /// The herdr this process belongs to: `$HERDR_BIN_PATH` (herdr sets it for its panes and
    /// popups), else `herdr` on PATH. New tabs open in `$HERDR_ACTIVE_WORKSPACE_ID` when it is set.
    pub fn from_env(runner: Box<dyn Runner>) -> Herdr {
        let bin = non_empty_env("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".to_string());
        Herdr::new(bin, non_empty_env("HERDR_ACTIVE_WORKSPACE_ID"), runner)
    }

    /// One CLI call, parsed. herdr reports a refused call as `{"error": {"message": ...}}`.
    fn call(&self, args: &[&str]) -> Result<Value, String> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let text = self.runner.run(&self.bin, &args)?;
        // Some calls answer nothing when they succeed (`pane run` does).
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        let value: Value = serde_json::from_str(text.trim()).map_err(|_| "herdr answered something other than JSON".to_string())?;
        match value.get("error") {
            Some(err) => Err(err["message"].as_str().unwrap_or("herdr refused the call").to_string()),
            None => Ok(value),
        }
    }

    /// Every pane herdr has, in every workspace.
    pub fn panes(&self) -> Result<Vec<Pane>, String> {
        let v = self.call(&["pane", "list"])?;
        let panes = v["result"]["panes"].as_array().ok_or("herdr's pane list has no panes")?;
        Ok(panes
            .iter()
            .filter_map(|p| Some(Pane { pane_id: text(p, "pane_id")?, tab_id: text(p, "tab_id")?, cwd: text(p, "cwd") }))
            .collect())
    }

    /// Every tab herdr has, with its label, in every workspace.
    pub fn tab_labels(&self) -> Result<Vec<(String, String)>, String> {
        let v = self.call(&["tab", "list"])?;
        let tabs = v["result"]["tabs"].as_array().ok_or("herdr's tab list has no tabs")?;
        Ok(tabs.iter().filter_map(|t| Some((text(t, "tab_id")?, text(t, "label").unwrap_or_default()))).collect())
    }

    /// The argument lists of a pane's foreground processes, with a stale session-file argument replaced by the file
    /// the process has open (see `bridge::herdr::live_process_argvs`).
    pub fn foreground_argvs(&self, pane_id: &str) -> Result<Vec<Vec<String>>, String> {
        let v = self.call(&["pane", "process-info", "--pane", pane_id])?;
        crate::bridge::herdr::live_process_argvs(&v["result"])
    }

    /// The pid of a pane's shell, when herdr reports it.
    pub fn shell_pid(&self, pane_id: &str) -> Result<Option<u32>, String> {
        let v = self.call(&["pane", "process-info", "--pane", pane_id])?;
        Ok(v["result"]["process_info"]["shell_pid"].as_u64().and_then(|pid| u32::try_from(pid).ok()))
    }

    /// Open a tab in `cwd` without switching to it. Returns the tab's id and its root pane's id.
    pub fn create_tab(&self, cwd: &Path, label: &str) -> Result<(String, String), String> {
        self.create_tab_with_env(cwd, label, &[])
    }

    /// [`Herdr::create_tab`], with `env` set for the tab's process. herdr starts a tab with its own environment, not
    /// the caller's, so a tab that must read this sesync's configuration is given it here.
    pub fn create_tab_with_env(&self, cwd: &Path, label: &str, env: &[(&str, &str)]) -> Result<(String, String), String> {
        self.create_tab_in_workspace(self.workspace.as_deref(), cwd, label, env)
    }

    /// A new tab labelled `label`, in `workspace` when given (else herdr's default), with `env` set for its shell.
    pub fn create_tab_in_workspace(&self, workspace: Option<&str>, cwd: &Path, label: &str, env: &[(&str, &str)]) -> Result<(String, String), String> {
        let cwd = cwd.display().to_string();
        let assignments: Vec<String> = env.iter().map(|(key, value)| format!("{key}={value}")).collect();
        let mut args = vec!["tab", "create", "--cwd", cwd.as_str(), "--label", label, "--no-focus"];
        for assignment in &assignments {
            args.extend(["--env", assignment.as_str()]);
        }
        if let Some(workspace) = workspace {
            args.extend(["--workspace", workspace]);
        }
        let v = self.call(&args)?;
        match (v["result"]["tab"]["tab_id"].as_str(), v["result"]["root_pane"]["pane_id"].as_str()) {
            (Some(tab), Some(pane)) => Ok((tab.to_string(), pane.to_string())),
            _ => Err("herdr's new tab has no tab or pane id".to_string()),
        }
    }

    /// Type `command` into a pane and press Enter.
    pub fn run_in_pane(&self, pane_id: &str, command: &str) -> Result<(), String> {
        self.call(&["pane", "run", pane_id, command]).map(|_| ())
    }

    /// The workspace that holds every bridge tab to `machine`, created the first time one is needed. Matched by its
    /// label (see [`machine_workspace_label`]), which herdr keeps across restarts.
    pub fn machine_workspace(&self, machine: &str, cwd: &Path) -> Result<String, String> {
        let want = machine_workspace_label(machine);
        let v = self.call(&["workspace", "list"])?;
        let found = v["result"]["workspaces"]
            .as_array()
            .and_then(|list| list.iter().find(|w| w["label"].as_str() == Some(want.as_str())))
            .and_then(|w| w["workspace_id"].as_str().map(str::to_string));
        match found {
            Some(id) => Ok(id),
            None => self.create_workspace_with_id(cwd, &want),
        }
    }

    /// A new tab in `workspace`, not focused, with `label`, returning the tab and its root pane.
    pub fn create_tab_in(&self, workspace: &str, cwd: &Path, label: &str) -> Result<(String, String), String> {
        let cwd = cwd.display().to_string();
        let v = self.call(&["tab", "create", "--cwd", cwd.as_str(), "--label", label, "--no-focus", "--workspace", workspace])?;
        match (v["result"]["tab"]["tab_id"].as_str(), v["result"]["root_pane"]["pane_id"].as_str()) {
            (Some(tab), Some(pane)) => Ok((tab.to_string(), pane.to_string())),
            _ => Err("herdr's new tab has no tab or pane id".to_string()),
        }
    }

    /// Create a workspace labelled `label` and return its id, as herdr reports it.
    fn create_workspace_with_id(&self, cwd: &Path, label: &str) -> Result<String, String> {
        let cwd = cwd.display().to_string();
        let v = self.call(&["workspace", "create", "--cwd", cwd.as_str(), "--label", label, "--no-focus"])?;
        v["result"]["root_pane"]["workspace_id"]
            .as_str()
            .or_else(|| v["result"]["tab"]["workspace_id"].as_str())
            .map(str::to_string)
            .ok_or_else(|| "herdr's new workspace has no id".to_string())
    }

    /// Open a workspace in `cwd`, unfocused, for when herdr has none to open a tab in. Returns the workspace's first
    /// tab's id and that tab's root pane's id.
    pub fn create_workspace(&self, cwd: &Path, label: &str) -> Result<(String, String), String> {
        let cwd = cwd.display().to_string();
        let v = self.call(&["workspace", "create", "--cwd", cwd.as_str(), "--label", label, "--no-focus"])?;
        match (v["result"]["tab"]["tab_id"].as_str(), v["result"]["root_pane"]["pane_id"].as_str()) {
            (Some(tab), Some(pane)) => Ok((tab.to_string(), pane.to_string())),
            _ => Err("herdr's new workspace has no tab or pane id".to_string()),
        }
    }

    pub fn focus_tab(&self, tab_id: &str) -> Result<(), String> {
        self.call(&["tab", "focus", tab_id]).map(|_| ())
    }

    pub fn close_tab(&self, tab_id: &str) -> Result<(), String> {
        self.call(&["tab", "close", tab_id]).map(|_| ())
    }
}

fn text(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().map(str::to_string)
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// The label of the herdr workspace that holds every bridge tab to `machine`. Its suffix marks it as sesync's.
pub fn machine_workspace_label(machine: &str) -> String {
    format!("{machine} \u{00b7} sesync")
}

/// What opening one session takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The harness's resume command: the program first, the session's identifier last.
    pub argv: Vec<String>,
    /// Where the new tab starts: the session's project directory.
    pub cwd: PathBuf,
    /// True when the project directory is unknown or gone, so `cwd` is the home directory instead.
    pub cwd_fallback: bool,
    /// The tab's label.
    pub label: String,
    /// The session's tag: the identifier its tab label ends with (see [`session_tag`]).
    pub tag: String,
}

impl Plan {
    /// The identifier that marks a running pane as this session: the resume command's last argument.
    pub fn ident(&self) -> &str {
        self.argv.last().map_or("", String::as_str)
    }
}

/// The plan for session `key` of harness `fmt`, titled `title`. A session whose project directory
/// is unknown or gone starts in `home`. Err says why the session cannot be opened from herdr.
pub fn plan(locations: &Locations, home: &Path, fmt: &str, key: &str, title: &str) -> Result<Plan, String> {
    let harness = harness::get(fmt).ok_or_else(|| format!("sesync does not know the harness '{fmt}'"))?;
    let loc = locations.of(fmt);
    let argv = harness.resume_argv(loc, key)?;
    let (cwd, cwd_fallback) = match harness.project_dir(loc, key).filter(|dir| Path::new(dir).is_dir()) {
        Some(dir) => (PathBuf::from(dir), false),
        None => (home.to_path_buf(), true),
    };
    let tag = session_tag(key);
    Ok(Plan { argv, cwd, cwd_fallback, label: tab_label(fmt, title, &tag), tag })
}

/// The tab's label: the harness, then the session's title, then the session's tag, on one line and
/// at most [`LABEL_MAX`] characters. The title gives way first, so the tag survives.
pub fn tab_label(fmt: &str, title: &str, tag: &str) -> String {
    let title: String = crate::session_cols::display_safe(title).chars().filter(|c| !c.is_control()).collect();
    let head = if title.is_empty() { format!("{fmt} session") } else { format!("{fmt}: {title}") };
    let suffix = tag_suffix(tag);
    let room = LABEL_MAX - suffix.chars().count();
    if head.chars().count() <= room {
        return format!("{head}{suffix}");
    }
    let cut: String = head.chars().take(room - 1).collect();
    format!("{cut}…{suffix}")
}

/// A short identifier for a session, stable across runs: the first 8 hex digits of the SHA-256 of
/// its key.
pub fn session_tag(key: &str) -> String {
    Sha256::digest(key.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// The shell a herdr pane runs a typed line in. The line is quoted for that shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneShell {
    Posix,
    PowerShell,
    Cmd,
}

impl PaneShell {
    /// The shell new panes of the herdr at `home` run: `default_shell` from its config.toml, else
    /// `$SHELL` on Unix, else herdr's platform default (PowerShell on Windows).
    pub fn current(home: &Path) -> PaneShell {
        let config = config_path(home).and_then(|path| std::fs::read_to_string(path).ok());
        let configured = config.as_deref().and_then(configured_shell);
        PaneShell::resolve(configured.as_deref(), non_empty_env("SHELL").as_deref(), cfg!(windows))
    }

    /// The shell for the configured executable, else the platform's default.
    pub fn resolve(configured: Option<&str>, env_shell: Option<&str>, windows: bool) -> PaneShell {
        let chosen = configured.or(if windows { None } else { env_shell });
        match chosen {
            Some(exe) => PaneShell::from_exe(exe),
            None if windows => PaneShell::PowerShell,
            None => PaneShell::Posix,
        }
    }

    /// The shell an executable is, by its file name without directory or `.exe`.
    pub fn from_exe(exe: &str) -> PaneShell {
        let name = exe.rsplit(['/', '\\']).next().unwrap_or(exe).to_ascii_lowercase();
        match name.strip_suffix(".exe").unwrap_or(&name) {
            "pwsh" | "powershell" => PaneShell::PowerShell,
            "cmd" => PaneShell::Cmd,
            _ => PaneShell::Posix,
        }
    }
}

/// herdr's config file, `config.toml` in [`herdr_config_dir`].
fn config_path(home: &Path) -> Option<PathBuf> {
    herdr_config_dir(home).map(|dir| dir.join("config.toml"))
}

/// herdr's configuration directory, by herdr's own rule (`config::config_dir`):
/// `$XDG_CONFIG_HOME/herdr` when that is set, else `%APPDATA%\herdr` on Windows and
/// `~/.config/herdr` elsewhere.
pub fn herdr_config_dir(home: &Path) -> Option<PathBuf> {
    if let Some(xdg) = non_empty_env("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("herdr"));
    }
    if cfg!(windows) {
        return non_empty_env("APPDATA").map(|dir| PathBuf::from(dir).join("herdr"));
    }
    Some(home.join(".config").join("herdr"))
}

/// The `default_shell` set in a config.toml's `[terminal]` table, when it is set and non-empty.
pub fn configured_shell(config: &str) -> Option<String> {
    let mut in_terminal = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_terminal = line == "[terminal]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        if !in_terminal || key.trim() != "default_shell" {
            continue;
        }
        let value = value.trim();
        let shell = if let Some(rest) = value.strip_prefix('"') {
            rest.split('"').next().unwrap_or("").replace("\\\\", "\\")
        } else if let Some(rest) = value.strip_prefix('\'') {
            rest.split('\'').next().unwrap_or("").to_string()
        } else {
            value.split('#').next().unwrap_or("").to_string()
        };
        return Some(shell.trim().to_string()).filter(|s| !s.is_empty());
    }
    None
}

/// The argument list as one line for `shell`, every argument quoted, so a path with spaces or
/// quotes arrives as one argument. An argument with a control character is refused: the line
/// would end early in the pane.
pub fn shell_line(argv: &[String], shell: PaneShell) -> Result<String, String> {
    let mut parts = Vec::with_capacity(argv.len());
    for arg in argv {
        if arg.chars().any(char::is_control) {
            return Err(format!("'{}' has a control character, which a shell line cannot carry", arg.escape_debug()));
        }
        parts.push(match shell {
            PaneShell::Posix => shell_quote(arg),
            PaneShell::PowerShell => powershell_quote(arg),
            PaneShell::Cmd => cmd_quote(arg)?,
        });
    }
    // PowerShell runs a quoted program only after the call operator `&`.
    if shell == PaneShell::PowerShell {
        if let Some(program) = parts.first_mut() {
            *program = format!("& {program}");
        }
    }
    Ok(parts.join(" "))
}

/// One argument for a POSIX shell: bare when it is plain, else single-quoted.
pub fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c));
    if plain { arg.to_string() } else { format!("'{}'", arg.replace('\'', r"'\''")) }
}

/// One argument for PowerShell, single-quoted. PowerShell also reads the typographic single quotes
/// as quotes, so each of those is doubled too.
pub fn powershell_quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for c in arg.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// One argument for cmd.exe typed at its prompt. A double quote cannot be carried by cmd's quote
/// handling, so it is refused. Characters cmd reads as operators or expansions are escaped with `^`
/// outside the quotes. Trailing backslashes are doubled, so the program's argument parser does not
/// read the closing quote as escaped.
pub fn cmd_quote(arg: &str) -> Result<String, String> {
    if arg.contains('"') {
        return Err(format!("'{arg}' has a double quote, which cmd.exe cannot carry"));
    }
    let special = |c: char| c.is_whitespace() || "%!^&|<>()".contains(c);
    if !arg.is_empty() && !arg.chars().any(special) {
        return Ok(arg.to_string());
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        if "%!^&|<>()".contains(c) {
            out.push_str(&format!("\"^{c}\""));
        } else {
            out.push(c);
        }
    }
    out.push_str(&"\\".repeat(arg.len() - arg.trim_end_matches('\\').len()));
    out.push('"');
    Ok(out)
}

/// A pane, the label of its tab, and the argument lists of its foreground processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneProcs {
    pub pane_id: String,
    pub tab_id: String,
    /// The tab's label; empty when herdr did not give one.
    pub tab_label: String,
    pub argvs: Vec<Vec<String>>,
}

/// The suffix a session's tab label ends with. sesync's tabs carry it, so a session stays findable
/// when herdr reports no argument lists for its pane (Windows).
pub fn tag_suffix(tag: &str) -> String {
    if tag.is_empty() { String::new() } else { format!(" · {tag}") }
}

/// Whether one of these foreground argument lists has `ident` as a flag's value. Nothing matches an
/// empty identifier. The phone bridge uses the same rule to find a conversation's panes.
pub fn argv_carries(argvs: &[Vec<String>], ident: &str) -> bool {
    !ident.is_empty()
        && argvs.iter().any(|argv| argv.windows(2).any(|pair| pair[0].starts_with('-') && pair[1] == ident))
}

/// The pane whose foreground process was started with `ident` as a flag's value. When no pane
/// matches that way, a pane with no foreground argument list whose tab is labelled with `tag` is
/// the session. Nothing matches an empty identifier.
pub fn find_open_pane<'a>(panes: &'a [PaneProcs], ident: &str, tag: &str) -> Option<&'a PaneProcs> {
    panes.iter().find(|pane| argv_carries(&pane.argvs, ident)).or_else(|| {
        let suffix = tag_suffix(tag);
        if suffix.is_empty() {
            return None;
        }
        panes.iter().find(|pane| pane.argvs.is_empty() && pane.tab_label.ends_with(&suffix))
    })
}

/// What [`open`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// A pane already runs this session: its tab is focused and nothing starts.
    Focused { tab_id: String },
    /// The tab sesync opened for this session was idle (after a herdr restart, say): the resume
    /// command was typed into it again.
    Resumed { tab_id: String },
    /// A new tab runs the resume command.
    Created { tab_id: String },
}

impl Opened {
    /// The status line's words for this outcome.
    pub fn message(&self, plan: &Plan) -> String {
        match self {
            Opened::Focused { .. } => format!("'{}' is already open in herdr; focused its tab", plan.label),
            Opened::Resumed { .. } => format!("resumed '{}' in its herdr tab", plan.label),
            Opened::Created { .. } if plan.cwd_fallback => format!(
                "opened '{}' in a new herdr tab, in {} (the session's own directory is unknown or gone)",
                plan.label,
                plan.cwd.display()
            ),
            Opened::Created { .. } => format!("opened '{}' in a new herdr tab, in {}", plan.label, plan.cwd.display()),
        }
    }
}

/// How [`open`] tells whether a pane runs the session.
pub enum Detect<'a> {
    /// herdr's process info shows the programs running in a pane's shell (Linux, macOS).
    Programs,
    /// herdr shows only the shell (Windows): a pane runs something when this says a program runs
    /// below its shell's pid (`proc_tree::program_running_under`); None when it cannot tell.
    ShellOnly(&'a dyn Fn(u32) -> Option<bool>),
}

impl Detect<'static> {
    /// This platform's way.
    pub fn platform() -> Detect<'static> {
        if cfg!(windows) { Detect::ShellOnly(&crate::proc_tree::program_running_under) } else { Detect::Programs }
    }
}

/// Focus the tab already running the session, or open a tab that resumes it. Any failure before a
/// tab exists changes nothing; a failure to start the command closes the tab it made.
///
/// Every herdr call is a process of its own (about half a second on Windows), so only what can
/// answer is asked: the tab list for the tab sesync labelled with the session's tag, then the
/// processes of the panes that can be the session (`Detect`). Where herdr shows programs, those are
/// the tagged tab's panes and the panes in the session's project directory; where it shows only the
/// shell, the tagged tab's panes, and an idle one gets the command again instead of a second tab.
/// A new tab is focused after its command is typed, so keys pressed meanwhile never reach its shell.
pub fn open(herdr: &Herdr, plan: &Plan, shell: PaneShell, detect: Detect) -> Result<Opened, String> {
    open_with_focus(herdr, plan, shell, detect, true)
}

/// [`open`], and with `focus` false the tab herdr shows is left where it is: a phone-initiated resume must not move
/// the desktop's focus. Nothing else differs.
pub fn open_with_focus(herdr: &Herdr, plan: &Plan, shell: PaneShell, detect: Detect, focus: bool) -> Result<Opened, String> {
    let tab_labels = herdr
        .tab_labels()
        .map_err(|e| format!("herdr is not reachable ({e}); open sesync from a herdr popup while herdr runs"))?;
    let suffix = tag_suffix(&plan.tag);
    let tagged = tab_labels.iter().find(|(_, label)| !suffix.is_empty() && label.ends_with(&suffix)).map(|(tab, _)| tab.clone());
    let command = shell_line(&plan.argv, shell)?;
    match detect {
        Detect::Programs => {
            let panes = herdr.panes().map_err(|e| format!("could not read herdr's panes ({e})"))?;
            let mut procs = Vec::new();
            for pane in panes {
                let in_tagged_tab = tagged.as_deref() == Some(pane.tab_id.as_str());
                let in_project = pane.cwd.as_deref().is_some_and(|cwd| Path::new(cwd) == plan.cwd);
                if !in_tagged_tab && !in_project {
                    continue;
                }
                // Fails closed: a pane whose processes cannot be read might be the session, so no
                // second copy is started.
                let argvs = herdr
                    .foreground_argvs(&pane.pane_id)
                    .map_err(|e| format!("could not read the processes of pane {}: {e}", pane.pane_id))?;
                let tab_label = tab_labels.iter().find(|(tab, _)| *tab == pane.tab_id).map(|(_, label)| label.clone()).unwrap_or_default();
                procs.push(PaneProcs { pane_id: pane.pane_id, tab_id: pane.tab_id, tab_label, argvs });
            }
            if let Some(pane) = find_open_pane(&procs, plan.ident(), &plan.tag) {
                if focus {
                    herdr.focus_tab(&pane.tab_id)?;
                }
                return Ok(Opened::Focused { tab_id: pane.tab_id.clone() });
            }
        }
        Detect::ShellOnly(program_running_under) => {
            if let Some(tab_id) = tagged {
                let panes = herdr.panes().map_err(|e| format!("could not read herdr's panes ({e})"))?;
                let mut idle = None;
                for pane in panes.into_iter().filter(|p| p.tab_id == tab_id) {
                    let pid = herdr
                        .shell_pid(&pane.pane_id)
                        .map_err(|e| format!("could not read the processes of pane {}: {e}", pane.pane_id))?;
                    // Fails closed: unknown means it might be the session running.
                    match pid.and_then(program_running_under) {
                        Some(false) => idle = idle.or(Some(pane.pane_id)),
                        _ => {
                            if focus {
                                herdr.focus_tab(&tab_id)?;
                            }
                            return Ok(Opened::Focused { tab_id });
                        }
                    }
                }
                if let Some(pane_id) = idle {
                    refuse_second_agent(plan)?;
                    herdr
                        .run_in_pane(&pane_id, &command)
                        .map_err(|e| format!("could not start the resume command in herdr: {e}"))?;
                    if focus {
                        herdr.focus_tab(&tab_id)?;
                    }
                    return Ok(Opened::Resumed { tab_id });
                }
            }
        }
    }
    refuse_second_agent(plan)?;
    let (tab_id, pane_id) = herdr.create_tab(&plan.cwd, &plan.label)?;
    if let Err(e) = herdr.run_in_pane(&pane_id, &command) {
        let _ = herdr.close_tab(&tab_id);
        return Err(format!("could not start the resume command in herdr: {e}"));
    }
    if focus {
        herdr
            .focus_tab(&tab_id)
            .map_err(|e| format!("opened '{}' in a new herdr tab, but could not switch to it ({e})", plan.label))?;
    }
    Ok(Opened::Created { tab_id })
}

/// Refuse to start `plan`'s agent while a local process already runs its session, in herdr or not: a second agent
/// splits the conversation (omp forks the session into a new file). Reached only when no herdr pane was found for it.
pub fn refuse_second_agent(plan: &Plan) -> Result<(), String> {
    match crate::bridge::herdr::agent_running(plan.ident()) {
        Some(pid) => Err(format!(
            "'{}' is already running (pid {pid}) outside a herdr tab sesync can see; switch to that terminal, or quit it there before opening it again",
            plan.label
        )),
        None => Ok(()),
    }
}
