//! The daemon's push step across several teams, against a real in-process team server.
//! `team_pass::run` reads the team environment, so this file holds a single test and sets that
//! environment for itself; no other test shares the process.
#[path = "support/team_server.rs"]
mod support;

use sesync::device_keys::load_or_create;
use sesync::state::State;
use sesync::team_join::{self, credential_path, trust_of};
use sesync::team_watch::TeamWatch;
use std::collections::BTreeMap;
use support::{sandbox, TeamServer};

#[test]
fn one_damaged_or_left_team_does_not_stop_the_push_into_the_others() {
    let srv = TeamServer::start(1_000_020);
    for k in ["SESYNC_TEAM_SPACE", "SESYNC_TEAM_TOKEN", "SESYNC_TEAM_MACHINE", "SESYNC_TEAM_QUIET"] {
        std::env::remove_var(k);
    }
    std::env::set_var("SESYNC_TEAM_URL", &srv.base);

    let admin = sandbox("pass-admin");
    let wcfg = sandbox("pass-writer");
    let rcfg = sandbox("pass-reader");
    for space in ["alpha", "beta"] {
        srv.create(&admin, space, &["writer", "reader"]);
        srv.admit(&admin, &wcfg, space, "writer", "writer-box");
    }
    srv.admit(&admin, &rcfg, "alpha", "reader", "reader-box");
    // beta's credential is damaged on disk (a crash mid-edit, a bad copy); gamma was left: the
    // session is still shared with it, but its credential is gone.
    std::fs::write(credential_path(&wcfg, "beta"), "token=\n").unwrap();
    let mut state = State::default();
    for space in ["alpha", "beta", "gamma"] {
        state.share("ses_1", space);
    }

    let now = 2_000_000_000.0;
    let passes = sesync::team_pass::run(&wcfg, &mut state, now, |_| Ok(b"the shared session".to_vec()), |_| Some(now - 1_000.0))
        .expect("the healthy team is still pushed");
    let pushed: Vec<&String> = passes.iter().flat_map(|p| p.pushed.iter()).collect();
    assert_eq!(pushed, vec!["ses_1"], "pushed once, into alpha");
    assert!(state.team_hash("alpha", "ses_1").is_some());

    let cred = team_join::load(&credential_path(&rcfg, "alpha")).unwrap();
    let watch = TeamWatch {
        base: srv.base.clone(),
        token: cred.token.clone(),
        machine: cred.machine.clone(),
        space: "alpha".into(),
        device: load_or_create(&rcfg).unwrap(),
        trust: trust_of(&cred),
    };
    let out = watch.pull_all(&BTreeMap::new());
    assert_eq!(out.shown.len(), 1, "the reader in alpha gets it");
    assert_eq!(out.shown[0].text, b"the shared session");
}
