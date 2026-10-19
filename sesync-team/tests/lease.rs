//! Cross-machine turn lease. A turn on one machine blocks a turn on another. A lease
//! expires 30 minutes after the last heartbeat, so a crashed machine can never hold a
//! conversation forever. A stale holder's writes are refused by a fencing version.

use sesync_team::lease::{Lease, LeaseError, LeaseTable, LEASE_SECS};

const SID: &str = "ses_space1_conversation";

#[test]
fn a_turn_on_one_machine_blocks_a_turn_on_another() {
    let mut t = LeaseTable::default();
    let a = t.acquire(SID, "laptop-a", 1_000).expect("first acquire");
    assert!(matches!(t.acquire(SID, "laptop-b", 1_001), Err(LeaseError::Held { .. })));
    assert_eq!(a.holder, "laptop-a");
}

#[test]
fn the_same_machine_can_renew_its_own_lease() {
    let mut t = LeaseTable::default();
    t.acquire(SID, "laptop-a", 1_000).unwrap();
    let renewed = t.heartbeat(SID, "laptop-a", 1_500).expect("renew");
    assert_eq!(renewed.last_seen, 1_500);
}

#[test]
fn a_lease_expires_thirty_minutes_after_the_last_heartbeat() {
    assert_eq!(LEASE_SECS, 30 * 60, "the expiry must be 30 minutes");
    let mut t = LeaseTable::default();
    t.acquire(SID, "laptop-a", 1_000).unwrap();
    // Just before expiry: still held by a.
    assert!(matches!(t.acquire(SID, "laptop-b", 1_000 + LEASE_SECS - 1), Err(LeaseError::Held { .. })));
    // At expiry: b may take it over.
    t.acquire(SID, "laptop-b", 1_000 + LEASE_SECS).expect("expired lease is taken over");
}

#[test]
fn a_heartbeat_moves_the_expiry_forward() {
    let mut t = LeaseTable::default();
    t.acquire(SID, "laptop-a", 1_000).unwrap();
    t.heartbeat(SID, "laptop-a", 1_000 + LEASE_SECS - 60).unwrap();
    // Without the heartbeat b would get it at 1000+LEASE; with it, b must still be refused.
    assert!(matches!(t.acquire(SID, "laptop-b", 1_000 + LEASE_SECS + 10), Err(LeaseError::Held { .. })));
}

#[test]
fn a_stale_holder_cannot_write_after_takeover() {
    // a holds version 1, goes silent, b takes over (version 2). a's write with version 1
    // must be refused: it would overwrite b's newer work.
    let mut t = LeaseTable::default();
    let first: Lease = t.acquire(SID, "laptop-a", 1_000).unwrap();
    t.acquire(SID, "laptop-b", 1_000 + LEASE_SECS).unwrap();
    assert!(matches!(t.check_write(SID, first.version, "laptop-a"), Err(LeaseError::Stale { .. })));
}

#[test]
fn releasing_frees_the_conversation_at_once() {
    let mut t = LeaseTable::default();
    t.acquire(SID, "laptop-a", 1_000).unwrap();
    t.release(SID, "laptop-a").unwrap();
    t.acquire(SID, "laptop-b", 1_001).expect("a released turn can be taken immediately");
}

#[test]
fn a_version_is_never_granted_twice_even_after_a_release() {
    // A push takes the lease, writes and releases, every time. The version must still rise, or a
    // record's sequence would repeat and a replayed older record would look current.
    let mut t = LeaseTable::default();
    let first = t.acquire(SID, "laptop-a", 1_000).unwrap();
    t.release(SID, "laptop-a").unwrap();
    let second = t.acquire(SID, "laptop-a", 1_001).unwrap();
    let third = t.acquire(SID, "laptop-b", 1_002);
    assert!(third.is_err(), "held by a again, so b is refused");
    t.release(SID, "laptop-a").unwrap();
    let third = t.acquire(SID, "laptop-b", 1_003).unwrap();
    assert!(first.version < second.version && second.version < third.version,
        "versions rise across releases: {} {} {}", first.version, second.version, third.version);
}

#[test]
fn only_the_holder_can_release() {
    let mut t = LeaseTable::default();
    t.acquire(SID, "laptop-a", 1_000).unwrap();
    assert!(t.release(SID, "laptop-b").is_err(), "another machine must not release a's turn");
}

#[test]
fn seeding_continues_the_version_and_never_lowers_it() {
    let mut t = LeaseTable::default();
    t.seed(SID, 5);
    let a = t.acquire(SID, "laptop-a", 1_000).unwrap();
    assert_eq!(a.version, 6, "the next grant is above the seeded floor");
    t.release(SID, "laptop-a").unwrap();
    t.seed(SID, 2);
    let b = t.acquire(SID, "laptop-a", 1_001).unwrap();
    assert_eq!(b.version, 7, "a lower seed does not move the version back");
}
