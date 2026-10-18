//! The stored member credential: owner-only, round-trips, refuses a file others can read, and
//! never leaks its token into an error.
use sesync::team_join::{credential_path, load, save, Credential};
use std::os::unix::fs::PermissionsExt;

fn sandbox(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-join-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cred() -> Credential {
    Credential { space: "personal".into(), member: "member-box".into(), machine: "box-2".into(),
                 token: "tok-0123456789abcdef0123456789abcdef".into(),
                 admin_public: None, signed_list: None, list_version: None }
}

#[test]
fn a_credential_round_trips_and_is_owner_only() {
    let d = sandbox("roundtrip");
    let p = credential_path(&d, "personal");
    save(&p, &cred()).unwrap();
    assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600, "mode 600");
    assert_eq!(load(&p).unwrap(), cred());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_credential_readable_by_others_is_refused() {
    let d = sandbox("loose");
    let p = credential_path(&d, "personal");
    save(&p, &cred()).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = load(&p).unwrap_err();
    assert!(err.contains("readable by other users"), "{err}");
    assert!(!err.contains("tok-"), "the error never shows the token");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_missing_credential_says_how_to_fix_it() {
    let d = sandbox("missing");
    let err = load(&credential_path(&d, "personal")).unwrap_err();
    assert!(err.contains("team join"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_damaged_credential_is_refused_without_echoing_it() {
    let d = sandbox("damaged");
    let p = credential_path(&d, "personal");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "token=tok-secretvalue\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    let err = load(&p).unwrap_err();
    assert!(err.contains("damaged"), "{err}");
    assert!(!err.contains("secretvalue"), "the token is never echoed");
    let _ = std::fs::remove_dir_all(&d);
}
