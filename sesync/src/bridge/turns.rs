//! Claims for prompts the phone submits. A submitted prompt is a turn: before the text goes in, the
//! bridge claims the conversation in sesync-lock, as a harness turn does, so the TUI, the daemon and
//! the other harnesses cannot write the same conversation at once. The claim is released when the
//! agent reports its turn over.
//!
//! A conversation the lock tool refuses is named back to the phone with its holder. A claim is
//! dropped on its own when the agent never starts working (the prompt went nowhere) or when a turn
//! runs past the limit, so a stuck pane cannot lock a conversation forever.

use super::herdr::{self, Herdr};
use crate::lock::{self, ClaimOutcome};
use std::collections::HashSet;
use std::path::PathBuf;
use parking_lot::Mutex;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// The harness name the phone's claims carry. A claim held by the bridge names this and its pid.
pub const HARNESS: &str = "phone";

/// Where sesync-lock is and where its claims live: the same values the TUI and the daemon use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockTools {
    pub sesync_lock: PathBuf,
    pub lock_dir: PathBuf,
}

/// How a turn's claim is watched. The defaults are the production values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// How long the agent may take to start working after a prompt before its claim is dropped.
    pub grace: Duration,
    /// The longest a turn may hold its claim.
    pub limit: Duration,
    /// How often the agent's status is read.
    pub poll: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing { grace: Duration::from_secs(15), limit: Duration::from_secs(30 * 60), poll: Duration::from_millis(500) }
    }
}

/// The conversations this bridge currently holds a turn claim on.
#[derive(Clone, Default)]
pub struct Turns {
    held: Arc<Mutex<HashSet<String>>>,
}

impl Turns {
    /// Claim `conv` for a turn in the pane `target`, or say who holds it. A conversation this bridge
    /// already has a turn in is refused too: one prompt runs at a time.
    pub fn begin(&self, tools: &LockTools, conv: &str, target: &str) -> Result<(), String> {
        let mut held = self.held.lock();
        if held.contains(conv) {
            return Err(format!(
                "conversation is in use by {HARNESS} {}: a prompt from this phone is still running",
                std::process::id()
            ));
        }
        match lock::claim(&tools.sesync_lock, Some(&tools.lock_dir), conv, HARNESS, std::process::id(), target) {
            ClaimOutcome::Ran(()) => {
                held.insert(conv.to_string());
                Ok(())
            }
            ClaimOutcome::Skipped(holder) => {
                Err(format!("conversation is in use by {holder}; try again when its turn ends"))
            }
            ClaimOutcome::Failed(reason) => Err(format!("lock tool failed, the prompt was not sent: {reason}")),
        }
    }

    /// Release `conv`'s claim and forget the turn.
    pub fn end(&self, tools: &LockTools, conv: &str) {
        lock::release(&tools.sesync_lock, Some(&tools.lock_dir), conv, HARNESS, std::process::id());
        self.held.lock().remove(conv);
    }

    /// Hold the claim on `conv` until the agent in `pane_id` has worked and returned to idle or done,
    /// or the grace period or the limit runs out, or the pane is gone. Then release it.
    pub fn watch(&self, tools: LockTools, herdr: Arc<dyn Herdr>, conv: String, pane_id: String) {
        self.watch_with(Timing::default(), tools, herdr, conv, pane_id);
    }

    pub fn watch_with(&self, timing: Timing, tools: LockTools, herdr: Arc<dyn Herdr>, conv: String, pane_id: String) {
        let turns = self.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let mut worked = false;
            loop {
                thread::sleep(timing.poll);
                let finished = match herdr::pane_info(herdr.as_ref(), &pane_id) {
                    Ok(pane) => match pane.agent_status.as_str() {
                        "working" | "blocked" => {
                            worked = true;
                            false
                        }
                        // Back to idle or done after a turn that worked: the turn is over.
                        "idle" | "done" if worked => true,
                        // Nothing started: once the grace period is up the prompt went nowhere.
                        _ => started.elapsed() >= timing.grace,
                    },
                    Err(_) => true,
                };
                if finished || started.elapsed() >= timing.limit {
                    break;
                }
            }
            turns.end(&tools, &conv);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A sesync-lock stand-in that logs its arguments and answers as the real one does: a claim on a
    /// held conversation exits 2 and names the holder.
    fn fake_lock(dir: &std::path::Path, holder: Option<&str>) -> PathBuf {
        let bin = dir.join("sesync-lock");
        let log = dir.join("lock.log");
        let holder_line = holder.unwrap_or("");
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> '{log}'\ncase \"$1\" in\n  claim) if [ -n '{holder_line}' ]; then echo '{holder_line}'; exit 2; fi; echo \"phone $4\"; exit 0;;\n  release) exit 0;;\nesac\nexit 1\n",
            log = log.display(),
        );
        crate::fake_cli::stage(&bin, &script);
        bin
    }

