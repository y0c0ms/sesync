//! A pre-multi-team state file carries a `marked` list. The daemon folds it into shares at
//! startup and then runs `absorb_user_edits` on every pass, which takes shares from disk. The fold
//! must be on disk first, or the first pass discards it and the session is never pushed.

use sesync::state::State;
use std::fs;

fn sandbox(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sesync-legacy-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

const CONV: &str = "aaaaaaaa-1111-2222-3333-444444444444";

#[test]
fn a_legacy_marked_share_survives_the_first_absorb_once_saved() {
    let dir = sandbox("saved");
    let path = dir.join("state.json");
    fs::write(&path, format!("{{\"sources\":{{}},\"marked\":[\"{CONV}\"],\"mirror_keys\":[]}}")).unwrap();

    let mut state = State::load(&path).unwrap();
    state.migrate_legacy_team_state(&["personal".to_string()]);
    assert!(state.is_shared_with(CONV, "personal"), "the fold must share the legacy id");
    state.save(&path).unwrap();

    state.absorb_user_edits(&path).unwrap();
    assert!(state.is_shared_with(CONV, "personal"),
        "a saved fold must survive absorb_user_edits");
}
