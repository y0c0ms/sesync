//! Claims for prompts the phone submits. A submitted prompt is a turn: before the text goes in, the
//! bridge claims the conversation in sesync-lock, as a harness turn does, so the TUI, the daemon and
//! the other harnesses cannot write the same conversation at once. The claim is released when the
//! agent reports its turn over.
//!
//! A conversation the lock tool refuses is named back to the phone with its holder. A claim is
//! dropped on its own when the agent never starts working (the prompt went nowhere) or when a turn
//! runs past the limit, so a stuck pane cannot lock a conversation forever. A prompt the agent has not
//! started working on yet (a model `/switch`, or a turn the provider refused at once) does not stop this
//! phone's next prompt: the new one takes over the claim.

use super::herdr::{self, Herdr};
use crate::lock::{self, ClaimOutcome};
use std::collections::HashMap;
use std::path::PathBuf;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// One of this bridge's turns: which prompt holds the claim, and whether its agent has been seen working.
struct Held {
    turn: u64,
    worked: Arc<AtomicBool>,
}

/// A turn `begin` started, for `watch` and `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Turn(u64);

/// The conversations this bridge currently holds a turn claim on.
#[derive(Clone, Default)]
pub struct Turns {
    held: Arc<Mutex<HashMap<String, Held>>>,
    next: Arc<AtomicU64>,
}

impl Turns {
    /// Claim `conv` for a turn in the pane `target`, or say who holds it. A conversation whose turn from this
    /// bridge is running (its agent has worked) is refused too: one prompt runs at a time. One whose prompt the
    /// agent has not started on is taken over by this one, under the claim already held.
    pub fn begin(&self, tools: &LockTools, conv: &str, target: &str) -> Result<Turn, String> {
        let mut held = self.held.lock();
        let turn = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(h) = held.get(conv) {
            if h.worked.load(Ordering::Relaxed) {
                return Err(format!(
                    "conversation is in use by {HARNESS} {}: a prompt from this phone is still running",
                    std::process::id()
                ));
            }
            held.insert(conv.to_string(), Held { turn, worked: Arc::new(AtomicBool::new(false)) });
            return Ok(Turn(turn));
        }
        match lock::claim(&tools.sesync_lock, Some(&tools.lock_dir), conv, HARNESS, std::process::id(), target) {
            ClaimOutcome::Ran(()) => {
                held.insert(conv.to_string(), Held { turn, worked: Arc::new(AtomicBool::new(false)) });
                Ok(Turn(turn))
            }
            ClaimOutcome::Skipped(holder) => {
                Err(format!("conversation is in use by {holder}; try again when its turn ends"))
            }
            ClaimOutcome::Failed(reason) => Err(format!("lock tool failed, the prompt was not sent: {reason}")),
        }
    }

    /// Release `conv`'s claim and forget the turn, unless a later prompt has taken the claim over.
    pub fn end(&self, tools: &LockTools, conv: &str, turn: Turn) {
        let mut held = self.held.lock();
        if held.get(conv).is_some_and(|h| h.turn == turn.0) {
            held.remove(conv);
            lock::release(&tools.sesync_lock, Some(&tools.lock_dir), conv, HARNESS, std::process::id());
        }
    }

    /// Hold the claim on `conv` until the agent in `pane_id` has worked and returned to idle or done,
    /// or the grace period or the limit runs out, or the pane is gone, or a later prompt took the claim
    /// over. Then release it (`end`).
    pub fn watch(&self, tools: LockTools, herdr: Arc<dyn Herdr>, conv: String, pane_id: String, turn: Turn) {
        self.watch_with(Timing::default(), tools, herdr, conv, pane_id, turn);
    }

