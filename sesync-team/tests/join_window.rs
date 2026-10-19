//! A join window. An approved join is valid for at most 10 minutes. After that it expires and the
//! device is not a member, unless the admin's signed list has included it by then. An approval
//! dated in the future is refused, so it cannot be made to outlive its window.
use sesync_team::membership::{device_entry, Device, Join, JoinError, MemberList, Verifier, JOIN_SKEW_SECS, JOIN_WINDOW_SECS};

const T0: u64 = 1_000_000;
const SPACE: &str = "personal";

fn setup() -> (Device, Device, Device) {
    (Device::generate(), Device::generate(), Device::generate())
}

fn base(admin: &Device, member: &Device) -> Verifier {
    let mut v = Verifier::new(admin.public_key());
    v.accept(&MemberList::new(1, vec![device_entry(admin), device_entry(member)]).sign(admin)).unwrap();
    v
}

#[test]
fn the_window_is_ten_minutes() {
    assert_eq!(JOIN_WINDOW_SECS, 10 * 60);
}

#[test]
fn the_skew_allowance_is_five_minutes() {
    assert_eq!(JOIN_SKEW_SECS, 5 * 60);
}

#[test]
fn an_approved_join_is_admitted_within_the_window() {
    let (admin, member, newcomer) = setup();
    let mut v = base(&admin, &member);
    let join = Join::request(SPACE, &newcomer).approve(&member, T0);
    v.admit_at(SPACE, &join, T0 + 9 * 60).expect("inside the window");
    assert!(v.is_member_at(&newcomer.public_key(), T0 + 9 * 60));
}

#[test]
fn an_approved_join_expires_after_ten_minutes() {
    let (admin, member, newcomer) = setup();
    let mut v = base(&admin, &member);
    let join = Join::request(SPACE, &newcomer).approve(&member, T0);
    assert_eq!(v.admit_at(SPACE, &join, T0 + 10 * 60 + 1), Err(JoinError::Expired));
    assert!(!v.is_member_at(&newcomer.public_key(), T0 + 10 * 60 + 1));
}

#[test]
fn a_join_admitted_in_time_expires_unless_the_admin_confirms_it() {
    let (admin, member, newcomer) = setup();
    let mut v = base(&admin, &member);
    let join = Join::request(SPACE, &newcomer).approve(&member, T0);
    v.admit_at(SPACE, &join, T0 + 60).unwrap();
    // The window lapses before the admin signs a list that includes the device.
    assert!(!v.is_member_at(&newcomer.public_key(), T0 + 10 * 60 + 1), "unconfirmed, it lapses");
}

#[test]
fn an_admin_signed_list_that_includes_the_device_confirms_it() {
    let (admin, member, newcomer) = setup();
    let mut v = base(&admin, &member);
    let join = Join::request(SPACE, &newcomer).approve(&member, T0);
    v.admit_at(SPACE, &join, T0 + 60).unwrap();
    v.accept(&MemberList::new(2, vec![device_entry(&admin), device_entry(&member), device_entry(&newcomer)])
        .sign(&admin)).unwrap();
    assert!(v.is_member_at(&newcomer.public_key(), T0 + 60 * 60), "confirmed by the admin: no longer on a timer");
}

#[test]
fn a_join_is_not_admitted_after_its_window_even_with_a_valid_approval() {
    let (admin, member, newcomer) = setup();
    let mut v = base(&admin, &member);
    let join = Join::request(SPACE, &newcomer).approve(&member, T0);
    assert_eq!(v.admit_at(SPACE, &join, T0 + 11 * 60), Err(JoinError::Expired));
}

#[test]
fn an_approval_dated_more_than_the_skew_allowance_in_the_future_is_refused() {
    let (admin, member, newcomer) = setup();
    let mut v = base(&admin, &member);
    let future = Join::request(SPACE, &newcomer).approve(&member, T0 + JOIN_SKEW_SECS + 1);
    assert_eq!(v.admit_at(SPACE, &future, T0), Err(JoinError::ClockSkew),
        "an approval signed for a time ahead of this clock is refused");
    assert!(!v.is_member_at(&newcomer.public_key(), T0), "nothing is admitted from it");
}

#[test]
fn an_approval_within_the_skew_allowance_is_accepted() {
    let (admin, member, newcomer) = setup();
    let mut v = base(&admin, &member);
    let edge = Join::request(SPACE, &newcomer).approve(&member, T0 + JOIN_SKEW_SECS);
    v.admit_at(SPACE, &edge, T0).expect("exactly the allowance ahead is still accepted");
    assert!(v.is_member_at(&newcomer.public_key(), T0));
}

#[test]
fn a_confirmed_join_does_not_come_back_once_the_admin_drops_the_device() {
    // Confirmed by the admin, then removed by a later list: the old pending join must not revive it.
    let (admin, member, newcomer) = setup();
    let mut v = base(&admin, &member);
    v.admit_at(SPACE, &Join::request(SPACE, &newcomer).approve(&member, T0), T0 + 60).unwrap();
    v.accept(&MemberList::new(2, vec![device_entry(&admin), device_entry(&member), device_entry(&newcomer)])
        .sign(&admin)).unwrap();
    v.accept(&MemberList::new(3, vec![device_entry(&admin), device_entry(&member)]).sign(&admin)).unwrap();
    assert!(!v.is_member_at(&newcomer.public_key(), T0 + 60), "dropped by the admin, so not a member");
}
