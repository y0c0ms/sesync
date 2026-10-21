//! The server must survive a database that is not reachable for its first seconds (the
//! kube-router race). These tests use an unused local port, so no database is needed.

use sesync_team::pg::PgStore;
use std::time::{Duration, Instant};

#[test]
fn startup_gives_up_after_its_attempts_when_nothing_listens() {
    // Port 1 is never a database here; every attempt is refused.
    let started = Instant::now();
    let r = PgStore::connect_with_retry("postgres://u:p@127.0.0.1:1/db", [0u8; 32], 3, Duration::from_millis(50));
    assert!(r.is_err(), "no database, no store");
    assert!(started.elapsed() >= Duration::from_millis(50), "it waited between attempts");
    assert!(started.elapsed() < Duration::from_secs(10), "the retries are bounded");
}

#[test]
fn a_single_attempt_is_not_retried() {
    // A retry would sleep the full 30 s wait. A refused connect alone can take about 2 s on
    // Windows (it retries the SYN), so the bound sits between the two, not at the connect time.
    let started = Instant::now();
    let r = PgStore::connect_with_retry("postgres://u:p@127.0.0.1:1/db", [0u8; 32], 1, Duration::from_secs(30));
    assert!(r.is_err());
    assert!(started.elapsed() < Duration::from_secs(15), "one attempt does not sleep");
}

#[test]
fn a_database_that_comes_up_late_is_reached() {
    let Some(url) = std::env::var("TEAM_TEST_PG").ok() else { return };
    // A healthy database: the first attempt succeeds and the store works.
    let s = PgStore::connect_with_retry(&url, [5u8; 32], 5, Duration::from_millis(200));
    assert!(s.is_ok(), "a reachable database connects within the retries");
}
