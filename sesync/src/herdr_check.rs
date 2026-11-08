//! Does a herdr build still do what sesync needs? `sesync herdr check --herdr <exe>` answers it on a
//! throwaway herdr session: it starts that herdr's server headless, with a copy of the user's
//! herdr config, and drives the real "Open in herdr" code (`herdr_open::open`) with a harmless probe
//! (`sesync herdr probe`) in place of an agent. Nothing of the user's herdr is touched.
//!
//! `sesync herdr guard` registers this check with herdr: it writes `update-guard.toml` in herdr's
//! configuration directory, and our herdr build reads it in `herdr update`, warns, and offers to run
//! the check on the downloaded release before installing it (see docs/ARCHITECTURE.md).

use crate::herdr_open::{self, Detect, Herdr, Isolated, Opened, PaneShell, Plan, Runner};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

/// The file in herdr's configuration directory that `herdr update` (our build) reads.
pub const GUARD_FILE: &str = "update-guard.toml";

/// How long the probe waits to be told to stop before it exits on its own.
const PROBE_LIFETIME: Duration = Duration::from_secs(120);

/// What one step of the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub what: String,
    pub ok: bool,
    pub detail: String,
}

/// The guard file's text: herdr runs `check` with the candidate binary appended.
pub fn guard_text(sesync_exe: &Path) -> String {
    let exe = serde_json::to_string(&sesync_exe.display().to_string()).unwrap_or_default();
    format!(
        "# Written by `sesync herdr guard`; `sesync herdr guard --remove` deletes it.\n\
         # herdr update (sesync's herdr build) reads it: it warns that sesync relies on this herdr,\n\
         # and can run the check below on the downloaded release (its herdr binary appended) before\n\
         # installing it. Official herdr builds ignore this file.\n\
         name = \"sesync\"\n\
         check = [{exe}, \"herdr\", \"check\", \"--herdr\"]\n"
    )
}

