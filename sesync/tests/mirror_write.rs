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
    let stores = Stores { home: &d, omp: &d, pi: &d, claude: &d, gemini: &d, opencode_bin: Path::new("/nonexistent"), opencode_db: Path::new("/nonexistent"), sesync_conv: Path::new("/nonexistent"), sesync_lock: Path::new("/nonexistent") };
    let err = write_mirror(&stores, &mut state, "/src/a.jsonl", "omp", "conv", "t", "omp").unwrap_err();
    assert!(err.contains("own format"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_unknown_target_is_refused_not_guessed() {
    let d = sandbox("unknown");
    let mut state = State::default();
    state.sources.insert("/src/a.jsonl".into(), Source::new("omp", "/src/a.jsonl"));
    let stores = Stores { home: &d, omp: &d, pi: &d, claude: &d, gemini: &d, opencode_bin: Path::new("/nonexistent"), opencode_db: Path::new("/nonexistent"), sesync_conv: Path::new("/nonexistent"), sesync_lock: Path::new("/nonexistent") };
    // "windsurf" is no target this daemon knows, unlike gemini which is one since the
    // Gemini CLI wiring (a wrong target must be refused, never guessed into a store).
    let err = write_mirror(&stores, &mut state, "/src/a.jsonl", "omp", "conv", "t", "windsurf").unwrap_err();
    assert!(err.contains("no file mirror writer"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_raw_source_needs_a_journal_before_the_gemini_writer() {
    let d = sandbox("gemini-raw");
    let mut state = State::default();
    state.sources.insert("/src/a.jsonl".into(), Source::new("claude", "/src/a.jsonl"));
    let stores = Stores { home: &d, omp: &d, pi: &d, claude: &d, gemini: &d, opencode_bin: Path::new("/nonexistent"), opencode_db: Path::new("/nonexistent"), sesync_conv: Path::new("/nonexistent"), sesync_lock: Path::new("/nonexistent") };
    // The gemini writer takes a journal; a raw Claude transcript must be converted first.
    let err = write_mirror(&stores, &mut state, "/src/a.jsonl", "claude", "conv", "t", "gemini").unwrap_err();
    assert!(err.contains("must be converted to a journal"), "{err}");
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
fn replacing_a_mirror_at_a_new_path_removes_the_old_file() {
    // The stale-bucket migration: a rewrite into the right place must take the old copy
    // with it, but only when it is still exactly what sesync wrote.
    let d = sandbox("replace-path");
    let mut state = State::default();
    state.sources.insert("/src/a.jsonl".into(), Source::new("omp", "/src/a.jsonl"));
    let old = d.join("sessions/--/mirror.jsonl");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, b"old copy").unwrap();
    record_mirror(&mut state, "/src/a.jsonl", "omp", &old).unwrap();
    let new = d.join("sessions/-proj/mirror.jsonl");
    std::fs::create_dir_all(new.parent().unwrap()).unwrap();
    std::fs::write(&new, b"new copy").unwrap();
    record_mirror(&mut state, "/src/a.jsonl", "omp", &new).unwrap();
    assert!(!old.exists(), "the old copy must not be left behind");
    assert!(new.exists(), "the new copy must stay");
    assert!(!state.mirror_keys.contains(&old.display().to_string()), "the old path stops being a mirror key");
    assert!(state.mirror_keys.contains(&new.display().to_string()));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn replacing_a_mirror_keeps_an_edited_old_file() {
    let d = sandbox("replace-edited");
    let mut state = State::default();
    state.sources.insert("/src/a.jsonl".into(), Source::new("omp", "/src/a.jsonl"));
    let old = d.join("sessions/--/mirror.jsonl");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, b"as written").unwrap();
    record_mirror(&mut state, "/src/a.jsonl", "omp", &old).unwrap();
    std::fs::write(&old, b"someone edited this").unwrap();
    let new = d.join("sessions/-proj/mirror.jsonl");
    std::fs::create_dir_all(new.parent().unwrap()).unwrap();
    std::fs::write(&new, b"new copy").unwrap();
    record_mirror(&mut state, "/src/a.jsonl", "omp", &new).unwrap();
    assert!(old.exists(), "a file changed since sesync wrote it must never be deleted");
    assert!(state.mirror_keys.contains(&old.display().to_string()), "it must stay a mirror key so it is not read as a source");
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
