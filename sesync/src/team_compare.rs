//! The Team tab's comparison: this machine's completed turns against the team copy's, per conversation.
//! Reading and converting this machine's copy can take a while (a Claude transcript goes through the
//! converter), so a worker thread does it and the TUI applies the answers between frames
//! (invariant 22). `evaluate` is the same work, run synchronously for tests and for the worker.

use crate::harness::{Locations, Tools};
use crate::turns::{compare, fingerprint, turns_of, Relation, Turn};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

/// One conversation to compare: this machine's copy against the newest team copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub conversation: String,
    /// This machine's copy, as (harness, source key), when it has one.
    pub local: Option<(String, String)>,
    /// What the local copy looked like when the job was made (`local_stamp`). A new stamp means a new job.
    pub stamp: String,
    /// The newest team copy: its record sequence and its journal text, when the team holds one.
    pub team: Option<(u64, Vec<u8>)>,
}

/// The answer for one job. It is applied only while its stamp and team sequence still hold.
#[derive(Debug, Clone, PartialEq)]
pub struct Compared {
    pub conversation: String,
    pub stamp: String,
    pub team_seq: u64,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Both copies were read; the relation is over their completed turns.
    Read(Read),
    /// This machine's copy could not be read or converted. The reason, never the content.
    Unreadable(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Read {
    pub relation: Relation,
    /// This machine's completed turns, in order.
    pub local_turns: Vec<Turn>,
    /// The team copy's completed turns, in order (empty when the team holds none).
    pub team_turns: Vec<Turn>,
    /// Fingerprint of this machine's completed turns: the value a "keep mine" request is made for.
    pub local_fp: String,
    /// This machine's copy as an omp journal (the canonical bytes), for a fork or a replace. None when
    /// this machine has no copy.
    pub local_journal: Option<Vec<u8>>,
    /// The project directory the local copy records (its header's cwd), when it has one.
    pub local_cwd: Option<String>,
}

/// Compare one conversation. Pure apart from reading the local copy and running its converter.
pub fn evaluate(job: &Job, locations: &Locations, tools: &Tools, scratch: &Path) -> Compared {
    let team_seq = job.team.as_ref().map_or(0, |t| t.0);
    let outcome = match read_both(job, locations, tools, scratch) {
        Ok(read) => Outcome::Read(read),
        Err(why) => Outcome::Unreadable(why),
    };
    Compared { conversation: job.conversation.clone(), stamp: job.stamp.clone(), team_seq, outcome }
}

fn read_both(job: &Job, locations: &Locations, tools: &Tools, scratch: &Path) -> Result<Read, String> {
    let local_journal = match &job.local {
        Some((fmt, key)) => Some(crate::team_sync::canonical_for_source(fmt, key, locations, tools, scratch)
            .map_err(|e| format!("cannot read this copy: {e}"))?),
        None => None,
    };
    let local_turns = match &local_journal {
        Some(bytes) => turns_of(bytes).map_err(|e| format!("this copy is not a readable journal: {e}"))?,
        None => Vec::new(),
    };
    let team_turns = match &job.team {
        Some((_, text)) => turns_of(text).map_err(|e| format!("the team copy is not a readable journal: {e}"))?,
        None => Vec::new(),
    };
    let relation = compare(&local_turns, &team_turns);
    let local_cwd = local_journal.as_deref()
        .map(crate::harness::journal::session_head)
        .and_then(|h| h.cwd);
    Ok(Read {
        relation,
        local_fp: fingerprint(&local_turns),
        local_turns,
        team_turns,
        local_journal,
        local_cwd,
    })
}

/// The thread that compares. Dropping it stops the thread.
pub struct Worker {
    jobs: Sender<Job>,
    done: Receiver<Compared>,
}

impl Worker {
    /// Start the thread. It reads with the same locations and converter the TUI uses.
    pub fn start(locations: Locations, home: PathBuf, sesync_conv: PathBuf) -> Worker {
        let (jobs, wanted) = mpsc::channel::<Job>();
        let (answer, done) = mpsc::channel();
        std::thread::spawn(move || {
            let scratch = std::env::temp_dir().join(format!("sesync-compare-{}", std::process::id()));
            let tools = Tools { home: &home, sesync_conv: &sesync_conv };
            while let Ok(job) = wanted.recv() {
                let result = evaluate(&job, &locations, &tools, &scratch);
                if answer.send(result).is_err() {
                    break;
                }
            }
            let _ = std::fs::remove_dir_all(&scratch);
        });
        Worker { jobs, done }
    }

    /// Queue a comparison. Returns at once.
    pub fn request(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    /// Every answer that arrived since the last call.
    pub fn take(&self) -> Vec<Compared> {
        self.done.try_iter().collect()
    }
}
