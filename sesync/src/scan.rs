//! Change detection rule. Ported from daemon/sesync.py _scan_journals.
//! Kept as a pure function so every rule can be tested without a filesystem.

use crate::buckets;
use std::collections::BTreeSet;
use std::path::Path;

/// Files smaller than this are not sessions (a warm-up file, not a conversation).
pub const MIN_BYTES: u64 = 512;

pub struct ScanInput {
    pub size: u64,
    /// Last write time, seconds since the epoch.
    pub mtime: f64,
    pub now: f64,
    /// True when the path is a mirror the daemon created. Those are never sources.
    pub is_mirror: bool,
    pub max_age_secs: f64,
    /// Seconds of silence required before a file is treated as finished.
    pub quiet_secs: f64,
    /// The content hash differs from the one recorded at the last sync.
    pub hash_changed: bool,
    /// Every live (not diverged) mirror already carries the current hash.
    pub live_mirrors_current: bool,
    /// Mirrors recorded for this source.
    pub mirror_count: usize,
    /// How many harness targets should have a mirror.
    pub wanted_mirrors: usize,
    /// A chosen target has no mirror yet, so the user asked for this file explicitly.
    pub needs_mirror: bool,
}

/// Should this source be (re-)synced now?
pub fn should_sync(i: &ScanInput) -> bool {
    if i.is_mirror {
        return false; // loop prevention
    }
    if i.size < MIN_BYTES {
        return false;
    }
    // An explicit choice in the TUI outranks the age window: the user asked for this
    // session, and an old session would otherwise never be mirrored.
    if i.now - i.mtime > i.max_age_secs && !i.needs_mirror {
        return false;
    }
    if i.now - i.mtime < i.quiet_secs {
        return false; // still being written
    }
    let up_to_date = !i.hash_changed && i.live_mirrors_current
        && i.mirror_count >= i.wanted_mirrors && !i.needs_mirror;
    !up_to_date
}

/// May a chosen mirror with an unmet target be attempted again? A first failure backs off
/// for the quiet period, each further one doubles it, up to ten minutes. Without this, a
/// transcript that can never convert was retried every pass forever (logs, CPU). A source
/// whose content changed is retried regardless, because `hash_changed` makes the pass due.
pub fn mirror_retry_due(failures: u32, last_failure: f64, now: f64, quiet_secs: f64) -> bool {
    if failures == 0 {
        return true;
    }
    let wait = (quiet_secs * 2f64.powi(failures.min(20) as i32)).min(600.0);
    now - last_failure >= wait
}

/// The (wanted_mirrors, needs_mirror) pair for a source with chosen targets. `missing` is
/// true when a chosen target has no mirror (compare the actual sets, not counts: mirrors
/// for targets no longer chosen must not stand in). A missing target forces a pass — but
/// while a failed write is backing off, it must not keep the source "changed" every pass,
/// or the backoff never holds. While waiting, the demand reads as met; the retry timer (or
/// a content change, which sets `hash_changed`) brings the attempt back.
pub fn mirror_demand(missing: bool, chosen: usize, mirrors: usize, failures: u32,
                     last_failure: f64, now: f64, quiet_secs: f64) -> (usize, bool) {
    if !missing {
        return (chosen, false);
    }
    if mirror_retry_due(failures, last_failure, now, quiet_secs) {
        (chosen, true)
    } else {
        (mirrors, false)
    }
}

/// Is a recorded mirror in a place its harness will never list? Older daemons wrote file
/// mirrors into the wrong bucket (the wrong `--home`, or a dropped cwd), and OpenCode
/// imports made before its SQLite migration no longer exist in the store. A stale mirror
/// must count as missing so the next pass rewrites it where it belongs. Conservative by
/// design: whenever the expected place cannot be known (no cwd, outside home, no id set)
/// the answer is "not stale".
pub fn mirror_stale(target: &str, mirror_path: &str, cwd: Option<&str>, home: &str,
                    opencode_ids: Option<&BTreeSet<String>>) -> bool {
    match target {
        // The record holds the session id, not a path.
        "opencode" => opencode_ids.is_some_and(|ids| !ids.contains(mirror_path)),
        "omp" => match cwd.and_then(|c| buckets::omp_home_bucket(c, home)) {
            Some(expected) => parent_name(mirror_path).as_deref() != Some(expected.as_str()),
            None => false,
        },
        "pi" => match cwd {
            Some(c) => parent_name(mirror_path).as_deref() != Some(buckets::pi_bucket(c).as_str()),
            None => false,
        },
        _ => false,
    }
}

