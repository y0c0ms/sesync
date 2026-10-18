//! One sync pass and the status view, ported from daemon/sesync.py one_pass
//! and status. A failed source is counted, not fatal: the other sources still sync.

use sesync::pass::{format_status, run_pass, PassSource};
use sesync::state::{Mirror, Source, State};

#[test]
fn a_failing_source_does_not_stop_the_others() {
    let sources = vec![
        PassSource { key: "bad".into(), title: "t".into(), urgent: false },
        PassSource { key: "good".into(), title: "t".into(), urgent: false },
    ];
    let mut state = State::default();
    state.sources.insert("bad".into(), Source::new("omp", "bad"));
    state.sources.insert("good".into(), Source::new("omp", "good"));
    let mut synced = Vec::new();
    let n = run_pass(&mut state, &sources, |s| {
        if s.key == "bad" { Err("converter exploded".to_string()) } else { synced.push(s.key.clone()); Ok(()) }
    });
    assert_eq!(synced, vec!["good".to_string()], "the good source must still sync");
    assert_eq!(state.sources["bad"].failures, 1, "the failure must be counted");
    assert_eq!(n, 2, "both sources were attempted");
}

#[test]
fn a_successful_source_does_not_count_a_failure() {
    let sources = vec![PassSource { key: "ok".into(), title: "t".into(), urgent: false }];
    let mut state = State::default();
    state.sources.insert("ok".into(), Source::new("omp", "ok"));
    run_pass(&mut state, &sources, |_| Ok(()));
    assert_eq!(state.sources["ok"].failures, 0);
}

#[test]
fn status_lists_each_source_then_its_mirrors() {
    let mut state = State::default();
    let mut src = Source::new("omp", "/x/2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    src.last_seen_change = 5.0;
    src.mirrors.insert("pi".into(), Mirror { target: "pi".into(), path: "/p/mirror.jsonl".into(),
        source_hash: String::new(), installed_hash: String::new(), diverged: false,
        edit_stamp: String::new(), target_updated: 0.0 });
    let key = "/x/2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl";
    state.sources.insert(key.into(), src);
    let text = format_status(&state);
    assert!(text.contains("omp       2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl"),
            "source line uses the file name, as Python prints it: {text}");
    assert!(text.contains("    -> pi        mirror.jsonl"), "mirror line is indented under its source: {text}");
}

#[test]
fn status_reports_failures() {
    let mut state = State::default();
    let mut src = Source::new("pi", "/p/a.jsonl");
    src.failures = 3;
    state.sources.insert("/p/a.jsonl".into(), src);
    assert!(format_status(&state).contains("failures: 3"));
}
