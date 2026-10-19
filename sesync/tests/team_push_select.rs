//! What a team push may send. The rule is the operator's: only marked sessions leave the
//! machine. Unmarked sessions, subagent transcripts and anything not in the team never go.
use sesync::state::State;
use sesync::team_push::select_for_push;

#[test]
fn only_marked_sessions_are_selected() {
    let mut st = State::default();
    st.share("ses_a", "team-a");
    st.share("ses_c", "team-a");
    let all = vec!["ses_a".to_string(), "ses_b".to_string(), "ses_c".to_string()];
    assert_eq!(select_for_push(&st, "team-a", &all), vec!["ses_a".to_string(), "ses_c".to_string()]);
}

#[test]
fn nothing_is_selected_when_nothing_is_marked() {
    let st = State::default();
    let all = vec!["ses_a".to_string()];
    assert!(select_for_push(&st, "team-a", &all).is_empty(), "no marks, no push");
}

#[test]
fn a_marked_session_that_no_longer_exists_is_not_selected() {
    let mut st = State::default();
    st.share("ses_gone", "team-a");
    let all = vec!["ses_a".to_string()];
    assert!(select_for_push(&st, "team-a", &all).is_empty(), "only sessions that exist can be pushed");
}
