//! Retiring the Gemini CLI (replaced by Antigravity on 2026-06-18). sesync's own records move:
//! a Gemini source is re-keyed to its best surviving copy, Gemini targets and copies are released,
//! and Gemini paths leave `mirror_keys`. A Gemini file on disk is never touched.

use sesync::harness::{Retired, RETIRED};
use sesync::state::{Mirror, Source, State};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-retire-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn mirror(target: &str, path: &str) -> Mirror {
    Mirror { target: target.into(), path: path.into(), installed_hash: "h".into(), ..Default::default() }
}

/// The retired Gemini CLI, as the daemon retires it at every load.
fn gemini() -> &'static Retired {
    RETIRED.iter().find(|r| r.id == "gemini").expect("gemini is a retired harness")
}

const GEM: &str = "/home/u/.gemini/tmp/proj/chats/session-2026-10-01T10-00-abc12345.jsonl";
const OMP: &str = "/home/u/.omp/agent/sessions/-proj/2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl";
const PI: &str = "/home/u/.pi/agent/sessions/--proj--/2026-10-01T10-00-00-000Z_bbbbbbbb-1111-2222-3333-444444444444.jsonl";
const AGY: &str = "/home/u/.gemini/antigravity-cli/conversations/cccccccc-1111-2222-3333-444444444444.db";

/// A Gemini source mirrored into omp, Pi and Antigravity, shared with a team, plus an omp source
/// that chose Gemini as a target.
fn gemini_world() -> State {
    let mut st = State::default();
    let mut g = Source::new("gemini", GEM);
    g.conv = "gemini-conv-id".into();
    g.title = "Plan the move".into();
    g.turn_at = 42.0;
    g.chosen_targets = ["omp", "pi", "antigravity", "gemini"].iter().map(|s| s.to_string()).collect();
    for (t, p) in [("omp", OMP), ("pi", PI), ("antigravity", AGY)] {
        g.mirrors.insert(t.into(), mirror(t, p));
    }
    st.sources.insert(GEM.into(), g);
    st.mirror_keys.extend([OMP, PI, AGY].map(String::from));
    st.share("gemini-conv-id", "team-a");

    let other = "/home/u/.omp/agent/sessions/-x/2026-10-02T10-00-00-000Z_dddddddd-1111-2222-3333-444444444444.jsonl";
    let gem_copy = "/home/u/.gemini/tmp/x/chats/session-2026-10-02T10-00-ddd11111.jsonl";
    let mut o = Source::new("omp", other);
    o.chosen_targets = ["gemini", "claude"].iter().map(|s| s.to_string()).collect();
    o.mirrors.insert("gemini".into(), mirror("gemini", gem_copy));
    st.sources.insert(other.into(), o);
    st.mirror_keys.insert(gem_copy.into());
    // A Gemini file an older binary demoted and left recorded, attached to no source.
    st.mirror_keys.insert("/home/u/.gemini/tmp/y/chats/session-2026-09-01T10-00-eee22222.jsonl".into());
    st
}

#[test]
fn a_gemini_source_moves_to_its_omp_copy_keeping_the_conversation() {
    let mut st = gemini_world();
    let log = st.retire_harness(gemini(), |_, _| true);
    assert!(!log.is_empty());
    assert!(!st.sources.contains_key(GEM), "the gemini entry is gone");
    let s = &st.sources[OMP];
    assert_eq!(s.fmt, "omp", "omp is preferred over pi and antigravity");
    assert_eq!(s.key, OMP);
    assert_eq!(s.conv, "gemini-conv-id", "the conversation id stays: mirror ids, locks and the team keep it");
    assert_eq!(s.title, "Plan the move");
    assert_eq!(s.turn_at, 42.0);
    assert_eq!(s.chosen_targets, ["antigravity", "pi"].iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        "chosen loses gemini and the new source's own format");
    assert_eq!(s.mirrors.keys().cloned().collect::<BTreeSet<_>>(), ["antigravity", "pi"].iter().map(|s| s.to_string()).collect(),
        "the copy that became the source is no mirror of itself");
    assert_eq!(s.mirrors["pi"].path, PI, "the other copies stay recorded, replaced in place later");
    assert!(st.is_shared_with("gemini-conv-id", "team-a"), "shares follow the conversation id");
    assert!(!st.mirror_keys.contains(OMP), "the new source is not a mirror key, or the scan would skip it");
    assert!(st.mirror_keys.contains(PI) && st.mirror_keys.contains(AGY), "the other copies are still copies");
    assert!(!st.is_recorded_mirror(OMP));
}

