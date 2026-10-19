//! One session on more than one team: the shares, the per-team bookkeeping, the floors and the
//! pull dedupe. All state-level: no server, no files beyond a temp state path.
use sesync::state::{State, team_key};
use sesync::team_pull_verified::{PullOutcome, PulledRecord};
use sesync::team_watch::{TeamPull, TeamWatch};

fn outcome(conv: &str, text: &[u8], seq: u64) -> PullOutcome {
    let mut accepted = std::collections::BTreeMap::new();
    accepted.insert(conv.to_string(), seq);
    PullOutcome {
        shown: vec![PulledRecord { conversation: conv.to_string(), text: text.to_vec() }],
        accepted,
        ..Default::default()
    }
}

#[test]
fn a_session_can_be_shared_with_more_than_one_team() {
    let mut st = State::default();
    st.share("conv-1", "team-a");
    st.share("conv-1", "team-b");
    assert_eq!(st.shares_of("conv-1"), ["team-a".to_string(), "team-b".to_string()].into_iter().collect());
    assert!(st.is_shared_with("conv-1", "team-a") && st.is_shared_with("conv-1", "team-b"));

    // Being dropped from one team does not touch the other: the session stays shared with team-b
    // and the local copy is untouched (revocation is forward-only and per team).
    st.unshare("conv-1", "team-a");
    assert!(!st.is_shared_with("conv-1", "team-a"), "no longer shared with team-a");
    assert!(st.is_shared_with("conv-1", "team-b"), "still shared with team-b");
    assert_eq!(st.sessions_shared_with("team-a"), Vec::<String>::new());
    assert_eq!(st.sessions_shared_with("team-b"), vec!["conv-1".to_string()]);

    // Dropping the last team removes the share entirely.
    st.unshare("conv-1", "team-b");
    assert!(!st.is_shared("conv-1"));
    assert!(st.shares_of("conv-1").is_empty());
}

#[test]
fn the_bookkeeping_is_per_team() {
    let mut st = State::default();
    st.record_team_push("team-a", "conv-1", "hash-a", 100.0);
    st.record_team_push("team-b", "conv-1", "hash-b", 200.0);
    assert_eq!(st.team_hash("team-a", "conv-1").as_deref(), Some("hash-a"));
    assert_eq!(st.team_hash("team-b", "conv-1").as_deref(), Some("hash-b"));
    assert_eq!(st.team_pushed_at("team-a", "conv-1"), Some(100.0));
    assert_eq!(st.team_pushed_at("team-b", "conv-1"), Some(200.0));
    assert_eq!(st.team_hash("team-c", "conv-1"), None, "an unknown team has no record");

    // Floors are per team too: a record accepted under one team never raises another's floor.
    st.raise_seen("team-a", "conv-1", 5);
    assert_eq!(st.seen_floor("team-a", "conv-1"), 5);
    assert_eq!(st.seen_floor("team-b", "conv-1"), 0, "team-b's floor is untouched");
    // A lower sequence never lowers a floor.
    st.raise_seen("team-a", "conv-1", 3);
    assert_eq!(st.seen_floor("team-a", "conv-1"), 5);

    let all = st.floors_all();
    assert_eq!(all["team-a"]["conv-1"], 5);
    assert!(!all.contains_key("team-b"), "team-b holds no floor for this conversation");
}

#[test]
fn the_display_keeps_one_entry_per_conversation_and_the_newest_wins() {
    // The same conversation on two teams, each with its own record and sequence. The local view
    // holds one entry, and it is the newest statement of the two.
    let older = TeamPull { space: "team-a".into(), out: outcome("conv-1", b"older bytes", 3) };
    let newer = TeamPull { space: "team-b".into(), out: outcome("conv-1", b"newer bytes", 5) };
    let shown = TeamWatch::newest_per_conversation(&[older, newer.clone()]);
    assert_eq!(shown.len(), 1, "one entry per conversation");
    assert_eq!(shown[0].1, b"newer bytes", "the higher sequence wins");

    // Reversed order: the outcome must not depend on which team was pulled first.
    let shown = TeamWatch::newest_per_conversation(&[newer, TeamPull {
        space: "team-a".into(), out: outcome("conv-1", b"older bytes", 3) }]);
    assert_eq!(shown[0].1, b"newer bytes");

    // A conversation only one team holds is shown once, and distinct ones all appear.
    let only_a = TeamPull { space: "team-a".into(), out: outcome("conv-2", b"only a", 1) };
    let shown = TeamWatch::newest_per_conversation(&[TeamPull {
        space: "team-a".into(), out: outcome("conv-1", b"newer bytes", 5) }, only_a]);
    assert_eq!(shown.len(), 2);
}

#[test]
fn a_legacy_marked_state_migrates_to_the_machines_one_team() {
    let dir = std::env::temp_dir().join(format!("sesync-multi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.json");
    // A pre-multi-team state: a flat marked list and conversation-keyed team records.
    std::fs::write(&path, serde_json::json!({
        "sources": {},
        "mirror_keys": [],
        "marked": ["conv-1", "conv-2"],
        "team": {"conv-1": {"hash": "h", "pushed_at": 10.0}},
        "team_seen": {"conv-1": 4},
    }).to_string()).unwrap();
    let mut st = State::load(&path).unwrap();
    assert_eq!(st.shared_sessions().len(), 0, "nothing is shared before the teams are known");
    st.migrate_legacy_team_state(&["the-team".to_string()]);
    assert_eq!(st.shares_of("conv-1"), ["the-team".to_string()].into_iter().collect());
    assert_eq!(st.shares_of("conv-2"), ["the-team".to_string()].into_iter().collect());
    assert_eq!(st.team_hash("the-team", "conv-1").as_deref(), Some("h"),
        "the push record is re-keyed to the team");
    assert_eq!(st.seen_floor("the-team", "conv-1"), 4, "the floor is re-keyed too");
    assert!(st.team_seen.contains_key(&team_key("the-team", "conv-1")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_on_two_teams_is_converted_once_per_pass() {
    // The operator's efficiency rule: pulling/pushing for one team must not repeat the work for
    // the other. The canonical bytes of a conversation are built once and reused.
    use sesync::team_pass::CanonCache;
    let calls = std::cell::Cell::new(0);
    let mut cache = CanonCache::default();
    let mut build = |_id: &str| {
        calls.set(calls.get() + 1);
        Ok(b"the canonical journal".to_vec())
    };
    let a = cache.get("conv-1", |id| build(id)).unwrap();
    let b = cache.get("conv-1", |id| build(id)).unwrap();
    assert_eq!(a, b);
    assert_eq!(calls.get(), 1, "the conversation is converted once for both teams");
    let _ = cache.get("conv-2", |id| build(id)).unwrap();
    assert_eq!(calls.get(), 2, "a different conversation is its own conversion");
}
