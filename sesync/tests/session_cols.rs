//! The sessions-list columns: team state, last sync, next due, mirror targets, and updated time.
//! Pure rules, so these run without a terminal.
use sesync::session_cols::{columns, fit, relative, short_span, Input, MirrorState};

fn base() -> Input {
    Input { marked: false, pushed_at: None, last_change: 0.0, now: 10_000.0, quiet_secs: 120.0, mirror_targets: vec![] }
}

#[test]
fn an_unmarked_never_pushed_session_shows_dashes_and_never() {
    let c = columns(&base());
    assert_eq!(c.team, "-");
    assert_eq!(c.last_sync, "never");
    assert_eq!(c.next_sync, "-", "an unmarked session has no team schedule");
    assert_eq!(c.mirrors, "-");
    assert_eq!(c.updated, "-", "no change time recorded");
}

#[test]
fn a_marked_session_is_marked_until_it_is_pushed_then_shared() {
    let mut i = base();
    i.marked = true;
    assert_eq!(columns(&i).team, "marked");
    i.pushed_at = Some(9_000.0);
    assert_eq!(columns(&i).team, "shared", "a pushed session is shared, marked or not");
}

#[test]
fn last_sync_is_the_time_of_the_last_push() {
    let mut i = base();
    i.marked = true;
    i.pushed_at = Some(9_820.0);
    assert_eq!(columns(&i).last_sync, "3m ago");
}

#[test]
fn next_sync_counts_down_while_the_session_is_still_quiet_for_the_team() {
    let mut i = base();
    i.marked = true;
    i.last_change = 9_950.0; // changed 50s ago; due 120s after the change, so 70s from now
    assert_eq!(columns(&i).next_sync, "in 1m");
}

#[test]
fn next_sync_reads_next_tick_once_the_quiet_period_has_passed() {
    let mut i = base();
    i.marked = true;
    i.last_change = 9_000.0;
    assert_eq!(columns(&i).next_sync, "next tick", "due now; the daemon's next pass pushes it");
}

#[test]
fn mirror_targets_are_listed_sorted_or_dashed() {
    let mut i = base();
    i.mirror_targets = vec![("pi".into(), MirrorState::Mirrored), ("claude".into(), MirrorState::Mirrored)];
    let c = columns(&i);
    assert_eq!(c.mirrors, "claude,pi");
    assert_eq!(c.mirror_parts, vec![("claude".to_string(), MirrorState::Mirrored), ("pi".to_string(), MirrorState::Mirrored)]);
}

#[test]
fn pending_and_failing_mirrors_carry_their_mark_in_text_and_structurally() {
    let mut i = base();
    i.mirror_targets = vec![
        ("omp".to_string(), MirrorState::Failing),
        ("claude".to_string(), MirrorState::Pending),
        ("pi".to_string(), MirrorState::Mirrored),
    ];
    let c = columns(&i);
    assert_eq!(c.mirrors, "claude?,omp!,pi", "the text form keeps the marks for readers without colour");
    assert_eq!(
        c.mirror_parts,
        vec![
            ("claude".to_string(), MirrorState::Pending),
            ("omp".to_string(), MirrorState::Failing),
            ("pi".to_string(), MirrorState::Mirrored),
        ],
        "the structured form lets the view colour each target"
    );
}

#[test]
fn updated_is_the_relative_time_since_the_source_changed() {
    let mut i = base();
    i.last_change = 9_400.0;
    assert_eq!(columns(&i).updated, "10m ago");
}

#[test]
fn relative_times_use_the_largest_sensible_unit() {
    assert_eq!(short_span(5), "5s");
    assert_eq!(short_span(59), "59s");
    assert_eq!(short_span(60), "1m");
    assert_eq!(short_span(3599), "59m");
    assert_eq!(short_span(3600), "1h");
    assert_eq!(short_span(86_400), "1d");
    assert_eq!(relative(100.0, 200.0), "0s ago", "a future time reads as just now, never negative");
}

#[test]
fn fit_pads_short_text_and_cuts_long_text_with_an_ellipsis() {
    assert_eq!(fit("abc", 5), "abc  ");
    assert_eq!(fit("abcdef", 4), "abc…");
    assert_eq!(fit("abc", 0), "");
    assert_eq!(fit("héllo", 3), "hé…", "counts characters, not bytes");
}
