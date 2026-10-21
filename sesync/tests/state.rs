//! State file, ported from daemon/sesync.py State.load / State.save.
//! Must read and write the same JSON layout the Python daemon uses, so an
//! existing state.json keeps working after the switch.

use sesync::state::{Mirror, Source, State};
use std::fs;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("hs-state-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

const PY_STATE: &str = r#"{
 "sources": {
  "omp:/x/a.jsonl": {
   "fmt": "omp", "key": "/x/a.jsonl", "last_seen_change": 12.5,
   "last_hash": "abc", "last_stamp": "10:99", "failures": 1,
   "mirrors": {
    "pi": {"target": "pi", "path": "/p/a.jsonl", "source_hash": "abc", "installed_hash": "def", "diverged": true}
   }
  }
 },
 "mirror_keys": ["/p/a.jsonl", "ses_1"]
}"#;

#[test]
fn loads_the_python_state_layout() {
    let d = tmp("load");
    let f = d.join("state.json");
    fs::write(&f, PY_STATE).unwrap();
    let s = State::load(&f).unwrap();
    let src = &s.sources["omp:/x/a.jsonl"];
    assert_eq!(src.fmt, "omp");
    assert_eq!(src.failures, 1);
    assert_eq!(src.last_failure, 0.0, "an older file without the field is still valid");
    assert_eq!(src.last_hash, "abc");
    let m = &src.mirrors["pi"];
    assert_eq!(m.path, "/p/a.jsonl");
    assert!(m.diverged, "diverged flag must survive a load");
    assert!(s.mirror_keys.contains("ses_1"));
}

#[test]
fn missing_file_is_an_empty_state() {
    let d = tmp("missing");
    let s = State::load(&d.join("nope.json")).unwrap();
    assert!(s.sources.is_empty());
    assert!(s.mirror_keys.is_empty());
}

#[test]
fn save_then_load_round_trips() {
    let d = tmp("roundtrip");
    let f = d.join("state.json");
    let mut s = State::default();
    let mut src = Source::new("opencode", "ses_9");
    src.last_hash = "h".into();
    src.failures = 2;
    src.last_failure = 1_234.5;
    src.mirrors.insert("claude".into(), Mirror { target: "claude".into(), path: "/c/x.jsonl".into(),
        source_hash: "h".into(), installed_hash: String::new(), diverged: false,
        edit_stamp: String::new(), target_updated: 0.0 });
    s.sources.insert("opencode:ses_9".into(), src);
    s.mirror_keys.insert("/c/x.jsonl".into());
    s.save(&f).unwrap();
    let back = State::load(&f).unwrap();
    assert_eq!(back.sources["opencode:ses_9"].mirrors["claude"].path, "/c/x.jsonl");
    assert_eq!(back.sources["opencode:ses_9"].last_failure, 1_234.5);
    assert_eq!(back.sources["opencode:ses_9"].failures, 2);
    assert!(back.mirror_keys.contains("/c/x.jsonl"));
}

#[test]
fn save_writes_sorted_mirror_keys_and_leaves_no_tmp_file() {
    let d = tmp("sorted");
    let f = d.join("state.json");
    let mut s = State::default();
    s.mirror_keys.insert("zeta".into());
    s.mirror_keys.insert("alpha".into());
    s.save(&f).unwrap();
    let text = fs::read_to_string(&f).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["mirror_keys"], serde_json::json!(["alpha", "zeta"]));
    assert!(!d.join("state.tmp").exists(), "atomic write must not leave a tmp file");
}

#[test]
fn corrupt_state_is_an_error_not_silently_empty() {
    // Silently dropping state would forget every mirror and risk duplicate writes.
    let d = tmp("corrupt");
    let f = d.join("state.json");
    fs::write(&f, "{not json").unwrap();
    assert!(State::load(&f).is_err());
}

#[test]
fn subagent_transcripts_are_recorded_under_their_parent_and_round_trip() {
    let d = tmp("subagents");
    let f = d.join("state.json");
    let mut s = State::default();
    s.subagents
        .entry("ec447302-155c-4280-9c34-2b5122fcd4a4".into())
        .or_default()
        .push("/projects/-proj/ec447302/subagents/agent-a1.jsonl".into());
    s.save(&f).unwrap();
    let back = State::load(&f).unwrap();
    assert_eq!(back.subagents["ec447302-155c-4280-9c34-2b5122fcd4a4"],
               vec!["/projects/-proj/ec447302/subagents/agent-a1.jsonl".to_string()]);
}

