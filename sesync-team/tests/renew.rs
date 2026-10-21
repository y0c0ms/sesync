//! Silent renewal of a member token. A token renews only while its enrolment is within the
//! 7-day window that started at the code-confirmed enrolment. After that, the 6-digit code is
//! required again. Renewal never extends past the window, and never works for a refused machine.

use sesync_team::service::{MemoryStore, Service, ServiceError, RENEW_WINDOW_SECS, TOKEN_LIFETIME_SECS};
use sesync_team::totp::code_at;

const T0: u64 = 1_000_020;
const DAY: u64 = 86_400;

fn enrolled() -> (Service<MemoryStore>, String) {
    let mut s = Service::new(MemoryStore::default());
    s.create_space("personal", &["alice"]).unwrap();
    let code = code_at(&s.space_secret("personal").unwrap(), T0);
    let token = s.enrol("personal", "alice", "box-1", &code, None, T0).map(|a| a.token().to_string()).unwrap();
    (s, token)
}

#[test]
fn the_window_is_seven_days_and_the_token_is_a_day() {
    assert_eq!(RENEW_WINDOW_SECS, 7 * DAY);
    assert_eq!(TOKEN_LIFETIME_SECS, DAY);
}

#[test]
fn a_token_renews_within_the_window() {
    let (mut s, token) = enrolled();
    // Two days later the token has expired, but the enrolment is still inside its window.
    let renewed = s.renew("personal", &token, "box-1", T0 + 2 * DAY).expect("renews inside the window");
    assert!(s.authenticate("personal", &renewed, "box-1", T0 + 2 * DAY + 1).is_ok());
}

#[test]
fn renewal_stops_at_the_window_and_the_code_is_needed_again() {
    let (mut s, token) = enrolled();
    let after_window = T0 + RENEW_WINDOW_SECS + 1;
    let r = s.renew("personal", &token, "box-1", after_window);
    assert!(matches!(r, Err(ServiceError::Auth(_))), "past the window, renewal is refused");
}

#[test]
fn a_renewed_token_never_outlives_the_window() {
    let (mut s, token) = enrolled();
    // Renew on day 6: the new token may last at most until day 7 (the window end), not 24 h more.
    let day6 = T0 + 6 * DAY;
    let renewed = s.renew("personal", &token, "box-1", day6).unwrap();
    let window_end = T0 + RENEW_WINDOW_SECS;
    assert!(s.authenticate("personal", &renewed, "box-1", window_end - 1).is_ok());
    assert!(s.authenticate("personal", &renewed, "box-1", window_end + 1).is_err(),
        "the renewed token stops at the window end");
}

#[test]
fn renewal_is_bound_to_the_machine() {
    let (mut s, token) = enrolled();
    assert!(s.renew("personal", &token, "box-2", T0 + DAY).is_err(), "another machine cannot renew it");
}

#[test]
fn a_forged_token_cannot_renew() {
    let (mut s, _) = enrolled();
    assert!(s.renew("personal", "alice|box-1|99999999999|forged", "box-1", T0 + DAY).is_err());
}