#[test]
fn gemini_targets_and_copies_are_released_everywhere() {
    let mut st = gemini_world();
    st.retire_harness(gemini(), |_, _| true);
    for (key, s) in &st.sources {
        assert!(!s.chosen_targets.contains("gemini"), "{key} still chooses gemini");
        assert!(!s.mirrors.contains_key("gemini"), "{key} still records a gemini copy");
    }
    let other = st.sources.values().find(|s| s.key.contains("dddddddd")).unwrap();
    assert!(other.chosen_targets.contains("claude"), "other choices stay");
    assert!(st.mirror_keys.iter().all(|k| !k.contains("/.gemini/tmp/")), "no gemini path is left: {:?}", st.mirror_keys);
    assert!(st.mirror_keys.contains(AGY), "an antigravity path under ~/.gemini is not a gemini session");
}

#[test]
fn retirement_is_idempotent() {
    let mut st = gemini_world();
    st.retire_harness(gemini(), |_, _| true);
    let keys: BTreeSet<String> = st.sources.keys().cloned().collect();
    let mk = st.mirror_keys.clone();
    assert!(st.retire_harness(gemini(), |_, _| true).is_empty(), "a second run changes nothing");
    assert_eq!(st.sources.keys().cloned().collect::<BTreeSet<_>>(), keys);
    assert_eq!(st.mirror_keys, mk);
}

#[test]
fn a_missing_copy_is_skipped_for_the_next_harness_in_order() {
    let mut st = gemini_world();
    st.retire_harness(gemini(), |t, p| !(t == "omp" && p == OMP));
    assert!(!st.sources.contains_key(OMP), "the omp copy is gone, it cannot be the source");
    let s = &st.sources[PI];
    assert_eq!(s.fmt, "pi");
    assert!(s.mirrors.contains_key("omp"), "the missing omp copy stays recorded so the next pass rewrites it");
    assert!(s.chosen_targets.contains("omp") && !s.chosen_targets.contains("pi"));

    let mut st = gemini_world();
    st.retire_harness(gemini(), |t, _| t == "antigravity");
    assert_eq!(st.sources[AGY].fmt, "antigravity", "antigravity is the last resort");
}

#[test]
fn a_gemini_source_with_no_surviving_copy_is_dropped() {
    let mut st = gemini_world();
    let log = st.retire_harness(gemini(), |_, _| false);
    assert!(!st.sources.contains_key(GEM));
    assert!(st.sources.values().all(|s| s.fmt == "omp" && s.key.contains("dddddddd")), "only the other source is left");
    assert!(log.iter().any(|l| l.contains("dropped")), "{log:?}");
}

#[test]
fn a_copy_that_already_is_a_source_is_not_taken_over() {
    let mut st = gemini_world();
    let mut live = Source::new("omp", OMP);
    live.chosen_targets.insert("claude".into());
    st.sources.insert(OMP.into(), live.clone());
    st.retire_harness(gemini(), |_, _| true);
    assert_eq!(st.sources[OMP].chosen_targets, live.chosen_targets, "the live entry keeps its own records");
    assert_eq!(st.sources[PI].fmt, "pi", "the next copy in order takes the gemini conversation");
}