#[test]
fn a_state_file_written_before_subagents_existed_still_loads() {
    // The state layout must stay compatible: an older file has no "subagents" key.
    let d = tmp("old-state");
    let f = d.join("state.json");
    fs::write(&f, PY_STATE).unwrap();
    let s = State::load(&f).unwrap();
    assert!(s.subagents.is_empty());
    assert!(s.sources.contains_key("omp:/x/a.jsonl"));
}

#[test]
fn the_team_sequence_floors_round_trip_and_default_to_empty() {
    // The highest team record sequence accepted per conversation survives a restart, so an old
    // record cannot be replayed after the daemon has been restarted.
    let d = tmp("team-seen");
    let f = d.join("state.json");
    let mut s = State::default();
    s.raise_seen("team-a", "ses_a".into(), 7);
    s.raise_seen("team-a", "ses_b".into(), 1);
    s.save(&f).unwrap();
    let back = State::load(&f).unwrap();
    assert_eq!(back.seen_floor("team-a", "ses_a"), 7);
    assert_eq!(back.seen_floor("team-a", "ses_b"), 1);
    assert_eq!(back.team_seen.len(), 2);
}

#[test]
fn a_state_file_written_before_team_floors_existed_has_none() {
    let d = tmp("no-team-seen");
    let f = d.join("state.json");
    fs::write(&f, PY_STATE).unwrap();
    let s = State::load(&f).unwrap();
    assert!(s.team_seen.is_empty(), "the optional key defaults to empty");
}

#[test]
fn a_tui_save_keeps_everything_the_daemon_recorded() {
    let dir = tmp("tui-keeps-daemon");
    let path = dir.join("state.json");
    let mut daemon = State::default();
    let mut src = Source::new("omp", "/omp/s.jsonl");
    src.mirrors.insert("pi".into(), Mirror {
        target: "pi".into(), path: "/pi/m.jsonl".into(), ..Default::default()
    });
    daemon.sources.insert("/omp/s.jsonl".into(), src);
    daemon.mirror_keys.insert("/pi/m.jsonl".into());
    daemon.raise_seen("team-a", "ses_x".into(), 7);
    daemon.save(&path).unwrap();

    // Exactly what the TUI does: load, set the choice, save only the user-owned fields.
    let mut tui = State::load(&path).unwrap();
    tui.sources.get_mut("/omp/s.jsonl").unwrap().chosen_targets.insert("pi".into());
    tui.save_user(&path).unwrap();

    let after = State::load(&path).unwrap();
    assert!(after.mirror_keys.contains("/pi/m.jsonl"), "the daemon mirror record survives a TUI save");
    assert_eq!(after.seen_floor("team-a", "ses_x"), 7, "the team floor survives");
    assert!(after.sources["/omp/s.jsonl"].chosen_targets.contains("pi"), "the choice is written");
    assert_eq!(after.sources["/omp/s.jsonl"].mirrors.len(), 1, "the recorded mirror survives");
}

#[test]
fn the_daemon_adopts_a_choice_made_while_it_ran() {
    let dir = tmp("adopt-choice");
    let path = dir.join("state.json");
    let mut tui = State::default();
    let mut src = Source::new("omp", "/omp/s.jsonl");
    // The app marks the entries it creates (save_chosen_targets): only those may be added to the
    // file, since any other entry missing there was removed by the daemon.
    src.tui_created = true;
    src.chosen_targets.insert("pi".into());
    tui.sources.insert("/omp/s.jsonl".into(), src);
    tui.share("/omp/s.jsonl", "team-a");
    tui.save_user(&path).unwrap();

    let mut daemon = State::default();
    daemon.mirror_keys.insert("/old/m.jsonl".into());
    daemon.absorb_user_edits(&path).unwrap();
    assert!(daemon.sources["/omp/s.jsonl"].chosen_targets.contains("pi"), "the daemon sees the choice");
    assert!(daemon.is_shared("/omp/s.jsonl"), "the daemon sees the mark");
    assert!(daemon.mirror_keys.contains("/old/m.jsonl"), "the daemon keeps its own records");

    daemon.save(&path).unwrap();
    let after = State::load(&path).unwrap();
    assert!(after.sources["/omp/s.jsonl"].chosen_targets.contains("pi"), "the choice survives the daemon save");
    assert!(after.mirror_keys.contains("/old/m.jsonl"), "and the daemon record is there");
}

