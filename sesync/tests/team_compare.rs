//! The Team list's comparison worker: a job in, the relation of the completed turns back.

#[path = "support/team_list.rs"]
mod team_list;

use sesync::manage::Roots;
use sesync::team_compare::{Job, Outcome, Worker};
use sesync::turns::Relation;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use team_list::*;

/// Wait for the worker's answers, up to a few seconds.
fn collect(w: &Worker, want: usize) -> Vec<sesync::team_compare::Compared> {
    let mut got = Vec::new();
    let until = Instant::now() + Duration::from_secs(10);
    while got.len() < want && Instant::now() < until {
        got.extend(w.take());
        std::thread::sleep(Duration::from_millis(10));
    }
    got
}

#[test]
fn the_worker_answers_a_job_with_the_relation_of_the_completed_turns() {
    let sb = sandbox("compare-worker");
    let key = write_local(&sb, SAME, "Ahead", &[("q", "a"), ("q2", "a2")]);
    let roots = Roots::under(&sb, sb.join("no-sesync-conv"));
    let w = Worker::start(roots.locations.clone(), sb.clone(), PathBuf::from(sb.join("no-sesync-conv")));
    w.request(Job {
        conversation: SAME.into(),
        local: Some(("omp".into(), key)),
        stamp: "stamp-1".into(),
        team: Some((2, journal(SAME, "Ahead", &[("q", "a")]))),
    });
    let answers = collect(&w, 1);
    assert_eq!(answers.len(), 1, "one answer for one job");
    let a = &answers[0];
    assert_eq!((a.conversation.as_str(), a.stamp.as_str(), a.team_seq), (SAME, "stamp-1", 2));
    match &a.outcome {
        Outcome::Read(read) => {
            assert!(matches!(read.relation, Relation::Ahead(1)), "{:?}", read.relation);
            assert_eq!(read.local_turns.len(), 2);
            assert!(read.local_journal.is_some(), "the local copy's journal is kept for a fork");
        }
        other => panic!("expected a read, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn an_unreadable_local_copy_is_reported_by_reason_and_never_by_content() {
    let sb = sandbox("compare-unreadable");
    let key = sb.join("omp").join("broken.jsonl");
    std::fs::create_dir_all(key.parent().unwrap()).unwrap();
    std::fs::write(&key, b"{\"type\":\"session\"\nnot json at all: secret-words-here\n").unwrap();
    let roots = Roots::under(&sb, sb.join("no-sesync-conv"));
    let w = Worker::start(roots.locations.clone(), sb.clone(), sb.join("no-sesync-conv"));
    w.request(Job {
        conversation: SAME.into(),
        local: Some(("omp".into(), key.display().to_string())),
        stamp: "s".into(),
        team: None,
    });
    let answers = collect(&w, 1);
    assert_eq!(answers.len(), 1);
    match &answers[0].outcome {
        Outcome::Unreadable(why) => assert!(!why.contains("secret-words-here"), "the reason never echoes content: {why}"),
        other => panic!("a broken journal is unreadable, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&sb);
}
