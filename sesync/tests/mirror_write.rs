//! The chosen-target mirror writer. A session is mirrored only into targets the user picked, and
//! never into its own format. These tests use temp directories only; no real harness store is
//! touched.
use sesync::harness::Locations;
use sesync::mirror_write::{record_mirror, write_mirror, Stores};
use sesync::state::{Source, State};
use std::path::{Path, PathBuf};

/// Every harness under `d`, in the layout `Roots::under` gives: `d/omp/sessions`, `d/pi/sessions`.
fn locations(d: &Path) -> Locations {
    sesync::manage::Roots::under(d, PathBuf::new()).locations
}

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
    let locs = locations(&d);
    let stores = Stores { locations: &locs, home: &d, sesync_conv: Path::new("/nonexistent"), sesync_lock: Path::new("/nonexistent"), archive: &d, lock_dir: &d };
    let err = write_mirror(&stores, &mut state, "/src/a.jsonl", "conv", "t", "omp", Path::new("/src/a.jsonl")).unwrap_err();
    assert!(err.contains("own format"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_unknown_target_is_refused_not_guessed() {
    let d = sandbox("unknown");
    let mut state = State::default();
    state.sources.insert("/src/a.jsonl".into(), Source::new("omp", "/src/a.jsonl"));
    let locs = locations(&d);
    let stores = Stores { locations: &locs, home: &d, sesync_conv: Path::new("/nonexistent"), sesync_lock: Path::new("/nonexistent"), archive: &d, lock_dir: &d };
    // "windsurf" is no harness this daemon knows (a wrong target must be refused, never guessed
    // into a store). The retired "gemini" is one of those now.
    for unknown in ["windsurf", "gemini"] {
        let err = write_mirror(&stores, &mut state, "/src/a.jsonl", "conv", "t", unknown, Path::new("/src/a.jsonl")).unwrap_err();
        assert!(err.contains("no harness named"), "{unknown}: {err}");
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_raw_source_needs_a_journal_before_the_antigravity_writer() {
    let d = sandbox("agy-raw");
    let mut state = State::default();
    state.sources.insert("/src/a.jsonl".into(), Source::new("claude", "/src/a.jsonl"));
    let locs = locations(&d);
    let stores = Stores { locations: &locs, home: &d, sesync_conv: Path::new("/nonexistent"), sesync_lock: Path::new("/nonexistent"), archive: &d, lock_dir: &d };
    // The antigravity writer takes a journal; a raw Claude transcript must be converted first.
    let err = write_mirror(&stores, &mut state, "/src/a.jsonl", "conv", "t", "antigravity", Path::new("/src/a.jsonl")).unwrap_err();
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

/// The install replaces an older copy with the same id in the folder it writes to, so the copy
/// being replaced can be gone before it is recorded over: its key goes too, never pointing at a
/// file that is not there.
#[test]
fn replacing_a_mirror_whose_old_file_is_already_gone_leaves_no_key() {
    let d = sandbox("replace-gone");
    let mut state = State::default();
    state.sources.insert("/src/a.jsonl".into(), Source::new("omp", "/src/a.jsonl"));
    let old = d.join("sessions/-proj/2026-10-01T10-00-00-000Z_mirror.jsonl");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, b"old copy").unwrap();
    record_mirror(&mut state, "/src/a.jsonl", "omp", &old).unwrap();
    std::fs::remove_file(&old).unwrap();
    let new = d.join("sessions/-proj/2026-10-01T10-00-01-000Z_mirror.jsonl");
    std::fs::write(&new, b"new copy").unwrap();
    record_mirror(&mut state, "/src/a.jsonl", "omp", &new).unwrap();
    assert!(!state.mirror_keys.contains(&old.display().to_string()), "no key for a file that is gone");
    assert!(state.mirror_keys.contains(&new.display().to_string()));
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

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

const ORIGINAL: &str = "2026-09-01T00-00-00-000Z_bbbbbbbb-2222-3333-4444-555555555555.jsonl";

/// An omp source and, recorded as its pi copy, the old source a promotion demoted: the user's own
/// conversation, whose bytes sesync never wrote (empty installed_hash).
fn demoted_original(d: &Path) -> (State, String, PathBuf, Vec<u8>) {
    let src = d.join("omp/sessions/-proj/2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, concat!(
        r#"{"type":"session","version":3,"id":"aaaaaaaa-1111-2222-3333-444444444444","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/proj"}"#, "\n",
        r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"user","content":"asked in omp"}}"#, "\n",
    )).unwrap();
    let original = d.join("pi/sessions/--elsewhere-").join(ORIGINAL);
    std::fs::create_dir_all(original.parent().unwrap()).unwrap();
    let bytes = b"{\"type\":\"session\",\"id\":\"bbbbbbbb-2222-3333-4444-555555555555\"}\nthe user's full history, thinking and all\n".to_vec();
    std::fs::write(&original, &bytes).unwrap();
    let key = src.display().to_string();
    let mut state = State::default();
    let mut s = Source::new("omp", &key);
    s.chosen_targets.insert("pi".into());
    s.mirrors.insert("pi".into(), sesync::state::Mirror {
        target: "pi".into(),
        path: original.display().to_string(),
        ..Default::default()
    });
    state.sources.insert(key.clone(), s);
    (state, key, original, bytes)
}

/// Before 2026-10-21 the demoted original was recorded as if sesync had written it, so the next
/// write deleted it and put a converted copy (live window only) in its place. Now its bytes go to
/// the archive first, verified, and the copy keeps the id its harness knows it by.
#[test]
fn a_copy_sesync_did_not_write_is_archived_before_it_is_replaced_in_place() {
    let d = sandbox("keep-original");
    let (mut state, key, original, bytes) = demoted_original(&d);
    let archive = d.join("archive");
    let locs = locations(&d);
    let conv = sesync_conv();
    let stores = Stores { locations: &locs, home: &d, sesync_conv: &conv, sesync_lock: Path::new("/nonexistent"), archive: &archive, lock_dir: &d.join("locks") };
    let installed = PathBuf::from(write_mirror(&stores, &mut state, &key, "conv", "Title", "pi", Path::new(&key)).expect("the write goes ahead"));
    assert!(installed.file_name().unwrap().to_string_lossy().ends_with("_bbbbbbbb-2222-3333-4444-555555555555.jsonl"),
        "the copy keeps its own session id: {}", installed.display());
    let kept: Vec<PathBuf> = std::fs::read_dir(archive.join("aaaaaaaa-1111-2222-3333-444444444444")).unwrap()
        .flatten().map(|e| e.path()).collect();
    assert_eq!(kept.len(), 1, "one archived copy: {kept:?}");
    assert!(kept[0].file_name().unwrap().to_string_lossy().starts_with(ORIGINAL), "{kept:?}");
    assert_eq!(sesync::archive_store::read_session(&kept[0]).unwrap(), bytes, "the archive holds the original bytes");
    assert!(!original.exists(), "the original was replaced, not left as a second session");
    let m = &state.sources[&key].mirrors["pi"];
    assert_eq!(m.installed_hash, sesync::mirror::file_hash(&installed).unwrap(), "from now on the copy is sesync's own");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_copy_that_cannot_be_archived_is_not_replaced() {
    let d = sandbox("keep-refused");
    let (mut state, key, original, bytes) = demoted_original(&d);
    // The archive cannot be created: its place is taken by a file.
    let archive = d.join("archive");
    std::fs::write(&archive, b"not a directory").unwrap();
    let locs = locations(&d);
    let conv = sesync_conv();
    let stores = Stores { locations: &locs, home: &d, sesync_conv: &conv, sesync_lock: Path::new("/nonexistent"), archive: &archive, lock_dir: &d.join("locks") };
    let err = write_mirror(&stores, &mut state, &key, "conv", "Title", "pi", Path::new(&key)).unwrap_err();
    assert!(err.contains("cannot keep a copy"), "{err}");
    assert_eq!(std::fs::read(&original).unwrap(), bytes, "nothing that cannot be restored is replaced");
    assert_eq!(state.sources[&key].mirrors["pi"].installed_hash, "", "the record still says the bytes are not sesync's");
    let _ = std::fs::remove_dir_all(&d);
}