    pub fn watch_with(&self, timing: Timing, tools: LockTools, herdr: Arc<dyn Herdr>, conv: String, pane_id: String, turn: Turn) {
        let Some(flag) = self.held.lock().get(&conv).filter(|h| h.turn == turn.0).map(|h| Arc::clone(&h.worked)) else {
            return;
        };
        let turns = self.clone();
        thread::spawn(move || {
            let started = Instant::now();
            loop {
                thread::sleep(timing.poll);
                if !turns.held.lock().get(&conv).is_some_and(|h| h.turn == turn.0) {
                    return; // a later prompt took the claim over and watches it
                }
                let finished = match herdr::pane_info(herdr.as_ref(), &pane_id) {
                    Ok(pane) => match pane.agent_status.as_str() {
                        "working" | "blocked" => {
                            flag.store(true, Ordering::Relaxed);
                            false
                        }
                        // Back to idle or done after a turn that worked: the turn is over.
                        "idle" | "done" if flag.load(Ordering::Relaxed) => true,
                        // Nothing started: once the grace period is up the prompt went nowhere.
                        _ => started.elapsed() >= timing.grace,
                    },
                    Err(_) => true,
                };
                if finished || started.elapsed() >= timing.limit {
                    break;
                }
            }
            turns.end(&tools, &conv, turn);
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
        let turn = turns.begin(&tools, "abc", "term_a").unwrap();
        assert!(log(&dir).contains("claim abc phone "), "the claim names the phone: {}", log(&dir));
        turns.end(&tools, "abc", turn);
        assert!(log(&dir).contains("release abc phone "));

        let busy = LockTools { sesync_lock: fake_lock(&dir, Some("claude 4242")), lock_dir: dir.clone() };
        let refused = Turns::default().begin(&busy, "abc", "term_a").unwrap_err();
        assert!(refused.contains("claude 4242"), "the holder is named: {refused}");
    }

    #[test]
    fn a_running_turn_refuses_the_next_prompt_and_an_unstarted_one_is_taken_over() {
        let dir = sandbox("takeover");
        let tools = LockTools { sesync_lock: fake_lock(&dir, None), lock_dir: dir.clone() };
        let turns = Turns::default();
        // A `/switch`: the agent never works. The next prompt takes the claim over at once, without a second claim.
        let first = turns.begin(&tools, "c", "w1:p1").unwrap();
        let idle: Arc<dyn Herdr> = Arc::new(Statuses { list: vec!["idle"], calls: AtomicUsize::new(0) });
        turns.watch_with(Timing { grace: Duration::from_secs(5), ..FAST }, tools.clone(), idle, "c".into(), "w1:p1".into(), first);
        let second = turns.begin(&tools, "c", "w1:p1").expect("an unstarted prompt does not block the next");
        assert_eq!(log(&dir).matches("claim c phone").count(), 1, "the claim is kept, not taken twice");
        // The first watcher stands down: its end must not release the second turn's claim.
        turns.end(&tools, "c", first);
        assert!(!log(&dir).contains("release c"), "a superseded turn does not release: {}", log(&dir));

        let working: Arc<dyn Herdr> = Arc::new(Statuses { list: vec!["working"], calls: AtomicUsize::new(0) });
        turns.watch_with(FAST, tools.clone(), working, "c".into(), "w1:p1".into(), second);
        thread::sleep(Duration::from_millis(150));
        let refused = turns.begin(&tools, "c", "w1:p1").unwrap_err();
        assert!(refused.contains("still running"), "one running prompt at a time: {refused}");
        turns.end(&tools, "c", second);
        assert!(log(&dir).contains("release c phone "));
    }

    #[test]
    fn a_turn_releases_when_the_agent_returns_to_idle_after_working() {
        let dir = sandbox("release");
        let tools = LockTools { sesync_lock: fake_lock(&dir, None), lock_dir: dir.clone() };
        let turns = Turns::default();
        let turn = turns.begin(&tools, "conv1", "w1:p1").unwrap();
        let herdr: Arc<dyn Herdr> = Arc::new(Statuses {
            list: vec!["unknown", "working", "working", "done"],
            calls: AtomicUsize::new(0),
        });
        turns.watch_with(FAST, tools.clone(), herdr, "conv1".into(), "w1:p1".into(), turn);
        assert!(wait_for(&dir, "release conv1"), "released after working then done");
        assert!(turns.begin(&tools, "conv1", "w1:p1").is_ok(), "the conversation is free again");
    }

    #[test]
    fn a_prompt_the_agent_never_acts_on_releases_after_the_grace_period() {
        let dir = sandbox("grace");
        let tools = LockTools { sesync_lock: fake_lock(&dir, None), lock_dir: dir.clone() };
        let turns = Turns::default();
        let turn = turns.begin(&tools, "conv2", "w1:p1").unwrap();
        let herdr: Arc<dyn Herdr> = Arc::new(Statuses { list: vec!["idle"], calls: AtomicUsize::new(0) });
        turns.watch_with(FAST, tools.clone(), herdr, "conv2".into(), "w1:p1".into(), turn);
        assert!(wait_for(&dir, "release conv2"), "an idle agent that never worked releases the claim");
    }

    #[test]
    fn a_turn_past_its_limit_releases_even_while_working() {
        let dir = sandbox("limit");
        let tools = LockTools { sesync_lock: fake_lock(&dir, None), lock_dir: dir.clone() };
        let turns = Turns::default();
        let turn = turns.begin(&tools, "conv3", "w1:p1").unwrap();
        let herdr: Arc<dyn Herdr> = Arc::new(Statuses { list: vec!["working"], calls: AtomicUsize::new(0) });
        let timing = Timing { limit: Duration::from_millis(200), ..FAST };
        turns.watch_with(timing, tools.clone(), herdr, "conv3".into(), "w1:p1".into(), turn);
        assert!(wait_for(&dir, "release conv3"), "a stuck turn cannot hold the conversation forever");
    }
}
