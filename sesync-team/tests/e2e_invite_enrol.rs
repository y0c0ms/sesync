//! The flow the operator runs: create a team, invite a member, and the invited member enrols
//! with a code from the team secret. Runs only when TEAM_TEST_PG names a throwaway database.
use sesync_team::pg::PgStore;
use sesync_team::service::Service;
use sesync_team::totp::code_at;

const T0: u64 = 1_000_020;

#[test]
fn an_invited_member_enrols_after_the_admin_creates_the_team() {
    let Some(url) = std::env::var("TEAM_TEST_PG").ok() else { return };
    let schema = "t_invite_enrol";
    let mut c = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    c.batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema};")).unwrap();
    let store = PgStore::connect(&format!("{url}?options=-c%20search_path%3D{schema}"), [6u8; 32]).unwrap();
    store.migrate().unwrap();
    let mut s = Service::new(store);
    s.create_space("personal", &["admin-box"]).unwrap();
    s.invite("personal", "member-box").unwrap();
    let code = code_at(&s.space_secret("personal").unwrap(), T0);
    assert!(s.enrol("personal", "member-box", "member-box-2", &code, None, T0).is_ok(), "the invited member enrols");
    assert!(s.enrol("personal", "stranger", "box-x", &code, None, T0).is_err(), "a non-member does not");
}
