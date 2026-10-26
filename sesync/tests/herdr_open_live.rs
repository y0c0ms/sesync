//! Live check against a running herdr. Ignored by default: it needs herdr's server and creates one
//! tab in the active workspace, which it closes again (also when an assertion fails).
//!
//! Run inside or next to a running herdr:
//!   cargo test --test herdr_open_live -- --ignored --nocapture
//!
//! The tab runs `tail -f` on a file in a temporary directory, a harmless stand-in for a harness.
#![cfg(unix)]

use sesync::herdr_open::{self, Herdr, Opened, Plan, Process};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

/// One herdr CLI call through the real runner, parsed.
fn herdr_json(args: &[&str]) -> Value {
    let bin = std::env::var("HERDR_BIN_PATH").ok().filter(|b| !b.is_empty()).unwrap_or_else(|| "herdr".to_string());
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let text = herdr_open::Runner::run(&Process, &bin, &args).expect("herdr answers");
    serde_json::from_str(text.trim()).expect("herdr answers JSON")
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

#[test]
#[ignore = "needs a running herdr; opens and closes one tab"]
fn a_session_opens_in_its_directory_runs_and_a_second_open_focuses_the_same_tab() {
    let base = std::env::temp_dir().join(format!("sesync herdr live {}", std::process::id()));
    let project = base.join("project");
    fs::create_dir_all(&project).unwrap();
    let session = base.join("session file.jsonl");
    fs::write(&session, "").unwrap();

    let label = herdr_open::tab_label("smoke", "live");
    let plan = Plan {
        argv: vec!["tail".into(), "-f".into(), session.display().to_string()],
        cwd: project.clone(),
        cwd_fallback: false,
        label: label.clone(),
    };
    let herdr = Herdr::from_env(Box::new(Process));
    let tabs_before = tab_labels().len();

    let opened = herdr_open::open(&herdr, &plan).expect("the first open creates a tab");
    let Opened::Created { tab_id } = opened.clone() else {
        panic!("the first open should create a tab, got {opened:?}");
    };
    let mut guard = CloseTab { herdr: Herdr::from_env(Box::new(Process)), tab: Some(tab_id.clone()) };
    println!("created tab {tab_id}: {}", opened.message(&plan));

    // The tab carries the label, and its root pane starts in the session's directory.
    let tabs = tab_labels();
    assert!(tabs.iter().any(|(t, l)| t == &tab_id && l == &label), "tab {tab_id} is labelled '{label}': {tabs:?}");
    let pane = herdr.panes().unwrap().into_iter().find(|p| p.tab_id == tab_id).expect("the new tab has a pane");
    let panes = herdr_json(&["pane", "list"]);
    let cwd = panes["result"]["panes"].as_array().unwrap().iter()
        .find(|p| p["pane_id"] == pane.pane_id.as_str())
        .and_then(|p| p["cwd"].as_str())
        .unwrap_or_default()
        .to_string();
    assert_eq!(cwd, project.display().to_string(), "the pane's directory");

    // The resume command was typed in and is now the pane's foreground process.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let argvs = herdr.foreground_argvs(&pane.pane_id).unwrap();
        if argvs.iter().any(|a| a == &plan.argv) {
            println!("foreground of {}: {argvs:?}", pane.pane_id);
            break;
        }
        assert!(Instant::now() < deadline, "the command never ran; foreground was {argvs:?}");
        std::thread::sleep(Duration::from_millis(200));
    }

    // A second open finds the running pane and focuses its tab; no tab is added.
    let again = herdr_open::open(&herdr, &plan).expect("the second open focuses");
    assert_eq!(again, Opened::Focused { tab_id: tab_id.clone() });
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
