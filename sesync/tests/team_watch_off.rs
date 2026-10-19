//! The team side of --watch is off unless configured. With no team environment, nothing is
//! configured and no network or key file is touched.
use sesync::team_watch::TeamWatch;

#[test]
fn with_no_team_environment_the_team_side_is_off() {
    for v in ["SESYNC_TEAM_URL", "SESYNC_TEAM_TOKEN", "SESYNC_TEAM_MACHINE", "SESYNC_TEAM_SPACE"] {
        std::env::remove_var(v);
    }
    let dir = std::env::temp_dir().join(format!("sesync-watch-off-{}", std::process::id()));
    assert!(TeamWatch::from_env(&dir).is_none(), "no configuration, no team side");
    assert!(!dir.join("device").exists(), "no device key is created when the team side is off");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_partial_team_environment_is_still_off() {
    std::env::set_var("SESYNC_TEAM_URL", "http://127.0.0.1:1");
    std::env::remove_var("SESYNC_TEAM_TOKEN");
    let dir = std::env::temp_dir().join(format!("sesync-watch-part-{}", std::process::id()));
    assert!(TeamWatch::from_env(&dir).is_none(), "missing the token means off");
    std::env::remove_var("SESYNC_TEAM_URL");
    let _ = std::fs::remove_dir_all(&dir);
}
