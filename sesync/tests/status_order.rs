//! The --status view, ported from daemon/sesync.py status. Sources are listed
//! newest first, mirrors are listed in a stable order under their source, and a
//! failure count is shown. The order is what a user reads, so it is pinned.

use sesync::pass::format_status;
use sesync::state::{Mirror, Source, State};

fn mirror(target: &str, path: &str) -> Mirror {
    Mirror { target: target.into(), path: path.into(), source_hash: String::new(),
             installed_hash: String::new(), diverged: false, edit_stamp: String::new(),
             target_updated: 0.0 }
}

#[test]
fn sources_are_listed_newest_first() {
    let mut st = State::default();
    let mut old = Source::new("omp", "/x/old.jsonl");
    old.last_seen_change = 10.0;
    let mut new = Source::new("omp", "/x/new.jsonl");
    new.last_seen_change = 99.0;
    st.sources.insert("/x/old.jsonl".into(), old);
    st.sources.insert("/x/new.jsonl".into(), new);
    let text = format_status(&st);
    let i_new = text.find("new.jsonl").unwrap();
    let i_old = text.find("old.jsonl").unwrap();
    assert!(i_new < i_old, "the newer source must come first:\n{text}");
}

#[test]
fn sources_with_equal_times_are_still_all_listed() {
    // partial_cmp can return None for NaN; equal times must not drop a source.
    let mut st = State::default();
    for n in ["a", "b", "c"] {
        let mut s = Source::new("pi", &format!("/p/{n}.jsonl"));
        s.last_seen_change = 5.0;
        st.sources.insert(format!("/p/{n}.jsonl"), s);
    }
    let text = format_status(&st);
    for n in ["a", "b", "c"] {
        assert!(text.contains(&format!("{n}.jsonl")), "{n} must be listed:\n{text}");
    }
}

#[test]
fn mirrors_are_listed_in_a_stable_order_under_their_source() {
    let mut st = State::default();
    let mut src = Source::new("omp", "/x/src.jsonl");
    src.mirrors.insert("pi".into(), mirror("pi", "/p/one.jsonl"));
    src.mirrors.insert("claude".into(), mirror("claude", "/c/two.jsonl"));
    st.sources.insert("/x/src.jsonl".into(), src);
    let text = format_status(&st);
    let i_claude = text.find("-> claude").expect("claude mirror listed");
    let i_pi = text.find("-> pi").expect("pi mirror listed");
    assert!(i_claude < i_pi, "mirror targets are sorted by name:\n{text}");
}

#[test]
fn a_source_with_no_failures_shows_no_failure_line() {
    let mut st = State::default();
    st.sources.insert("/x/ok.jsonl".into(), Source::new("omp", "/x/ok.jsonl"));
    assert!(!format_status(&st).contains("failures:"));
}

#[test]
fn an_opencode_source_shows_its_id_not_a_file_name() {
    let mut st = State::default();
    let mut s = Source::new("opencode", "ses_abc123");
    s.mirrors.insert("omp".into(), mirror("omp", "/o/mirror.jsonl"));
    st.sources.insert("ses_abc123".into(), s);
    let text = format_status(&st);
    assert!(text.contains("ses_abc123"), "{text}");
}
