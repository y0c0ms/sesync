//! The mirror cleanup. A mirror goes only when that loses nothing: sesync wrote it, it is still
//! exactly what sesync wrote, and its source is still on disk. Everything else is kept.

use sesync::lock::ClaimOutcome;
use sesync::manage::{delete_group, Group, Roots};
use sesync::mirror::file_hash;
use sesync::mirror_cleanup::{apply, plan, Verdict};
use sesync::state::{Mirror, Source, State};
use std::fs;
use std::path::{Path, PathBuf};

const MISSING_CLIENT: &str = "/no/such/sesync-lock";

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-mirrorclean-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// A source in the omp store with one mirror in the pi store, recorded the way sesync records it.
fn recorded_pair(sb: &Path, body: &str) -> (String, PathBuf, PathBuf) {
    let src = sb.join("omp").join("sessions").join("src.jsonl");
    let mir = sb.join("pi").join("sessions").join("mir.jsonl");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::create_dir_all(mir.parent().unwrap()).unwrap();
    fs::write(&src, body).unwrap();
    fs::write(&mir, body).unwrap();
    (src.display().to_string(), src, mir)
}

fn state_with_mirror(key: &str, mirror: &Path, installed_hash: &str) -> State {
    let mut state = State::default();
    let mut src = Source::new("omp", key);
    src.mirrors.insert("pi".into(), Mirror {
        target: "pi".into(),
        path: mirror.display().to_string(),
        source_hash: String::new(),
        installed_hash: installed_hash.into(),
        diverged: false,
    });
    state.sources.insert(key.into(), src);
    state.mirror_keys.insert(mirror.display().to_string());
    state
}

#[test]
fn an_unchanged_mirror_with_its_source_present_is_removed() {
    let sb = sandbox("unchanged");
    let (key, _src, mir) = recorded_pair(&sb, "session body\n");
    let mut state = state_with_mirror(&key, &mir, &file_hash(&mir).unwrap());

    let planned = plan(&state);
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].verdict, Verdict::Remove);
    assert!(mir.exists(), "planning changes nothing");

    let log = apply(&mut state, &planned, &sb.join("locks"), Path::new(MISSING_CLIENT));
    assert!(!mir.exists(), "the unchanged mirror is removed");
    assert!(state.sources[&key].mirrors.is_empty(), "its state record is cleared");
    assert!(!state.mirror_keys.contains(&mir.display().to_string()));
    assert!(log.iter().any(|l| l.starts_with("removed pi")), "{log:?}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_mirror_edited_after_sesync_wrote_it_is_kept() {
    let sb = sandbox("edited");
    let (key, _src, mir) = recorded_pair(&sb, "session body\n");
    let recorded = file_hash(&mir).unwrap();
    fs::write(&mir, "session body\nand the user kept going here\n").unwrap();
    let mut state = state_with_mirror(&key, &mir, &recorded);

    let planned = plan(&state);
    assert!(matches!(&planned[0].verdict, Verdict::Keep(why) if why.contains("newer version")), "{:?}", planned[0].verdict);
    apply(&mut state, &planned, &sb.join("locks"), Path::new(MISSING_CLIENT));
    assert!(mir.exists(), "a mirror with a newer version is never deleted");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_mirror_whose_source_is_gone_is_kept_as_a_possible_only_copy() {
    let sb = sandbox("orphan");
    let (key, src, mir) = recorded_pair(&sb, "session body\n");
    let recorded = file_hash(&mir).unwrap();
    fs::remove_file(&src).unwrap();
    let mut state = state_with_mirror(&key, &mir, &recorded);

    let planned = plan(&state);
    assert!(matches!(&planned[0].verdict, Verdict::Keep(why) if why.contains("source is missing")), "{:?}", planned[0].verdict);
    apply(&mut state, &planned, &sb.join("locks"), Path::new(MISSING_CLIENT));
    assert!(mir.exists());
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_mirror_already_gone_only_has_its_record_cleared() {
    let sb = sandbox("gone");
    let (key, _src, mir) = recorded_pair(&sb, "session body\n");
    let recorded = file_hash(&mir).unwrap();
    fs::remove_file(&mir).unwrap();
    let mut state = state_with_mirror(&key, &mir, &recorded);

    let planned = plan(&state);
    assert_eq!(planned[0].verdict, Verdict::Gone);
    apply(&mut state, &planned, &sb.join("locks"), Path::new(MISSING_CLIENT));
    assert!(state.sources[&key].mirrors.is_empty());
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_mirror_without_a_recorded_hash_is_kept() {
    let sb = sandbox("nohash");
    let (key, _src, mir) = recorded_pair(&sb, "session body\n");
    let state = state_with_mirror(&key, &mir, "");
    let planned = plan(&state);
    assert!(matches!(&planned[0].verdict, Verdict::Keep(why) if why.contains("no recorded hash")));
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn an_opencode_mirror_is_never_removed_by_file() {
    let sb = sandbox("opencode");
    let (key, _src, mir) = recorded_pair(&sb, "session body\n");
    let mut state = State::default();
    let mut src = Source::new("omp", &key);
    src.mirrors.insert("opencode".into(), Mirror {
        target: "opencode".into(),
        path: "ses_example".into(),
        source_hash: String::new(),
        installed_hash: String::new(),
        diverged: false,
    });
    state.sources.insert(key.clone(), src);
    let planned = plan(&state);
    assert!(matches!(&planned[0].verdict, Verdict::Keep(why) if why.contains("OpenCode")));
    apply(&mut state, &planned, &sb.join("locks"), Path::new(MISSING_CLIENT));
    assert!(mir.exists(), "no file of ours is touched for an OpenCode mirror");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_file_sesync_never_recorded_is_not_in_the_plan() {
    let sb = sandbox("unrecorded");
    let (key, _src, mir) = recorded_pair(&sb, "session body\n");
    let state = State::default();
    assert!(plan(&state).is_empty(), "only recorded mirrors are candidates");
    assert!(mir.exists(), "the file is untouched because it was never proven to be ours");
    let _ = key;
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_mirror_that_changes_between_plan_and_apply_is_kept() {
    let sb = sandbox("race");
    let (key, _src, mir) = recorded_pair(&sb, "session body\n");
    let mut state = state_with_mirror(&key, &mir, &file_hash(&mir).unwrap());
    let planned = plan(&state);
    assert_eq!(planned[0].verdict, Verdict::Remove);
    fs::write(&mir, "changed after planning\n").unwrap();

    let log = apply(&mut state, &planned, &sb.join("locks"), Path::new(MISSING_CLIENT));
    assert!(mir.exists(), "the hash is checked again under the claim, so the newer file survives");
    assert!(log.iter().any(|l| l.contains("changed since it was planned")), "{log:?}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn deleting_a_session_removes_only_that_file_and_leaves_its_mirror() {
    let sb = sandbox("delete");
    let (key, src, mir) = recorded_pair(&sb, "session body\n");
    let roots = Roots {
        omp: sb.join("omp").join("sessions"),
        pi: sb.join("pi").join("sessions"),
        claude: sb.join("claude").join("projects"),
    };
    let mut group = Group::file("omp", key.clone(), "t");
    group.mirrors.push(("pi".into(), mir.display().to_string()));

    let outcome = delete_group(&roots, &group, &sb.join("locks"), Path::new(MISSING_CLIENT));
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(!src.exists(), "the picked session is removed");
    assert!(mir.exists(), "its mirror in another harness is not touched by delete");
    let _ = ClaimOutcome::Ran(()); // the claim type is part of the public contract
    let _ = fs::remove_dir_all(&sb);
}
