//! The phone bridge's resource budget. A `sesync bridge serve` with no phone connected must stay under
//! 20 MB resident: it runs on a workstation that is also a cluster node, next to the pods.
//! The process is the real binary, started in a sandboxed HOME, and its VmRSS is read from /proc.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// The idle budget, in kilobytes of resident memory.
const IDLE_BUDGET_KB: u64 = 20 * 1024;

fn resident_kb(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("the bridge is running");
    let line = status.lines().find(|l| l.starts_with("VmRSS:")).expect("VmRSS is reported");
    line.split_whitespace().nth(1).and_then(|n| n.parse().ok()).expect("VmRSS is a number of kB")
}

#[test]
fn an_idle_bridge_stays_under_its_resident_memory_budget() {
    let sandbox = std::env::temp_dir().join(format!("sesync-bridge-rss-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    let config = sandbox.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();

    let mut child = Command::new(env!("CARGO_BIN_EXE_sesync"))
        .args([
            "bridge", "serve",
            "--listen", &format!("127.0.0.1:{port}"),
            "--allow", "127.0.0.1/32",
            "--herdr-socket", &sandbox.join("no-herdr.sock").display().to_string(),
            "--no-ntfy", "--no-usage",
        ])
        .env("SESYNC_CONFIG_DIR", &config)
        .env("SESYNC_LOCK", sandbox.join("no-sesync-lock"))
        .env("SESYNC_LOCK_DIR", sandbox.join("locks"))
        .env("HOME", &sandbox)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the sesync binary starts");

    // Wait for the bridge to say it is listening, so the measurement is of a serving process.
    let (tx, rx) = mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let listening = loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(line) if line.contains("listening") => break true,
            Ok(_) => continue,
            Err(_) => break false,
        }
    };
    assert!(listening, "the bridge did not start serving");
    std::thread::sleep(Duration::from_millis(500));

    let rss = resident_kb(child.id());
    let _ = child.kill();
    let _ = child.wait();
    assert!(rss < IDLE_BUDGET_KB, "an idle bridge uses {rss} kB resident; the budget is {IDLE_BUDGET_KB} kB");
    eprintln!("idle bridge resident memory: {rss} kB (budget {IDLE_BUDGET_KB} kB)");
}
