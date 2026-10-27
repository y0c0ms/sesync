//! Live check against a running herdr. Ignored by default: it needs herdr's server and creates one
//! tab in the active workspace, which it closes again (also when an assertion fails).
//!
//! Run inside or next to a running herdr (point HERDR_SESSION at a throwaway session to keep your
//! own tabs out of it):
//!   cargo test --test herdr_open_live -- --ignored --nocapture
//!
//! The tab runs a harmless stand-in for a harness: `tail -f` on a file on Linux and macOS, a long
//! `ping` on Windows. On Windows herdr shows only a pane's shell, so the open must not depend on the
//! number of panes: the herdr calls are counted.

use sesync::herdr_open::{self, Herdr, Opened, Plan, Process, Runner};
use serde_json::Value;
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// One herdr CLI call through the real runner, parsed.
fn herdr_json(args: &[&str]) -> Value {
    let bin = std::env::var("HERDR_BIN_PATH").ok().filter(|b| !b.is_empty()).unwrap_or_else(|| "herdr".to_string());
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let text = Process.run(&bin, &args).expect("herdr answers");
    serde_json::from_str(text.trim()).expect("herdr answers JSON")
}

/// The real runner, counting its calls.
struct Counting(Rc<Cell<usize>>);

impl Runner for Counting {
    fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
        self.0.set(self.0.get() + 1);
        Process.run(program, args)
    }
}

/// Closes its tab on drop, so a failed assertion does not leave one behind.
struct CloseTab {
    herdr: Herdr,
    tab: Option<String>,
}

impl Drop for CloseTab {
    fn drop(&mut self) {
        if let Some(tab) = self.tab.take() {
            let _ = self.herdr.close_tab(&tab);
        }
    }
}

fn tab_labels() -> Vec<(String, String)> {
    herdr_json(&["tab", "list"])["result"]["tabs"]
        .as_array()
        .expect("tab list")
        .iter()
        .map(|t| (t["tab_id"].as_str().unwrap_or_default().to_string(), t["label"].as_str().unwrap_or_default().to_string()))
        .collect()
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).expect("a home directory"))
}