/// The real daemon: a state left by the Gemini era is retired at load, and the Gemini files on
/// disk keep every byte.
#[test]
fn the_daemon_retires_gemini_records_and_never_touches_gemini_files() {
    let sb = tmp("daemon");
    let gem_dir = sb.join("home").join(".gemini").join("tmp").join("proj").join("chats");
    fs::create_dir_all(&gem_dir).unwrap();
    let gem_src = gem_dir.join("session-2026-10-01T10-00-abc12345.jsonl");
    let gem_copy = gem_dir.join("session-2026-10-02T10-00-ddd11111.jsonl");
    fs::write(&gem_src, "{\"sessionId\":\"x\"}\n").unwrap();
    fs::write(&gem_copy, "{\"sessionId\":\"y\"}\n").unwrap();
    let omp_dir = sb.join("omp").join("sessions").join("-proj");
    fs::create_dir_all(&omp_dir).unwrap();
    let omp_copy = omp_dir.join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    fs::write(&omp_copy, "{\"type\":\"session\",\"version\":3,\"id\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"cwd\":\"/proj\"}\n").unwrap();
    let other = omp_dir.join("2026-10-02T10-00-00-000Z_dddddddd-1111-2222-3333-444444444444.jsonl");
    fs::write(&other, "{\"type\":\"session\",\"version\":3,\"id\":\"dddddddd-1111-2222-3333-444444444444\",\"cwd\":\"/proj\"}\n").unwrap();

    let [gs, gc, oc, ot] = [&gem_src, &gem_copy, &omp_copy, &other].map(|p| p.display().to_string());
    let mut st = State::default();
    let mut g = Source::new("gemini", &gs);
    g.conv = "gemini-conv-id".into();
    g.chosen_targets = ["omp", "gemini"].iter().map(|s| s.to_string()).collect();
    g.mirrors.insert("omp".into(), mirror("omp", &oc));
    st.sources.insert(gs.clone(), g);
    let mut o = Source::new("omp", &ot);
    o.chosen_targets.insert("gemini".into());
    o.mirrors.insert("gemini".into(), mirror("gemini", &gc));
    st.sources.insert(ot.clone(), o);
    st.mirror_keys.extend([oc.clone(), gc.clone()]);
    let state_file = sb.join("state").join("state.json");
    st.save(&state_file).unwrap();
    let before: Vec<Vec<u8>> = [&gem_src, &gem_copy].iter().map(|p| fs::read(p).unwrap()).collect();

    let o = Command::new(env!("CARGO_BIN_EXE_sesync"))
        .arg("--once")
        .env("HOME", sb.join("home"))
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("ANTIGRAVITY_CLI_HOME", sb.join("antigravity"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", sb.join("no-opencode-bin"))
        .env("SESYNC_CONV", sb.join("no-converter"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .env("SESYNC_MAX_AGE_DAYS", "3650")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{err}");
    assert!(err.contains("moved the gemini session"), "the retirement is reported: {err}");

    for (p, b) in [&gem_src, &gem_copy].iter().zip(&before) {
        assert_eq!(&fs::read(p).unwrap(), b, "{} keeps every byte", p.display());
    }
    let after = State::load(&state_file).unwrap();
    assert!(!after.sources.contains_key(&gs), "the gemini source is retired on disk too");
    let moved = &after.sources[&oc];
    assert_eq!((moved.fmt.as_str(), moved.conv.as_str()), ("omp", "gemini-conv-id"));
    assert!(moved.chosen_targets.is_empty(), "nothing but gemini and itself were chosen");
    assert!(!after.sources[&ot].chosen_targets.contains("gemini"), "an older choice of gemini is released");
    assert!(!after.sources[&ot].mirrors.contains_key("gemini"));
    assert!(!after.mirror_keys.contains(&gc) && !after.mirror_keys.contains(&oc), "{:?}", after.mirror_keys);
    let _ = fs::remove_dir_all(&sb);
}
