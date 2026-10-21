//! When the database is down, a valid member must see "service unavailable" (503), never
//! "no such space" (404) or "not a member" (403). Those answers would make a client believe
//! its data is gone when only the database is unreachable.

#[path = "common/mod.rs"]
mod common;

use sesync_team::http::{self, EnrolBody, LeaseBody};
use sesync_team::service::{Service, Store};
use sesync_team::totp::code_at;

use common::{Faults, TestStore};

const T0: u64 = 1_000_020;

fn setup() -> (Service<TestStore>, Faults, String) {
    let faults = Faults::default();
    let mut s = Service::new(TestStore::with_faults(faults.clone()));
    s.create_space("team-a", &["alice"]).unwrap();
    let code = code_at(&s.space_secret("team-a").unwrap(), T0);
    let token = s.enrol("team-a", "alice", "box-1", &code, None, T0).map(|a| a.token().to_string()).unwrap();
    (s, faults, format!("Bearer {token}"))
}

#[test]
fn a_listing_during_an_outage_is_503_not_404() {
    let (s, faults, auth) = setup();
    faults.database_down.set(true);
    let (status, json) = http::list_sessions(&s, Some(&auth), "team-a", "box-1", T0 + 1);
    assert_eq!(status, 503, "an outage must not look like a missing space: {json}");
}

#[test]
fn a_lease_during_an_outage_is_503_not_403_or_404() {
    let (_s, faults, auth) = setup();
    faults.database_down.set(true);
    let body = LeaseBody { space: "team-a".into(), conversation: "c1".into(), machine: "box-1".into() };
    let (status, _) = http::acquire(&mut Service::new(TestStore::with_faults(faults.clone())), Some(&auth), &body, T0);
    assert_eq!(status, 503);
}

#[test]
fn a_join_during_an_outage_is_503_not_a_wrong_code() {
    let (mut s, faults, _) = setup();
    faults.database_down.set(true);
    let body = EnrolBody { space: "team-a".into(), member: "alice".into(), machine: "box-2".into(), code: "000000".into(), device: None, proof: None };
    let (status, _) = http::enrol(&mut s, &body, T0);
    assert_eq!(status, 503, "a join during an outage is unavailable, not a refused code");
}

#[test]
fn health_reports_503_during_an_outage_and_recovers() {
    let (s, faults, _) = setup();
    assert_eq!(http::health(&s).0, 200);
    faults.database_down.set(true);
    assert_eq!(http::health(&s).0, 503);
    faults.database_down.set(false);
    assert_eq!(http::health(&s).0, 200, "it recovers when the database returns");
}
