//! The chosen-target mirror writer. A session is mirrored only into targets the user picked, and
//! never into its own format. These tests use temp directories only; no real harness store is
//! touched.
use sesync::mirror_write::{record_mirror, write_mirror, Stores};
use sesync::state::{Source, State};
use std::path::{Path, PathBuf};

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-mirror-write-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn a_target_must_not_be_the_sources_own_format() {
    let d = sandbox("own");
    let mut state = State::default();
    state.sources.insert("/src/a.jsonl".into(), Source::new("omp", "/src/a.jsonl"));
    let stores = Stores { omp: &d, pi: &d, claude: &d, sesync_conv: Path::new("/nonexistent"), sesync_lock: Path::new("/nonexistent") };
    let err = write_mirror(&stores, &mut state, "/src/a.jsonl", "omp", "conv", "t", "omp").unwrap_err();
    assert!(err.contains("own format"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_unknown_target_is_refused_not_guessed() {
    let d = sandbox("unknown");
    let mut state = State::default();
    state.sources.insert("/src/a.jsonl".into(), Source::new("omp", "/src/a.jsonl"));
    let stores = Stores { omp: &d, pi: &d, claude: &d, sesync_conv: Path::new("/nonexistent"), sesync_lock: Path::new("/nonexistent") };
    let err = write_mirror(&stores, &mut state, "/src/a.jsonl", "omp", "conv", "t", "gemini").unwrap_err();
    assert!(err.contains("no file mirror writer"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_recorded_mirror_points_at_the_installed_file_and_is_tracked() {
    let d = sandbox("record");
    let mut state = State::default();
    let mut src = Source::new("omp", "/src/a.jsonl");
    src.last_hash = "abc".into();
    state.sources.insert("/src/a.jsonl".into(), src);
    let installed = d.join("copy.jsonl");
    std::fs::write(&installed, b"journal").unwrap();
    record_mirror(&mut state, "/src/a.jsonl", "pi", &installed).unwrap();
    let m = &state.sources["/src/a.jsonl"].mirrors["pi"];
    assert_eq!(m.path, installed.display().to_string());
    assert_eq!(m.source_hash, "abc", "the mirror remembers the source hash it was built from");
    assert!(!m.diverged);
    assert!(state.mirror_keys.contains(&installed.display().to_string()), "the file is marked as a mirror");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_source_with_no_chosen_target_is_never_counted_as_wanting_a_mirror() {
    // The daemon's `wanted` is the number of chosen targets. With the default empty set, a
    // source wants zero mirrors, so the scan never asks for a write.
    let src = Source::new("omp", "/src/a.jsonl");
    assert!(src.chosen_targets.is_empty(), "nothing is chosen by default");
    assert_eq!(src.chosen_targets.len(), 0, "wanted mirrors is zero by default");
}

#[test]
fn chosen_targets_survive_a_save_and_load_round_trip() {
    let d = sandbox("state");
    let path = d.join("state.json");
    let mut state = State::default();
    let mut src = Source::new("omp", "/src/a.jsonl");
    src.chosen_targets.insert("pi".into());
    src.chosen_targets.insert("claude".into());
    state.sources.insert("/src/a.jsonl".into(), src);
    state.save(&path).unwrap();
    let back = State::load(&path).unwrap();
    let chosen: Vec<&String> = back.sources["/src/a.jsonl"].chosen_targets.iter().collect();
    assert_eq!(chosen, vec!["claude", "pi"], "choices survive a restart");
    let _ = std::fs::remove_dir_all(&d);
}
