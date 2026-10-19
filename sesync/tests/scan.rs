//! Change detection, ported from daemon/sesync.py _scan_journals.
//! The rules pinned here: never treat a mirror we created as a source; skip tiny
//! and stale files; wait out the quiet period; re-sync only when content changed
//! or a live mirror is behind.

use sesync::scan::{should_sync, ScanInput, MIN_BYTES};

fn base() -> ScanInput {
    ScanInput {
        size: 4096,
        mtime: 1_000.0,
        now: 2_000.0,
        is_mirror: false,
        max_age_secs: 14.0 * 86400.0,
        quiet_secs: 20.0,
        hash_changed: true,
        live_mirrors_current: false,
        mirror_count: 0,
        wanted_mirrors: 1,
        needs_mirror: false,
    }
}

#[test]
fn a_mirror_we_created_is_never_a_source() {
    let mut i = base();
    i.is_mirror = true;
    assert!(!should_sync(&i), "loop prevention: our own mirrors must be ignored");
}

#[test]
fn tiny_files_are_skipped() {
    let mut i = base();
    i.size = MIN_BYTES - 1;
    assert!(!should_sync(&i));
}

#[test]
fn files_older_than_the_window_are_skipped() {
    let mut i = base();
    i.now = i.mtime + i.max_age_secs + 1.0;
    assert!(!should_sync(&i));
}

#[test]
fn an_explicitly_chosen_mirror_bypasses_the_age_window() {
    let mut i = base();
    i.now = i.mtime + i.max_age_secs + 1.0;
    i.needs_mirror = true;
    assert!(should_sync(&i), "a target the user chose must be written even for an old session");
}

#[test]
fn a_file_still_being_written_waits_for_quiet() {
    let mut i = base();
    i.now = i.mtime + 5.0; // only 5 s since the last write, quiet is 20 s
    assert!(!should_sync(&i));
}

#[test]
fn a_changed_quiet_file_is_synced() {
    assert!(should_sync(&base()));
}

#[test]
fn an_unchanged_up_to_date_file_is_not_resynced() {
    let mut i = base();
    i.hash_changed = false;
    i.live_mirrors_current = true;
    i.mirror_count = 1;
    assert!(!should_sync(&i), "same content and all mirrors current: nothing to do");
}

#[test]
fn a_missing_mirror_triggers_a_sync_even_if_content_is_unchanged() {
    let mut i = base();
    i.hash_changed = false;
    i.live_mirrors_current = true;
    i.mirror_count = 0; // a wanted target has no mirror yet
    assert!(should_sync(&i));
}
