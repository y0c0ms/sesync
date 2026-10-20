//! The mirror cleanup. A mirror goes only when that loses nothing: sesync wrote it, it is still
//! exactly what sesync wrote, and its source is still on disk. Everything else is kept.

use sesync::lock::ClaimOutcome;
use sesync::manage::{delete_group, Group, Roots};
use sesync::mirror::file_hash;
use sesync::mirror_cleanup::{apply, plan, plan_unchosen, release, Verdict};
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
        edit_stamp: String::new(),
        target_updated: 0.0,
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
        edit_stamp: String::new(),
        target_updated: 0.0,
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
        gemini: sb.join("gemini").join("tmp"),
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

// ---- the mirrors the user no longer wants --------------------------------------------------
//
// Un-choosing a target is how a session stops being mirrored somewhere. Nothing else drops that
// copy: `plan` only sees what is still recorded, and it declines for OpenCode by design ("remove
// it through OpenCode, not by file"). A pass now drops them, and these pin the rule.

#[test]
fn an_unchosen_file_mirror_is_planned_for_removal_and_forgotten() {
    let sb = sandbox("unchosen");
    let (key, _src, mir) = recorded_pair(&sb, "session body\n");
    let mut state = state_with_mirror(&key, &mir, &file_hash(&mir).unwrap());
    assert!(state.sources[&key].chosen_targets.is_empty(), "no target is chosen, by construction");

    let planned = plan_unchosen(&state);
    assert_eq!(planned.len(), 1, "the un-chosen mirror is planned");
    assert_eq!(planned[0].verdict, Verdict::Remove);
    assert!(mir.exists(), "planning changes nothing");

    let log = apply(&mut state, &planned, &sb.join("locks"), Path::new(MISSING_CLIENT));
    assert!(log.iter().any(|l| l.starts_with("removed")), "log: {log:?}");
    assert!(!mir.exists(), "the copy is gone");
    assert!(state.sources[&key].mirrors.is_empty(), "the record is gone");
    assert!(
        !state.mirror_keys.contains(&mir.display().to_string()),
        "and so is its mirror_keys entry, so the scan never re-adopts it"
    );
}

#[test]
fn a_chosen_mirror_is_never_in_the_unchosen_plan() {
    let sb = sandbox("chosen");
    let (key, _src, mir) = recorded_pair(&sb, "session body\n");
    let mut state = state_with_mirror(&key, &mir, &file_hash(&mir).unwrap());
    state.sources.get_mut(&key).unwrap().chosen_targets.insert("pi".into());

    assert!(plan_unchosen(&state).is_empty(), "a chosen target is left alone");
    assert!(mir.exists(), "and its copy is untouched");
}

#[test]
fn an_un_chosen_opencode_mirror_is_handed_to_the_cli_and_kept_by_the_file_rule() {
    let sb = sandbox("unchosen-opencode");
    let key = sb.join("omp").join("sessions").join("src.jsonl").display().to_string();
    fs::create_dir_all(Path::new(&key).parent().unwrap()).unwrap();
    fs::write(&key, "session body\n").unwrap();

    let mut state = State::default();
    let mut src = Source::new("omp", &key);
    src.mirrors.insert("opencode".into(), Mirror {
        target: "opencode".into(),
        // For OpenCode the record's path is the session id, not a file: there is nothing to delete.
        path: "ses_003220097fe04c7bvvm4CnHYO6".into(),
        source_hash: String::new(),
        installed_hash: String::new(),
        diverged: false,
        edit_stamp: String::new(),
        target_updated: 0.0,
    });
    state.sources.insert(key.clone(), src);

    let planned = plan_unchosen(&state);
    assert_eq!(planned.len(), 1, "an un-chosen OpenCode mirror is planned too");
    assert_eq!(
        planned[0].verdict,
        Verdict::Keep("OpenCode session: remove it through OpenCode, not by file".into()),
        "the file rule never touches it; the caller removes it through the CLI"
    );
}

/// A copy that cannot be proven unchanged is kept, but its record is released. Keeping the record
/// would leave sesync pointing at a mirror it will never update again, shown in the TUI forever -
/// which is the state the operator hit after un-ticking a target whose record had no hash.
#[test]
fn a_mirror_that_cannot_be_proven_unchanged_is_released_not_deleted() {
    let sb = sandbox("release");
    let (key, _src, mir) = recorded_pair(&sb, "session body\n");
    let mut state = state_with_mirror(&key, &mir, ""); // no recorded hash

    let planned = plan_unchosen(&state);
    assert_eq!(
        planned[0].verdict,
        Verdict::Keep("no recorded hash, so it cannot be shown unchanged".into())
    );

    let log = release(&mut state, &planned);
    assert!(mir.exists(), "the copy itself is kept");
    assert!(state.sources[&key].mirrors.is_empty(), "but the record is gone");
    assert!(
        !state.mirror_keys.contains(&mir.display().to_string()),
        "and its mirror_keys entry, so the file can be a session of its own again"
    );
    assert!(log[0].starts_with("released"), "log: {log:?}");
}
