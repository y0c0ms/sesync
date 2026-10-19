//! Marking sessions for the team. Only marked sessions are eligible to push. Marks are per
//! session, can be undone, and survive a restart. A state file written before marks existed
//! loads with nothing marked.
use sesync::state::State;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-marks-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn nothing_is_marked_by_default() {
    let st = State::default();
    assert!(!st.is_shared("ses_1"));
    assert!(st.shared_sessions().is_empty());
}

#[test]
fn a_session_can_be_marked_and_unmarked() {
    let mut st = State::default();
    st.share("ses_1", "team-a");
    assert!(st.is_shared("ses_1"));
    assert!(!st.is_shared("ses_2"), "marking is per session");
    st.unshare("ses_1", "team-a");
    assert!(!st.is_shared("ses_1"), "a mark can be undone");
}

#[test]
fn marks_survive_a_restart() {
    let d = tmp("restart");
    let p = d.join("state.json");
    let mut st = State::default();
    st.share("ses_keep", "team-a");
    st.save(&p).unwrap();
    let back = State::load(&p).unwrap();
    assert!(back.is_shared("ses_keep"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_old_state_file_loads_with_nothing_marked() {
    let d = tmp("old");
    let p = d.join("state.json");
    std::fs::write(&p, r#"{"sources": {}, "mirror_keys": [], "subagents": {}}"#).unwrap();
    let st = State::load(&p).expect("old state loads");
    assert!(st.shared_sessions().is_empty());
    let _ = std::fs::remove_dir_all(&d);
}