    /// Answers `pane.get` with the next status in the list, repeating the last one.
    struct Statuses {
        list: Vec<&'static str>,
        calls: AtomicUsize,
    }

    impl Herdr for Statuses {
        fn call(&self, method: &str, _params: Value) -> Result<Value, String> {
            assert_eq!(method, "pane.get");
            let i = self.calls.fetch_add(1, Ordering::Relaxed).min(self.list.len() - 1);
            Ok(json!({ "type": "pane_info", "pane": {
                "pane_id": "w1:p1", "terminal_id": "term_a", "tab_id": "w1:t1", "agent_status": self.list[i]
            }}))
        }
    }

    fn sandbox(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sesync-turns-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn log(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("lock.log")).unwrap_or_default()
    }

    fn wait_for(dir: &std::path::Path, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !log(dir).contains(needle) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        log(dir).contains(needle)
    }

    const FAST: Timing = Timing {
        grace: Duration::from_millis(300),
        limit: Duration::from_secs(10),
        poll: Duration::from_millis(20),
    };

    #[test]
    fn a_free_conversation_is_claimed_and_a_held_one_names_its_holder() {
        let dir = sandbox("claim");
        let tools = LockTools { sesync_lock: fake_lock(&dir, None), lock_dir: dir.clone() };
        let turns = Turns::default();
        turns.begin(&tools, "abc", "term_a").unwrap();
        assert!(log(&dir).contains("claim abc phone "), "the claim names the phone: {}", log(&dir));
        let again = turns.begin(&tools, "abc", "term_a").unwrap_err();
        assert!(again.contains("still running"), "one prompt at a time: {again}");
        turns.end(&tools, "abc");
        assert!(log(&dir).contains("release abc phone "));

        let busy = LockTools { sesync_lock: fake_lock(&dir, Some("claude 4242")), lock_dir: dir.clone() };
        let refused = Turns::default().begin(&busy, "abc", "term_a").unwrap_err();
        assert!(refused.contains("claude 4242"), "the holder is named: {refused}");
    }

    #[test]
    fn a_turn_releases_when_the_agent_returns_to_idle_after_working() {
        let dir = sandbox("release");
        let tools = LockTools { sesync_lock: fake_lock(&dir, None), lock_dir: dir.clone() };
        let turns = Turns::default();
        turns.begin(&tools, "conv1", "w1:p1").unwrap();
        let herdr: Arc<dyn Herdr> = Arc::new(Statuses {
            list: vec!["unknown", "working", "working", "done"],
            calls: AtomicUsize::new(0),
        });
        turns.watch_with(FAST, tools.clone(), herdr, "conv1".into(), "w1:p1".into());
        assert!(wait_for(&dir, "release conv1"), "released after working then done");
        assert!(turns.begin(&tools, "conv1", "w1:p1").is_ok(), "the conversation is free again");
    }

    #[test]
    fn a_prompt_the_agent_never_acts_on_releases_after_the_grace_period() {
        let dir = sandbox("grace");
        let tools = LockTools { sesync_lock: fake_lock(&dir, None), lock_dir: dir.clone() };
        let turns = Turns::default();
        turns.begin(&tools, "conv2", "w1:p1").unwrap();
        let herdr: Arc<dyn Herdr> = Arc::new(Statuses { list: vec!["idle"], calls: AtomicUsize::new(0) });
        turns.watch_with(FAST, tools.clone(), herdr, "conv2".into(), "w1:p1".into());
        assert!(wait_for(&dir, "release conv2"), "an idle agent that never worked releases the claim");
    }

    #[test]
    fn a_turn_past_its_limit_releases_even_while_working() {
        let dir = sandbox("limit");
        let tools = LockTools { sesync_lock: fake_lock(&dir, None), lock_dir: dir.clone() };
        let turns = Turns::default();
        turns.begin(&tools, "conv3", "w1:p1").unwrap();
        let herdr: Arc<dyn Herdr> = Arc::new(Statuses { list: vec!["working"], calls: AtomicUsize::new(0) });
        let timing = Timing { limit: Duration::from_millis(200), ..FAST };
        turns.watch_with(timing, tools.clone(), herdr, "conv3".into(), "w1:p1".into());
        assert!(wait_for(&dir, "release conv3"), "a stuck turn cannot hold the conversation forever");
    }
}