/// The loop seen on 2026-10-21. A promotion demoted an OpenCode source to a mirror, and the next
/// merge adopted the old entry back from the file the daemon had not saved over yet: the
/// conversation was then both the source and the mirror of its own copy, and the two entries
/// promoted each other on every pass, deleting and re-importing the OpenCode session each time.
#[test]
fn an_entry_the_daemon_removed_is_not_adopted_back_from_disk() {
    let d = tmp("no-readopt");
    let f = d.join("state.json");
    let mut before = State::default();
    let mut oc = Source::new("opencode", "ses_original");
    oc.chosen_targets.insert("antigravity".into());
    oc.mirrors.insert("antigravity".into(), Mirror { target: "antigravity".into(), path: "/a/copy.db".into(), ..Default::default() });
    before.sources.insert("ses_original".into(), oc);
    before.save(&f).unwrap();

    // The daemon loads that file; then a promotion moves the conversation to the Antigravity copy.
    let mut daemon = State::load(&f).unwrap();
    let mut g = Source::new("antigravity", "/a/copy.db");
    g.chosen_targets.insert("opencode".into());
    g.mirrors.insert("opencode".into(), Mirror { target: "opencode".into(), path: "ses_original".into(), ..Default::default() });
    daemon.sources.remove("ses_original");
    daemon.sources.insert("/a/copy.db".into(), g);

    // Meanwhile the TUI recorded a choice for a session nobody has scanned yet.
    let mut on_disk = State::load(&f).unwrap();
    let mut fresh = Source::new("omp", "/o/new.jsonl");
    fresh.chosen_targets.insert("pi".into());
    on_disk.sources.insert("/o/new.jsonl".into(), fresh);
    on_disk.save(&f).unwrap();

    daemon.absorb_user_edits(&f).unwrap();
    assert!(!daemon.sources.contains_key("ses_original"), "the demoted source must stay a mirror");
    assert_eq!(daemon.sources.len(), 2, "the promoted source and the TUI's new choice");
    assert!(daemon.sources["/o/new.jsonl"].chosen_targets.contains("pi"), "a new choice is still adopted");
}

/// The TUI's half of the same bug: an app that still held entries the daemon had removed saved
/// them back on its next keystroke.
#[test]
fn the_tui_never_saves_back_an_entry_the_daemon_removed() {
    let d = tmp("no-resurrect");
    let f = d.join("state.json");
    let mut daemon = State::default();
    let mut g = Source::new("antigravity", "/a/copy.db");
    g.mirrors.insert("opencode".into(), Mirror { target: "opencode".into(), path: "ses_original".into(), ..Default::default() });
    daemon.sources.insert("/a/copy.db".into(), g);
    daemon.save(&f).unwrap();

    let mut tui = State::default();
    // Loaded before the promotion: the daemon has since removed it.
    let mut stale = Source::new("omp", "/o/demoted.jsonl");
    stale.chosen_targets.insert("claude".into());
    tui.sources.insert("/o/demoted.jsonl".into(), stale);
    // Created by the app, but it is a copy the daemon keeps.
    let mut copy = Source::new("opencode", "ses_original");
    copy.tui_created = true;
    copy.chosen_targets.insert("antigravity".into());
    tui.sources.insert("ses_original".into(), copy);
    // Created by the app for a session the daemon has not scanned: this one belongs on disk.
    let mut mine = Source::new("omp", "/o/new.jsonl");
    mine.tui_created = true;
    mine.chosen_targets.insert("pi".into());
    tui.sources.insert("/o/new.jsonl".into(), mine);
    tui.save_user(&f).unwrap();

    let after = State::load(&f).unwrap();
    assert!(!after.sources.contains_key("/o/demoted.jsonl"), "an entry the daemon removed stays removed");
    assert!(!after.sources.contains_key("ses_original"), "a copy the daemon keeps never becomes a source");
    assert!(after.sources["/o/new.jsonl"].chosen_targets.contains("pi"), "the app's own new choice is saved");
    assert!(after.sources["/a/copy.db"].mirrors.contains_key("opencode"), "the daemon's records survive");
}
