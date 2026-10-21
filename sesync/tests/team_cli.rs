//! The team CLI's argument rules and its token-file rules. The network call is exercised by the
//! admin HTTP tests; these cover what the operator types and what the file may contain.

use sesync::team_cli::{parse, read_admin_token, Command};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn a(xs: &[&str]) -> Vec<String> { xs.iter().map(|s| s.to_string()).collect() }

#[test]
fn create_takes_a_name_and_optional_members() {
    assert_eq!(parse(&a(&["create", "personal"])).unwrap(),
               Command::Create { name: "personal".into(), members: vec![] });
    assert_eq!(parse(&a(&["create", "personal", "--member", "admin-box", "--member", "member-box"])).unwrap(),
               Command::Create { name: "personal".into(), members: vec!["admin-box".into(), "member-box".into()] });
}

#[test]
fn invite_takes_exactly_a_team_and_a_member() {
    assert_eq!(parse(&a(&["invite", "personal", "member-box"])).unwrap(),
               Command::Invite { name: "personal".into(), member: "member-box".into() });
    assert!(parse(&a(&["invite", "personal"])).is_err(), "member is required");
    assert!(parse(&a(&["invite", "personal", "a", "b"])).is_err(), "extra argument refused");
}

#[test]
fn roster_takes_exactly_a_team() {
    assert_eq!(parse(&a(&["roster", "personal"])).unwrap(), Command::Roster { name: "personal".into() });
    assert!(parse(&a(&["roster"])).is_err(), "name is required");
    assert!(parse(&a(&["roster", "personal", "extra"])).is_err(), "extra argument refused");
}

#[test]
fn approve_takes_a_team_member_machine_and_an_optional_fingerprint() {
    assert_eq!(parse(&a(&["approve", "personal", "alice", "box-2"])).unwrap(),
               Command::Approve { name: "personal".into(), member: "alice".into(), machine: "box-2".into(), fingerprint: None });
    assert_eq!(parse(&a(&["approve", "personal", "alice", "box-2", "--fingerprint", "a1b2-c3d4-e5f6-a7b8"])).unwrap(),
               Command::Approve { name: "personal".into(), member: "alice".into(), machine: "box-2".into(),
                                  fingerprint: Some("a1b2-c3d4-e5f6-a7b8".into()) });
    assert!(parse(&a(&["approve", "personal", "alice"])).is_err(), "machine is required");
    assert!(parse(&a(&["approve", "personal", "alice", "box-2", "--fingerprint"])).is_err(), "--fingerprint needs a value");
    assert!(parse(&a(&["approve", "personal", "alice", "box-2", "extra"])).is_err(), "extra argument refused");
}

#[test]
fn decline_and_revoke_take_a_team_a_member_and_a_machine() {
    assert_eq!(parse(&a(&["decline", "personal", "alice", "box-2"])).unwrap(),
               Command::Decline { name: "personal".into(), member: "alice".into(), machine: "box-2".into() });
    assert_eq!(parse(&a(&["revoke", "personal", "alice", "box-2"])).unwrap(),
               Command::Revoke { name: "personal".into(), member: "alice".into(), machine: "box-2".into() });
    for verb in ["decline", "revoke"] {
        assert!(parse(&a(&[verb, "personal", "alice"])).is_err(), "{verb}: machine is required");
        assert!(parse(&a(&[verb, "personal", "alice", "box-2", "extra"])).is_err(), "{verb}: extra argument refused");
    }
}

#[test]
fn remove_takes_a_team_and_a_member() {
    assert_eq!(parse(&a(&["remove", "personal", "alice"])).unwrap(),
               Command::Remove { name: "personal".into(), member: "alice".into() });
    assert!(parse(&a(&["remove", "personal"])).is_err(), "member is required");
    assert!(parse(&a(&["remove", "personal", "alice", "box-2"])).is_err(), "extra argument refused");
}

#[test]
fn publish_and_forget_are_gone() {
    // Revocation is a list change made with revoke or remove; nothing signs enrolled devices in bulk.
    assert!(parse(&a(&["publish", "personal"])).is_err());
    assert!(parse(&a(&["forget", "personal", "box-2"])).is_err());
}

#[test]
fn mistakes_return_the_usage_line() {
    assert!(parse(&a(&[])).unwrap_err().starts_with("usage:"));
    assert!(parse(&a(&["create"])).is_err(), "name is required");
    assert!(parse(&a(&["create", "personal", "--bogus"])).is_err());
    assert!(parse(&a(&["create", "personal", "--member"])).is_err(), "--member needs a value");
}

#[test]
fn the_token_file_must_be_owner_only() {
    let dir = std::env::temp_dir().join(format!("sesync-tok-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("admin.token");
    std::fs::write(&f, "x".repeat(40)).unwrap();
    #[cfg(unix)]
    {
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_admin_token(&f).is_err(), "a world-readable token file is refused");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert_eq!(read_admin_token(&f).unwrap().len(), 40, "owner-only file is accepted");
    std::fs::write(&f, "short").unwrap();
    assert!(read_admin_token(&f).is_err(), "a short token is refused");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_token_file_is_an_error_that_names_no_secret() {
    let err = read_admin_token(std::path::Path::new("/nonexistent/admin.token")).unwrap_err();
    assert!(!err.contains("/nonexistent"), "the error does not echo the path: {err}");
}
