//! Join approval. A new device joins only when a current member signs its approval. The
//! approval names the new device's entry and the space it is for, so a member cannot approve one
//! device and have a different one admitted, and an approval for one space cannot admit a device
//! to another. Joins are not revocations: any member may approve a join, but only the admin may
//! sign a revocation (a list without a device).
use sesync_team::membership::{device_entry, Device, Join, JoinError, MemberList, Verifier};

const T0: u64 = 1_000_000;
const SPACE: &str = "personal";

fn setup() -> (Device, Device, Device) {
    (Device::generate(), Device::generate(), Device::generate()) // admin, member, newcomer
}

#[test]
fn a_member_approved_join_is_admitted() {
    let (admin, member, newcomer) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&MemberList::new(1, vec![device_entry(&admin), device_entry(&member)]).sign(&admin)).unwrap();
    let join = Join::request(SPACE, &newcomer).approve(&member, T0);
    v.admit(SPACE, &join).expect("a member's approval admits the device");
    assert!(v.is_member_at(&newcomer.public_key(), T0 + 60), "admitted inside the window");
}

#[test]
fn a_join_nobody_approved_is_refused() {
    let (admin, member, newcomer) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&MemberList::new(1, vec![device_entry(&admin), device_entry(&member)]).sign(&admin)).unwrap();
    let unapproved = Join::request(SPACE, &newcomer);
    assert_eq!(v.admit(SPACE, &unapproved), Err(JoinError::NotApproved));
    assert!(!v.is_member(&newcomer.public_key()));
}

#[test]
fn an_approval_from_a_non_member_is_refused() {
    let (admin, member, newcomer) = setup();
    let outsider = Device::generate();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&MemberList::new(1, vec![device_entry(&admin), device_entry(&member)]).sign(&admin)).unwrap();
    let join = Join::request(SPACE, &newcomer).approve(&outsider, T0);
    assert_eq!(v.admit(SPACE, &join), Err(JoinError::ApproverNotMember));
}

#[test]
fn an_approval_cannot_be_moved_to_another_device() {
    let (admin, member, newcomer) = setup();
    let other = Device::generate();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&MemberList::new(1, vec![device_entry(&admin), device_entry(&member)]).sign(&admin)).unwrap();
    // The member approved `newcomer`; swapping in `other` must not pass.
    let mut join = Join::request(SPACE, &newcomer).approve(&member, T0);
    join.request = Join::request(SPACE, &other).request;
    assert_eq!(v.admit(SPACE, &join), Err(JoinError::BadApproval));
    assert!(!v.is_member(&other.public_key()));
}

#[test]
fn an_approval_for_one_space_cannot_admit_a_device_to_another() {
    // The same member is listed in both spaces, so the only thing that differs is the space the
    // approval was made for. A replay into the other space must be refused.
    let (_admin, member, newcomer) = setup();
    let team_b_admin = Device::generate();
    let mut team_b = Verifier::new(team_b_admin.public_key());
    team_b.accept(&MemberList::new(1, vec![device_entry(&team_b_admin), device_entry(&member)])
        .sign(&team_b_admin)).unwrap();
    let approved_for_a = Join::request("team-a", &newcomer).approve(&member, T0);
    assert_eq!(team_b.admit("team-b", &approved_for_a), Err(JoinError::WrongSpace),
        "an approval made for team-a does not admit to team-b");
    assert!(!team_b.is_member(&newcomer.public_key()));
}

#[test]
fn the_space_an_approval_names_is_the_space_it_is_admitted_to() {
    let (admin, member, newcomer) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&MemberList::new(1, vec![device_entry(&admin), device_entry(&member)]).sign(&admin)).unwrap();
    let join = Join::request("team-a", &newcomer).approve(&member, T0);
    assert_eq!(v.admit("team-b", &join), Err(JoinError::WrongSpace));
    v.admit("team-a", &join).expect("the same approval is good for the space it names");
}

#[test]
fn a_member_cannot_approve_a_revocation_by_admitting_nobody() {
    // Approving a join does not change who is listed as revoked: a join never removes a device.
    let (admin, member, newcomer) = setup();
    let mut v = Verifier::new(admin.public_key());
    v.accept(&MemberList::new(1, vec![device_entry(&admin), device_entry(&member)]).sign(&admin)).unwrap();
    v.admit(SPACE, &Join::request(SPACE, &newcomer).approve(&member, T0)).unwrap();
    assert!(v.is_member(&admin.public_key()) && v.is_member(&member.public_key()),
        "admitting a device leaves every existing member in place");
}
