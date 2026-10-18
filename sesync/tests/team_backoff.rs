//! The daemon's team poll interval: starts at the base, doubles while the team is idle, is capped
//! at the maximum, returns to the base on any activity, and is never below the base.
use std::time::Duration;

use sesync::team_backoff::{pull_is_idle, TeamBackoff};

fn secs(n: u64) -> Duration { Duration::from_secs(n) }

#[test]
fn it_starts_at_the_base() {
    let b = TeamBackoff::new(secs(5), secs(60));
    assert_eq!(b.interval(), secs(5));
}

#[test]
fn it_doubles_on_each_idle_step() {
    let mut b = TeamBackoff::new(secs(5), secs(60));
    b = b.next(false);
    assert_eq!(b.interval(), secs(10));
    b = b.next(false);
    assert_eq!(b.interval(), secs(20));
    b = b.next(false);
    assert_eq!(b.interval(), secs(40));
}

#[test]
fn it_caps_at_the_maximum() {
    let mut b = TeamBackoff::new(secs(5), secs(60));
    for _ in 0..10 {
        b = b.next(false);
    }
    assert_eq!(b.interval(), secs(60), "the doubling stops at the cap and stays there");
    b = b.next(false);
    assert_eq!(b.interval(), secs(60));
}

#[test]
fn a_cap_between_doublings_is_respected() {
    let mut b = TeamBackoff::new(secs(5), secs(30));
    b = b.next(false);
    assert_eq!(b.interval(), secs(10));
    b = b.next(false);
    assert_eq!(b.interval(), secs(20));
    b = b.next(false);
    assert_eq!(b.interval(), secs(30), "40 would pass the cap, so it is 30");
}

#[test]
fn activity_resets_to_the_base_from_any_interval() {
    let mut b = TeamBackoff::new(secs(5), secs(60));
    for _ in 0..4 {
        b = b.next(false);
    }
    assert_eq!(b.interval(), secs(60));
    b = b.next(true);
    assert_eq!(b.interval(), secs(5), "a new record or a successful push resets the interval");
    b = b.next(false);
    assert_eq!(b.interval(), secs(10), "and idling starts the climb again from the base");
}

#[test]
fn it_is_never_below_the_base() {
    // A maximum below the base is raised to the base: the interval cannot drop under SESYNC_POLL.
    let b = TeamBackoff::new(secs(5), secs(1));
    assert_eq!(b.interval(), secs(5));
    assert_eq!(b.next(false).interval(), secs(5));
    assert_eq!(b.next(true).interval(), secs(5));
}

#[test]
fn a_base_of_zero_stays_at_zero() {
    // SESYNC_POLL=0 (used by the tests) means the team side runs on every tick, as before.
    let b = TeamBackoff::new(Duration::ZERO, secs(60));
    assert_eq!(b.next(false).interval(), Duration::ZERO);
}

#[test]
fn doubling_never_overflows() {
    let mut b = TeamBackoff::new(Duration::MAX / 2 + secs(1), Duration::MAX);
    for _ in 0..3 {
        b = b.next(false);
    }
    assert_eq!(b.interval(), Duration::MAX, "a huge interval saturates at the cap, without panicking");
}

#[test]
fn a_pull_that_accepts_nothing_and_shows_the_same_set_is_idle() {
    assert!(pull_is_idle(0, 3, Some(3)));
    assert!(pull_is_idle(0, 0, Some(0)));
}

#[test]
fn a_pull_that_accepts_a_record_is_not_idle() {
    assert!(!pull_is_idle(1, 3, Some(3)));
}

#[test]
fn a_change_in_the_visible_set_is_not_idle() {
    assert!(!pull_is_idle(0, 2, Some(3)), "a conversation went away");
    assert!(!pull_is_idle(0, 4, Some(3)), "a conversation appeared");
}

#[test]
fn the_first_pull_is_never_idle() {
    assert!(!pull_is_idle(0, 0, None), "there is nothing to compare with yet");
}
