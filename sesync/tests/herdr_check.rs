//! The two contracts of `sesync herdr`: the guard file our herdr build's `herdr update` parses, and
//! the probe the check runs in a pane in place of an agent.

use sesync::herdr_check::{self, GUARD_FILE};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-herdr-check-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// The `check = [...]` line, read the way herdr reads it (a TOML array of basic strings, which is
/// also a JSON array).
fn check_command(text: &str) -> Vec<String> {
    let line = text.lines().find(|l| l.starts_with("check = ")).expect("a check line");
    serde_json::from_str(line.trim_start_matches("check = ")).expect("an array of strings")
}

#[test]
fn the_guard_names_this_sesync_and_herdr_appends_the_candidate() {
    let sb = sandbox("guard");
    let exe = Path::new(r#"C:\Users\some one\AppData\Local\Programs\sesync\bin\sesync "x".exe"#);
    let path = herdr_check::install_guard(&sb, exe).unwrap();
    assert_eq!(path, sb.join(GUARD_FILE));
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.lines().any(|l| l == "name = \"sesync\""));
    assert_eq!(check_command(&text), vec![exe.display().to_string(), "herdr".into(), "check".into(), "--herdr".into()]);
    assert!(text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).all(|l| l.contains(" = ")), "only key = value lines");

    assert!(herdr_check::remove_guard(&sb).unwrap());
    assert!(!sb.join(GUARD_FILE).exists());
    assert!(!herdr_check::remove_guard(&sb).unwrap(), "removing twice is not an error");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_probe_reports_where_it_runs_and_ends_when_asked() {
    let sb = sandbox("probe");
    let file = sb.join("probe.json");
    let worker = {
        let file = file.clone();
        std::thread::spawn(move || herdr_check::probe(&file))
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !file.exists() {
        assert!(Instant::now() < deadline, "no report");
        std::thread::sleep(Duration::from_millis(20));
    }
    let report: serde_json::Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(report["cwd"], std::env::current_dir().unwrap().display().to_string());
    assert_eq!(report["pid"], std::process::id());
    assert!(!worker.is_finished(), "it stays, so it counts as the session running");
    fs::write(herdr_check::stop_file(&file), b"").unwrap();
    assert_eq!(worker.join().unwrap(), 0);
    assert!(!herdr_check::stop_file(&file).exists(), "the stop request is consumed");
    let _ = fs::remove_dir_all(&sb);
}