fn parent_name(path: &str) -> Option<String> {
    Path::new(path).parent()?.file_name().map(|n| n.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_first_failure_backs_off_for_the_quiet_period() {
        assert!(mirror_retry_due(0, 0.0, 1_000.0, 20.0), "a fresh choice is attempted at once");
        assert!(!mirror_retry_due(1, 1_000.0, 1_010.0, 20.0), "10 s after a failure is too soon");
        assert!(mirror_retry_due(1, 1_000.0, 1_040.0, 20.0), "2x the quiet period is due");
    }

    #[test]
    fn the_wait_doubles_and_caps_at_ten_minutes() {
        assert!(!mirror_retry_due(3, 1_000.0, 1_000.0 + 159.0, 20.0));
        assert!(mirror_retry_due(3, 1_000.0, 1_000.0 + 160.0, 20.0), "8x after 3 failures");
        assert!(!mirror_retry_due(9, 1_000.0, 1_000.0 + 599.0, 20.0));
        assert!(mirror_retry_due(9, 1_000.0, 1_000.0 + 600.0, 20.0), "the cap is ten minutes");
        assert!(!mirror_retry_due(60, 1_000.0, 1_000.0 + 599.0, 20.0), "a huge counter must not overflow the wait");
    }

    #[test]
    fn a_backing_off_missing_mirror_does_not_keep_the_source_due() {
        // The bug this guards: should_sync alone sees mirror_count < wanted_mirrors and
        // retries every pass, no matter the failure count.
        let mut i = base();
        i.hash_changed = false;
        i.live_mirrors_current = true;
        i.mirror_count = 0;
        let (wanted, needs) = mirror_demand(true, 1, 0, 4, 1_000.0, 1_020.0, 20.0);
        i.wanted_mirrors = wanted;
        i.needs_mirror = needs;
        assert!(!should_sync(&i), "a backed-off missing mirror must not force a pass");
        let (wanted, needs) = mirror_demand(true, 1, 0, 4, 1_000.0, 1_400.0, 20.0);
        i.wanted_mirrors = wanted;
        i.needs_mirror = needs;
        assert!(should_sync(&i), "once the wait is over the pass is due again");
    }

    #[test]
    fn a_mirror_for_an_unchosen_target_does_not_satisfy_a_missing_choice() {
        // Counts cannot stand in for the sets: chosen {pi} with a stale mirror for omp has
        // one mirror and one choice, yet pi is still missing and the write must be due.
        let mut i = base();
        i.hash_changed = false;
        i.live_mirrors_current = true;
        i.mirror_count = 1;
        let (wanted, needs) = mirror_demand(true, 1, 1, 0, 0.0, 2_000.0, 20.0);
        i.wanted_mirrors = wanted;
        i.needs_mirror = needs;
        assert!(should_sync(&i), "the stale mirror must not suppress the overdue write");
    }

    #[test]
    fn a_file_mirror_in_the_wrong_bucket_is_stale() {
        // The bug this guards: the daemon passed --home = the store's parent, so a session
        // with cwd /home/u/proj mirrored to omp landed in --home-u-proj-, a bucket omp
        // never lists from that project. Older writes dropped the cwd and landed in --.
        let home = "/home/u";
        let cwd = Some("/home/u/proj");
        assert!(mirror_stale("omp", "/store/sessions/--/x.jsonl", cwd, home, None));
        assert!(mirror_stale("omp", "/store/sessions/--home-u-proj-/x.jsonl", cwd, home, None));
        assert!(!mirror_stale("omp", "/store/sessions/-proj/x.jsonl", cwd, home, None));
        // Pi's buckets are absolute (verified on disk): --home-u-proj- is right, -- is not.
        assert!(!mirror_stale("pi", "/store/sessions/--home-u-proj-/x.jsonl", cwd, home, None));
        assert!(mirror_stale("pi", "/store/sessions/--/x.jsonl", cwd, home, None));
        // Claude mirrors live under projects/<bucket>; the daemon has no rule to check there.
        assert!(!mirror_stale("claude", "/claude/projects/-home-u-proj/u.jsonl", cwd, home, None));
    }

    #[test]
    fn without_a_cwd_nothing_is_called_stale() {
        assert!(!mirror_stale("omp", "/store/sessions/--/x.jsonl", None, "/home/u", None));
        assert!(!mirror_stale("pi", "/store/sessions/--/x.jsonl", None, "/home/u", None));
        // cwd outside home: omp's fallback encoding is not checked.
        assert!(!mirror_stale("omp", "/store/sessions/--tmp-/x.jsonl", Some("/tmp"), "/home/u", None));
    }

    #[test]
    fn an_opencode_mirror_missing_from_the_store_is_stale() {
        let ids: BTreeSet<String> = ["ses_a".to_string()].into_iter().collect();
        assert!(mirror_stale("opencode", "ses_gone", None, "/home/u", Some(&ids)));
        assert!(!mirror_stale("opencode", "ses_a", None, "/home/u", Some(&ids)));
        assert!(!mirror_stale("opencode", "ses_x", None, "/home/u", None), "an unreadable store must not flag mirrors");
    }
}