/// What a pane shows, as text.
fn screen(pane_id: &str) -> String {
    let bin = std::env::var("HERDR_BIN_PATH").ok().filter(|b| !b.is_empty()).unwrap_or_else(|| "herdr".to_string());
    let out = std::process::Command::new(bin).args(["pane", "read", pane_id, "--source", "recent"]).output().expect("pane read");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[ignore = "needs a running herdr; opens and closes one tab"]
fn a_session_opens_in_its_directory_runs_and_a_second_open_focuses_the_same_tab() {
    let base = std::env::temp_dir().join(format!("sesync herdr live {}", std::process::id()));
    let project = base.join("project");
    fs::create_dir_all(&project).unwrap();
    let session = base.join("session file.jsonl");
    fs::write(&session, "").unwrap();

    let tag = herdr_open::session_tag(&session.display().to_string());
    let label = herdr_open::tab_label("smoke", "live", &tag);
    let argv: Vec<String> = if cfg!(windows) {
        ["ping", "-n", "1000", "127.0.0.1"].iter().map(|a| a.to_string()).collect()
    } else {
        vec!["tail".into(), "-f".into(), session.display().to_string()]
    };
    let plan = Plan { argv, cwd: project.clone(), cwd_fallback: false, label: label.clone(), tag };
    let shell = herdr_open::PaneShell::current(&home());
    let calls = Rc::new(Cell::new(0));
    let herdr = Herdr::from_env(Box::new(Counting(Rc::clone(&calls))));
    let shows_programs = !cfg!(windows);
    let tabs_before = tab_labels().len();

    let started = Instant::now();
    let opened = herdr_open::open(&herdr, &plan, shell, herdr_open::Detect::platform()).expect("the first open creates a tab");
    println!("first open: {:?}, {} herdr calls", started.elapsed(), calls.get());
    let Opened::Created { tab_id } = opened.clone() else {
        panic!("the first open should create a tab, got {opened:?}");
    };
    let mut guard = CloseTab { herdr: Herdr::from_env(Box::new(Process)), tab: Some(tab_id.clone()) };
    if !shows_programs {
        assert_eq!(calls.get(), 4, "tab list, tab create, pane run, tab focus, whatever the number of panes");
    }

    // The tab carries the label, is focused, and its root pane starts in the session's directory.
    let tabs = tab_labels();
    assert!(tabs.iter().any(|(t, l)| t == &tab_id && l == &label), "tab {tab_id} is labelled '{label}': {tabs:?}");
    let pane = herdr.panes().unwrap().into_iter().find(|p| p.tab_id == tab_id).expect("the new tab has a pane");
    let cwd = pane.cwd.clone().unwrap_or_default();
    assert_eq!(Path::new(&cwd), project.as_path(), "the pane's directory");

    // The command was typed in, and nothing else on its line.
    let command = herdr_open::shell_line(&plan.argv, shell).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let ran = if shows_programs {
            herdr.foreground_argvs(&pane.pane_id).unwrap().iter().any(|a| a == &plan.argv)
        } else {
            screen(&pane.pane_id).contains(&command)
        };
        if ran {
            break;
        }
        assert!(Instant::now() < deadline, "the command never ran in pane {}", pane.pane_id);
        std::thread::sleep(Duration::from_millis(200));
    }
    if !shows_programs {
        let screen = screen(&pane.pane_id);
        let line = screen.lines().find(|l| l.contains(&command)).unwrap_or_else(|| panic!("'{command}' is on screen:\n{screen}"));
        assert!(line.trim_end().ends_with(&command), "the typed line is the command: {line:?}");
        // `exitomp --resume …` was the bug: the character right before the command must not be
        // part of another word.
        let before = &line[..line.find(&command).unwrap()];
        assert!(!before.ends_with(|c: char| c.is_alphanumeric()), "nothing is glued in front of the command: {line:?}");
    }

    // A second open finds the session's tab and focuses it; no tab is added.
    calls.set(0);
    let again = herdr_open::open(&herdr, &plan, shell, herdr_open::Detect::platform()).expect("the second open focuses");
    assert_eq!(again, Opened::Focused { tab_id: tab_id.clone() });
    if !shows_programs {
        assert_eq!(calls.get(), 4, "tab list, pane list, the tagged pane's process info, tab focus");

        // The program ends (as after a herdr restart, when the tab comes back at its prompt): the
        // next open types the command into the same tab instead of opening another.
        herdr_json(&["pane", "send-keys", &pane.pane_id, "ctrl+c"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        let still_running = || herdr.shell_pid(&pane.pane_id).unwrap().and_then(sesync::proc_tree::program_running_under) == Some(true);
        while still_running() {
            assert!(Instant::now() < deadline, "ping did not stop");
            std::thread::sleep(Duration::from_millis(200));
        }
        let resumed = herdr_open::open(&herdr, &plan, shell, herdr_open::Detect::platform()).expect("the third open resumes");
        assert_eq!(resumed, Opened::Resumed { tab_id: tab_id.clone() });
        let lines = screen(&pane.pane_id).lines().filter(|l| l.contains(&command)).count();
        assert!(lines >= 2, "the command was typed into the same pane again:\n{}", screen(&pane.pane_id));
    }
    let focused = herdr_json(&["tab", "list"])["result"]["tabs"].as_array().unwrap().iter()
        .find(|t| t["tab_id"] == tab_id.as_str())
        .and_then(|t| t["focused"].as_bool());
    assert_eq!(focused, Some(true), "the smoke tab is the focused one");
    assert_eq!(tab_labels().len(), tabs_before + 1, "still exactly one new tab");

    herdr.close_tab(&tab_id).expect("the smoke tab closes");
    guard.tab = None;
    drop(guard);
    assert_eq!(tab_labels().len(), tabs_before, "the smoke tab is gone");
    let _ = fs::remove_dir_all(Path::new(&base));
}
