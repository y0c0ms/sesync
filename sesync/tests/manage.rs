//! Session management, ported from daemon/hs_manage.py. Rules pinned here:
//!   * a title is 1-80 characters, trimmed, with no control characters;
//!   * a delete or rename only touches files under the session roots;
//!   * mirrors are removed before the source;
//!   * the conversation is locked for the whole operation.
//! Runs against private temp stores; the real stores are never touched.

use sesync::manage::{delete_group, list_groups, list_groups_cached, rename_group, validate_title,
                      Group, Roots, TitleCache};
use std::fs;
use std::path::PathBuf;

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-manage-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn roots(sb: &PathBuf) -> Roots {
    Roots {
        omp: sb.join("omp").join("sessions"),
        pi: sb.join("pi").join("sessions"),
        claude: sb.join("claude").join("projects"),
        gemini: sb.join("gemini").join("tmp"),
    }
}

#[test]
fn titles_are_trimmed_and_length_checked() {
    assert_eq!(validate_title("  New name  ").unwrap(), "New name");
    assert!(validate_title("   ").is_err());
    assert!(validate_title("").is_err());
    assert!(validate_title(&"x".repeat(80)).is_ok());
    assert!(validate_title(&"x".repeat(81)).is_err());
}

#[test]
fn control_characters_are_refused() {
    for bad in ["bad\u{7}char", "line\nbreak", "tab\there", "del\u{7f}"] {
        assert!(validate_title(bad).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn a_path_outside_the_roots_is_refused() {
    let sb = sandbox("outside");
    let r = roots(&sb);
    fs::create_dir_all(&r.pi).unwrap();
    let outside = sb.join("elsewhere.jsonl");
    fs::write(&outside, "x").unwrap();
    let g = Group::file("pi", outside.display().to_string(), "t");
    assert!(delete_group(&r, &g, &sb.join("locks"), &sb.join("no-sesync_lock")).is_err());
    assert!(outside.exists(), "a refused delete must not touch the file");
}

#[test]
fn deleting_removes_only_the_picked_session_and_keeps_its_mirror() {
    let sb = sandbox("order");
    let r = roots(&sb);
    let dir = r.omp.join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(r.pi.join("-tmp")).unwrap();
    let src = dir.join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    let mirror = r.pi.join("-tmp").join("2026-10-01T10-00-01-000Z_bbbbbbbb-1111-2222-3333-444444444444.jsonl");
    fs::write(&src, "source").unwrap();
    fs::write(&mirror, "mirror").unwrap();
    let mut g = Group::file("omp", src.display().to_string(), "t");
    g.add_mirror("pi", mirror.display().to_string());
    let log = delete_group(&r, &g, &sb.join("locks"), &sb.join("no-sesync_lock")).unwrap();
    assert!(!src.exists(), "the picked session is removed");
    assert!(mirror.exists(), "its copy in another harness is not touched by delete");
    assert_eq!(log.len(), 1, "only the source is in the log: {log:?}");
}

#[test]
fn rename_changes_only_the_title_record() {
    let sb = sandbox("rename");
    let r = roots(&sb);
    let dir = r.omp.join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    let src = dir.join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    let body = "{\"type\":\"session\",\"version\":3,\"id\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"cwd\":\"/tmp\",\"title\":\"Old\"}\n{\"type\":\"message\",\"id\":\"m1\",\"message\":{\"role\":\"user\",\"content\":\"keep this\"}}\n";
    fs::write(&src, body).unwrap();
    let g = Group::file("omp", src.display().to_string(), "Old");
    let saved = rename_group(&r, &g, "  New name  ", &sb.join("locks"), &sb.join("no-sesync_lock")).unwrap();
    assert_eq!(saved, "New name");
    let after = fs::read_to_string(&src).unwrap();
    assert!(after.contains("keep this"), "message records must be untouched");
}

#[test]
fn list_shows_sources_without_their_mirrors() {
    let sb = sandbox("list");
    let r = roots(&sb);
    fs::create_dir_all(r.omp.join("-tmp")).unwrap();
    fs::write(r.omp.join("-tmp").join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl"), "x").unwrap();
    let groups = list_groups(&r, &sesync::state::State::default());
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].fmt, "omp");
}

#[test]
fn list_is_newest_first() {
    let sb = sandbox("newest-first");
    let r = roots(&sb);
    let dir = r.omp.join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    let make = |id: &str, secs: u64| {
        let p = dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl"));
        fs::write(&p, "x").unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        fs::File::options().write(true).open(&p).unwrap()
            .set_times(fs::FileTimes::new().set_modified(t)).unwrap();
        id.to_string()
    };
    let old = make("aaaaaaaa-1111-2222-3333-444444444444", 1_000_000);
    let newest = make("bbbbbbbb-1111-2222-3333-444444444444", 3_000_000);
    let middle = make("cccccccc-1111-2222-3333-444444444444", 2_000_000);
    let groups = list_groups(&r, &sesync::state::State::default());
    let ids: Vec<String> = groups.iter().map(|g| sesync::manage::session_id_of(&g.key)).collect();
    assert_eq!(ids, vec![newest, middle, old], "sessions must be newest first");
}

#[test]
fn harness_side_renames_are_read_for_every_format() {
    let sb = sandbox("harness-rename");
    let r = roots(&sb);
    let omp_dir = r.omp.join("-tmp");
    fs::create_dir_all(&omp_dir).unwrap();
    let omp = omp_dir.join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    fs::write(&omp, "{\"type\":\"session\",\"version\":3,\"id\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"cwd\":\"/tmp\",\"title\":\"Old omp\"}\n{\"type\":\"title\",\"title\":\"Renamed by omp\"}\n").unwrap();
    let pi_dir = r.pi.join("-tmp");
    fs::create_dir_all(&pi_dir).unwrap();
    let pi = pi_dir.join("2026-10-01T10-00-00-000Z_bbbbbbbb-1111-2222-3333-444444444444.jsonl");
    fs::write(&pi, "{\"type\":\"session_info\",\"name\":\"Renamed by pi\"}\n").unwrap();
    let claude_dir = r.claude.join("-tmp");
    fs::create_dir_all(&claude_dir).unwrap();
    let claude = claude_dir.join("cccccccc-1111-2222-3333-444444444444.jsonl");
    fs::write(&claude, "{\"type\":\"custom-title\",\"customTitle\":\"Renamed by claude\",\"sessionId\":\"cccccccc-1111-2222-3333-444444444444\"}\n").unwrap();
    let groups = list_groups(&r, &sesync::state::State::default());
    let title = |fmt: &str| groups.iter().find(|g| g.fmt == fmt).unwrap().title.clone();
    assert_eq!(title("omp"), "Renamed by omp");
    assert_eq!(title("pi"), "Renamed by pi");
    assert_eq!(title("claude"), "Renamed by claude");
}

#[test]
fn tui_rename_writes_the_native_record_for_every_format() {
    let sb = sandbox("rename-native");
    let r = roots(&sb);
    let omp_dir = r.omp.join("-tmp");
    fs::create_dir_all(&omp_dir).unwrap();
    let omp = omp_dir.join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    fs::write(&omp, "{\"type\":\"title\",\"v\":1,\"title\":\"Old\",\"source\":\"auto\",\"updatedAt\":\"2026-10-01T09:00:00.000Z\",\"pad\":\"\"}\n{\"type\":\"session\",\"version\":3,\"id\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"cwd\":\"/tmp\",\"title\":\"Old\"}\n").unwrap();
    let pi_dir = r.pi.join("-tmp");
    fs::create_dir_all(&pi_dir).unwrap();
    let pi = pi_dir.join("2026-10-01T10-00-00-000Z_bbbbbbbb-1111-2222-3333-444444444444.jsonl");
    fs::write(&pi, "{\"type\":\"session\",\"version\":3,\"id\":\"bbbbbbbb-1111-2222-3333-444444444444\",\"cwd\":\"/tmp\"}\n").unwrap();
    let claude_dir = r.claude.join("-tmp");
    fs::create_dir_all(&claude_dir).unwrap();
    let claude = claude_dir.join("cccccccc-1111-2222-3333-444444444444.jsonl");
    fs::write(&claude, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n").unwrap();
    let lock = sb.join("locks");
    let no_lock = sb.join("no-sesync_lock");
    for (fmt, path) in [("omp", &omp), ("pi", &pi), ("claude", &claude)] {
        let g = Group::file(fmt, path.display().to_string(), "Old");
        rename_group(&r, &g, "New Name", &lock, &no_lock).unwrap();
    }
    let omp_after = fs::read_to_string(&omp).unwrap();
    let first = omp_after.lines().next().unwrap();
    assert_eq!(first.as_bytes().len(), 255, "the slot is 255 bytes + newline, as omp writes it");
    let slot: serde_json::Value = serde_json::from_str(first).unwrap();
    assert_eq!(slot["type"], "title");
    assert_eq!(slot["title"], "New Name");
    assert_eq!(slot["source"], "user");
    assert_eq!(slot["updatedAt"].as_str().unwrap().len(), 24, "omp's ISO-ms stamp");
    let last: serde_json::Value = serde_json::from_str(omp_after.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "title_change", "omp records a rename as a title_change entry");
    assert_eq!(last["title"], "New Name");
    assert_eq!(last["previousTitle"], "Old");
    assert_eq!(last["source"], "user");
    assert_eq!(last["parentId"], "aaaaaaaa-1111-2222-3333-444444444444", "chained to the entry before it");
    assert!(fs::read_to_string(&pi).unwrap().contains("\"session_info\""));
    assert!(fs::read_to_string(&pi).unwrap().contains("\"name\":\"New Name\""));
    assert!(fs::read_to_string(&claude).unwrap().contains("\"custom-title\""));
    assert!(fs::read_to_string(&claude).unwrap().contains("\"customTitle\":\"New Name\""));
    let groups = list_groups(&r, &sesync::state::State::default());
    for g in &groups {
        assert_eq!(g.title, "New Name", "{} must read back the new name", g.fmt);
    }
}

#[test]
fn big_transcripts_are_titled_from_their_head_or_tail() {
    // Titles are windowed for large files (first and last 64 KiB). A journal header at the
    // head, and a rename at the tail, must both be found in a file too big to read whole
    // on every TUI refresh — reading whole files is what made the interface lag.
    let sb = sandbox("big-titles");
    let r = roots(&sb);
    let filler: String = (0..4000)
        .map(|i| format!("{{\"type\":\"message\",\"id\":\"m{i}\",\"message\":{{\"role\":\"user\",\"content\":\"filler line {i} ......................\"}}}}\n"))
        .collect();
    assert!(filler.len() > 256 * 1024, "the test file must exceed the whole-read limit");
    let omp = r.omp.join("-tmp");
    fs::create_dir_all(&omp).unwrap();
    fs::write(omp.join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl"),
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"cwd\":\"/tmp\",\"title\":\"Head title\"}}\n{filler}")).unwrap();
    let claude = r.claude.join("-tmp");
    fs::create_dir_all(&claude).unwrap();
    fs::write(claude.join("cccccccc-1111-2222-3333-444444444444.jsonl"),
        format!("{filler}{{\"type\":\"custom-title\",\"customTitle\":\"Tail rename\",\"sessionId\":\"cccccccc-1111-2222-3333-444444444444\"}}\n")).unwrap();
    let groups = list_groups(&r, &sesync::state::State::default());
    let title = |fmt: &str| groups.iter().find(|g| g.fmt == fmt).unwrap().title.clone();
    assert_eq!(title("omp"), "Head title", "an omp header in the head window must be read");
    assert_eq!(title("claude"), "Tail rename", "a Claude rename in the tail window must win");
}

#[test]
fn the_title_cache_re_reads_a_file_only_when_it_changed() {
    let sb = sandbox("title-cache");
    let r = roots(&sb);
    let dir = r.omp.join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    let src = dir.join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    fs::write(&src, "{\"type\":\"session\",\"version\":3,\"id\":\"aaaaaaaa-1111-2222-3333-444444444444\",\"cwd\":\"/tmp\",\"title\":\"Before\"}\n").unwrap();
    let mut cache = TitleCache::default();
    let before = list_groups_cached(&r, &sesync::state::State::default(), &mut cache);
    assert_eq!(before[0].title, "Before");
    let g = Group::file("omp", src.display().to_string(), "Before");
    rename_group(&r, &g, "After Rename", &sb.join("locks"), &sb.join("no-sesync_lock")).unwrap();
    let after = list_groups_cached(&r, &sesync::state::State::default(), &mut cache);
    assert_eq!(after[0].title, "After Rename", "a changed file must not be served from the cache");
}
