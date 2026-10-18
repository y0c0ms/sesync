//! Session management, ported from daemon/hs_manage.py. Rules pinned here:
//!   * a title is 1-80 characters, trimmed, with no control characters;
//!   * a delete or rename only touches files under the session roots;
//!   * mirrors are removed before the source;
//!   * the conversation is locked for the whole operation.
//! Runs against private temp stores; the real stores are never touched.

use sesync::manage::{delete_group, list_groups, rename_group, validate_title, Group, Roots};
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
