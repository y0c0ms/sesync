use sesync::session_cols::{columns, display_safe, fit, is_transient, text_width, Input, MirrorState};

fn base() -> Input {
    Input {
        marked: false,
        pushed_at: None,
        last_change: 0.0,
        now: 10_000.0,
        mirror_targets: vec![],
        synced_at: 0.0,
    }
}

#[test]
fn an_unshared_session_with_no_mirrors_shows_dashes() {
    let c = columns(&base());
    assert_eq!(c.team, "-");
    assert_eq!(c.synced, "-", "no mirror target, nothing to sync");
    assert_eq!(c.mirrors, "-");
    assert_eq!(c.updated, "-", "no change time recorded");
}

#[test]
fn team_column_reads_marked_then_shared() {
    let mut i = base();
    i.marked = true;
    assert_eq!(columns(&i).team, "marked");
    i.pushed_at = Some(9_000.0);
    assert_eq!(columns(&i).team, "shared", "a push outranks the mark");
}

#[test]
fn synced_is_the_age_of_the_oldest_successful_write() {
    let mut i = base();
    i.mirror_targets = vec![("pi".into(), MirrorState::Mirrored), ("claude".into(), MirrorState::Mirrored)];
    i.synced_at = 9_820.0;
    assert_eq!(columns(&i).synced, "3m ago");
}

#[test]
fn synced_reads_waiting_while_a_target_has_no_write_yet() {
    let mut i = base();
    i.mirror_targets = vec![("pi".into(), MirrorState::Mirrored), ("omp".into(), MirrorState::Pending)];
    i.synced_at = 9_820.0;
    assert_eq!(columns(&i).synced, "waiting");
}

#[test]
fn synced_reads_waiting_while_a_target_is_busy_and_ok_when_no_write_time_is_known() {
    let mut i = base();
    i.mirror_targets = vec![("pi".into(), MirrorState::Mirrored), ("agy".into(), MirrorState::Busy)];
    i.synced_at = 9_820.0;
    assert_eq!(columns(&i).synced, "waiting", "a copy in use is retried, not failed");
    i.mirror_targets = vec![("pi".into(), MirrorState::Mirrored)];
    i.synced_at = 0.0;
    assert_eq!(columns(&i).synced, "ok", "written copies with no recorded time are not 'never'");
}

#[test]
fn only_retried_problems_are_transient() {
    assert!(is_transient("held by claude; will retry"));
    assert!(is_transient("Antigravity has this conversation open; will retry (database in use)"));
    assert!(!is_transient("conversion failed: unexpected end of input"));
}

#[test]
fn synced_reads_failing_when_any_target_has_a_problem_even_if_others_are_fine() {
    let mut i = base();
    i.mirror_targets = vec![("pi".into(), MirrorState::Mirrored), ("omp".into(), MirrorState::Failing)];
    i.synced_at = 9_820.0;
    assert_eq!(columns(&i).synced, "failing");
}

#[test]
fn mirrors_list_is_sorted_by_target_and_marks_each_state() {
    let mut i = base();
    i.mirror_targets = vec![
        ("pi".into(), MirrorState::Mirrored),
        ("claude".into(), MirrorState::Pending),
        ("omp".into(), MirrorState::Failing),
        ("antigravity".into(), MirrorState::Busy),
    ];
    assert_eq!(columns(&i).mirrors, "agy~,claude?,omp!,pi", "Antigravity goes by agy");
}

#[test]
fn updated_is_the_relative_time_of_the_last_change() {
    let mut i = base();
    i.last_change = 9_950.0;
    assert_eq!(columns(&i).updated, "50s ago");
}

#[test]
fn display_safe_drops_glyphs_terminals_draw_at_different_widths() {
    // The title that garbled the list: U+2696 then the emoji selector U+FE0F.
    assert_eq!(display_safe("\u{2696}\u{FE0F} ETICA: Build"), "ETICA: Build");
    assert_eq!(display_safe("ship \u{1F680} it"), "ship it", "a pictograph between words leaves one space");
    assert_eq!(display_safe("keycap 1\u{FE0F}\u{20E3}"), "keycap 1");
    assert_eq!(display_safe("folder \u{F07B} icon"), "folder icon", "private-use icon-font glyphs go");
    assert_eq!(display_safe("caf\u{E9} \u{4E2D}\u{6587}"), "caf\u{E9} \u{4E2D}\u{6587}", "letters and CJK stay");
    assert_eq!(display_safe("a\u{202E}b\u{0007}c"), "abc", "bidi override and bell are dropped");
    assert_eq!(display_safe("  one\ttwo\n\nthree  "), "one two three", "whitespace collapses and trims");
}

/// Every harness icon is drawn: none is a glyph the screen scrub drops, or a harness would show
/// in the pickers without the icon the others have.
#[test]
fn every_harness_icon_survives_the_screen_scrub() {
    for h in sesync::harness::ALL {
        assert_eq!(display_safe(h.icon()), h.icon(), "the {} icon is dropped from the screen", h.id());
    }
}

#[test]
fn fit_pads_and_cuts_by_terminal_cells_not_characters() {
    assert_eq!(text_width(&fit("\u{4E2D}\u{6587}", 6)), 6, "two wide characters pad to six cells");
    assert_eq!(text_width(&fit("\u{4E2D}\u{6587}\u{4E2D}", 4)), 4, "a wide character that does not fit is cut whole");
    assert_eq!(text_width(&fit("plain title that is long", 8)), 8, "long text is cut to the width");
    assert_eq!(fit("ab", 5), "ab   ");
}