/// Write the guard into herdr's configuration directory. Returns the file written.
pub fn install_guard(config_dir: &Path, sesync_exe: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(config_dir)?;
    let path = config_dir.join(GUARD_FILE);
    let tmp = config_dir.join(format!("{GUARD_FILE}.tmp"));
    std::fs::write(&tmp, guard_text(sesync_exe))?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Delete the guard. False when there was none.
pub fn remove_guard(config_dir: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(config_dir.join(GUARD_FILE)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// The probe's stop request: the probe exits once this file exists.
pub fn stop_file(probe_file: &Path) -> PathBuf {
    let mut name = probe_file.as_os_str().to_owned();
    name.push(".stop");
    PathBuf::from(name)
}

/// The probe herdr runs in a pane: it records where it runs and what herdr gave it, then stays
/// until asked to stop (so it counts as the session running), at most [`PROBE_LIFETIME`].
pub fn probe(file: &Path) -> i32 {
    let report = serde_json::json!({
        "cwd": std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default(),
        "herdr_bin_path": std::env::var("HERDR_BIN_PATH").unwrap_or_default(),
        "pid": std::process::id(),
    });
    let tmp = stop_file(file).with_extension("tmp");
    if std::fs::write(&tmp, report.to_string()).and_then(|_| std::fs::rename(&tmp, file)).is_err() {
        return 1;
    }
    let stop = stop_file(file);
    let started = Instant::now();
    while !stop.exists() && started.elapsed() < PROBE_LIFETIME {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(&stop);
    0
}

/// A throwaway herdr session: its own name, a copy of the user's config, a headless server. Dropped,
/// it stops the probe and the server and deletes the session and its files.
struct Session {
    bin: String,
    name: String,
    dir: PathBuf,
    runner: Isolated,
    server: Option<Child>,
    probe: PathBuf,
}

impl Session {
    fn call(&self, args: &[&str]) -> Result<Value, String> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let text = self.runner.run(&self.bin, &args)?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        let v: Value = serde_json::from_str(text.trim()).map_err(|_| format!("herdr answered something other than JSON: {}", text.trim()))?;
        match v.get("error") {
            Some(e) => Err(e["message"].as_str().unwrap_or("herdr refused the call").to_string()),
            None => Ok(v),
        }
    }

    fn herdr(&self, workspace: Option<String>) -> Herdr {
        Herdr::new(self.bin.clone(), workspace, Box::new(Isolated { env: self.runner.env.clone() }))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = std::fs::write(stop_file(&self.probe), b"");
        let _ = self.call(&["server", "stop"]);
        if let Some(mut server) = self.server.take() {
            let deadline = Instant::now() + Duration::from_secs(5);
            while matches!(server.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = server.kill();
            let _ = server.wait();
        }
        let _ = self.call(&["session", "delete", &self.name]);
        // The probe may still be running for a moment after its pane went away.
        std::thread::sleep(Duration::from_millis(300));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Run the check against `herdr_bin`. Each step is passed to `report` as it ends; the check stops
/// at the first failure (the later steps need the earlier ones). True when every step passed.
pub fn check(herdr_bin: &Path, sesync_exe: &Path, home: &Path, report: &mut dyn FnMut(&Step)) -> bool {
    let mut steps = Steps { report, ok: true };
    run_steps(herdr_bin, sesync_exe, home, &mut steps);
    steps.ok
}

struct Steps<'a> {
    report: &'a mut dyn FnMut(&Step),
    ok: bool,
}

impl Steps<'_> {
    /// Report a step; false (and the check stops) when it failed.
    fn step(&mut self, what: &str, result: Result<String, String>) -> bool {
        let (ok, detail) = match result {
            Ok(detail) => (true, detail),
            Err(detail) => (false, detail),
        };
        (self.report)(&Step { what: what.to_string(), ok, detail });
        self.ok &= ok;
        ok
    }
}

fn run_steps(herdr_bin: &Path, sesync_exe: &Path, home: &Path, steps: &mut Steps) {
    let bin = herdr_bin.display().to_string();
    let bare = Isolated { env: Vec::new() };
    if !steps.step("runs", bare.run(&bin, &["--version".to_string()]).map(|v| v.trim().to_string())) {
        return;
    }

    // A copy of the user's config: the pane shell and the popup binding are what the user runs.
    let name = format!("sesync-check-{}", std::process::id());
    let dir = std::env::temp_dir().join(&name);
    let _ = std::fs::remove_dir_all(&dir);
    let project = dir.join("project");
    if let Err(e) = std::fs::create_dir_all(&project) {
        steps.step("prepares a scratch folder", Err(e.to_string()));
        return;
    }
    let user_config = std::env::var_os("HERDR_CONFIG_PATH").map(PathBuf::from)
        .or_else(|| herdr_open::herdr_config_dir(home).map(|d| d.join("config.toml")));
    let config_text = user_config.as_deref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let config = dir.join("config.toml");
    if let Err(e) = std::fs::write(&config, &config_text) {
        steps.step("prepares a scratch folder", Err(e.to_string()));
        return;
    }
    let probe_file = dir.join("probe.json");
    let runner = Isolated {
        env: vec![
            ("HERDR_SESSION".to_string(), name.clone()),
            ("HERDR_CONFIG_PATH".to_string(), config.display().to_string()),
        ],
    };
    let mut session = Session { bin: bin.clone(), name, dir, runner, server: None, probe: probe_file.clone() };

    // The server, headless.
    let mut command = session.runner.command(&bin);
    command.arg("server").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    match command.spawn() {
        Ok(child) => session.server = Some(child),
        Err(e) => {
            steps.step("starts a server", Err(e.to_string()));
            return;
        }
    }
    let started = Instant::now();
    let ready = loop {
        match session.call(&["tab", "list"]) {
            Ok(_) => break Ok(format!("in {:.1}s", started.elapsed().as_secs_f32())),
            Err(e) if started.elapsed() > Duration::from_secs(20) => break Err(format!("no answer after 20s: {e}")),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    };
    if !steps.step("starts a server", ready) {
        return;
    }

    let has_popup = config_text.contains("[[keys.command]]") && config_text.contains("sesync");
    let config_ok = session.runner.run(&bin, &["config".to_string(), "check".to_string()])
        .map_err(|e| e.trim().to_string())
        .and_then(|out| if out.contains("config: ok") { Ok(()) } else { Err(out.trim().to_string()) })
        .map(|()| if has_popup { "including the sesync popup binding".to_string() } else { "(it has no sesync popup binding)".to_string() });
    if !steps.step("accepts your config.toml", config_ok) {
        return;
    }

    let workspace = session.call(&["workspace", "create", "--cwd", &project.display().to_string(), "--label", "sesync check", "--no-focus"])
        .and_then(|_| session.call(&["workspace", "list"]))
        .and_then(|v| v["result"]["workspaces"][0]["workspace_id"].as_str().map(str::to_string).ok_or("no workspace listed".to_string()));
    let Some(workspace) = take(steps, "creates a workspace", workspace) else { return };

    // "Open in herdr", for real, with the probe as the session.
    let herdr = session.herdr(Some(workspace));
    let ident = probe_file.display().to_string();
    let tag = herdr_open::session_tag(&ident);
    let label = herdr_open::tab_label("check", "integration probe", &tag);
    let plan = Plan {
        argv: vec![sesync_exe.display().to_string(), "herdr".into(), "probe".into(), "--file".into(), ident],
        cwd: project.clone(),
        cwd_fallback: false,
        label: label.clone(),
        tag,
    };
    let shell = PaneShell::resolve(
        herdr_open::configured_shell(&config_text).as_deref(),
        std::env::var("SHELL").ok().filter(|s| !s.is_empty()).as_deref(),
        cfg!(windows),
    );
    let opened = herdr_open::open(&herdr, &plan, shell, Detect::platform());
    let tab_id = match opened {
        Ok(Opened::Created { tab_id }) => Ok(tab_id),
        Ok(other) => Err(format!("expected a new tab, got {other:?}")),
        Err(e) => Err(e),
    };
    let Some(tab_id) = take(steps, "opens a session in a new tab", tab_id.map(|t| (t.clone(), t)).map(|(t, _)| t)) else { return };

    let Some(seen) = take(steps, "runs the typed resume command", wait_probe(&probe_file)) else { return };
    let cwd = seen["cwd"].as_str().unwrap_or_default();
    let same_dir = same_path(Path::new(cwd), &project);
    if !steps.step("starts it in the session's folder", if same_dir { Ok(cwd.to_string()) } else { Err(format!("ran in {cwd}")) }) {
        return;
    }
    let bin_path = seen["herdr_bin_path"].as_str().unwrap_or_default();
    if !steps.step("tells panes where herdr is (HERDR_BIN_PATH)", if bin_path.is_empty() { Err("not set".into()) } else { Ok(bin_path.to_string()) }) {
        return;
    }
    let labelled = herdr.tab_labels().and_then(|tabs| {
        if tabs.iter().any(|(t, l)| *t == tab_id && *l == label) { Ok(label.clone()) } else { Err(format!("tabs: {tabs:?}")) }
    });
    if !steps.step("keeps the tab's label", labelled) {
        return;
    }

    let again = match herdr_open::open(&herdr, &plan, shell, Detect::platform()) {
        Ok(Opened::Focused { tab_id: t }) if t == tab_id => Ok("focused its tab".to_string()),
        Ok(other) => Err(format!("expected its tab to be focused, got {other:?}")),
        Err(e) => Err(e),
    };
    if !steps.step("finds the running session again", again) {
        return;
    }

    // Windows: a session tab back at its prompt (herdr restarted) gets the command again.
    if cfg!(windows) {
        let _ = std::fs::write(stop_file(&probe_file), b"");
        let pane = herdr.panes().ok().and_then(|p| p.into_iter().find(|p| p.tab_id == tab_id)).map(|p| p.pane_id);
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Some(pane) = &pane {
            let running = herdr.shell_pid(pane).ok().flatten().and_then(crate::proc_tree::program_running_under);
            if running == Some(false) || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = std::fs::remove_file(&probe_file);
        let resumed = match herdr_open::open(&herdr, &plan, shell, Detect::platform()) {
            Ok(Opened::Resumed { tab_id: t }) if t == tab_id => wait_probe(&probe_file).map(|_| "typed into the same tab".to_string()),
            Ok(other) => Err(format!("expected a resume in its tab, got {other:?}")),
            Err(e) => Err(e),
        };
        steps.step("resumes in the same tab after the session ended", resumed);
    }
}

/// Report a step and hand back its value when it passed.
fn take<T>(steps: &mut Steps, what: &str, result: Result<T, String>) -> Option<T> {
    match result {
        Ok(value) => {
            steps.step(what, Ok(String::new()));
            Some(value)
        }
        Err(e) => {
            steps.step(what, Err(e));
            None
        }
    }
}

/// Wait for the probe's report.
fn wait_probe(file: &Path) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(file) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                return Ok(v);
            }
        }
        if Instant::now() > deadline {
            return Err("the command typed into the pane never ran (no probe report after 20s)".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The same folder, however each path is spelled (case and separators on Windows, symlinks).
fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}
