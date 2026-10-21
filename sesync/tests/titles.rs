//! Title rules, ported from daemon/tests/test_titles.py. These are the spec.
//! A mirror name must never carry a date that is not the user's.

use sesync::harness::claude::claude_title_from_lines;
use sesync::harness::journal::omp_title_from_lines;
use sesync::harness::opencode::store::clean_title;

#[test]
fn default_timestamp_title_is_untitled() {
    assert_eq!(clean_title("New session - 2026-10-14T16:52:39.241Z"), "");
}

#[test]
fn default_title_with_other_precision_is_untitled() {
    assert_eq!(clean_title("New session - 2026-09-21T16:29:28.123Z"), "");
}

#[test]
fn real_title_is_kept() {
    assert_eq!(clean_title("Research harness stores"), "Research harness stores");
}

#[test]
fn title_that_mentions_a_date_is_kept() {
    // Only the exact O-penCode default pattern is stripped.
    assert_eq!(clean_title("Plan for 2026-10-14"), "Plan for 2026-10-14");
}

#[test]
fn missing_title_is_empty() {
    assert_eq!(clean_title(""), "");
    assert_eq!(clean_title("   "), "");
}

#[test]
fn claude_ai_title_beats_first_prompt() {
    let lines = [
        r#"{"type":"user","isMeta":false,"message":{"role":"user","content":"This session is being continued from a previous conversation"}}"#,
        r#"{"type":"ai-title","aiTitle":"Configure build pipeline servers"}"#,
        r#"{"type":"user","isMeta":false,"message":{"role":"user","content":"some first prompt"}}"#,
    ];
    let (title, has_user) = claude_title_from_lines(&lines);
    assert_eq!(title, "Configure build pipeline servers");
    assert!(has_user);
}

#[test]
fn claude_custom_title_beats_ai_title() {
    // /rename and -n write custom-title; Claude prefers it over ai-title.
    let lines = [
        r#"{"type":"ai-title","aiTitle":"Generated"}"#,
        r#"{"type":"custom-title","customTitle":"User chose this"}"#,
        r#"{"type":"user","isMeta":false,"message":{"role":"user","content":"prompt"}}"#,
    ];
    assert_eq!(claude_title_from_lines(&lines).0, "User chose this");
}

#[test]
fn claude_continuation_banner_is_never_the_title() {
    let lines = [
        r#"{"type":"user","isMeta":false,"message":{"role":"user","content":"This session is being continued from a previous conversation"}}"#,
        r#"{"type":"user","isMeta":false,"message":{"role":"user","content":"Real question here"}}"#,
    ];
    assert_eq!(claude_title_from_lines(&lines).0, "Real question here");
}

#[test]
fn omp_title_record_beats_header_title() {
    let lines = [
        r#"{"type":"session","version":3,"id":"x","cwd":"/tmp","title":"Old header"}"#,
        r#"{"type":"title","v":1,"title":"Renamed"}"#,
    ];
    assert_eq!(omp_title_from_lines(&lines).0, "Renamed");
}

#[test]
fn omp_title_change_beats_the_initial_slot() {
    // A rename made inside omp appends `title_change`; omp overlays it over the
    // slot on read, and the slot can lag behind until omp saves the file again.
    let lines = [
        r#"{"type":"session","version":3,"id":"x","cwd":"/tmp","title":"Old header"}"#,
        r#"{"type":"title","v":1,"title":"Converted name"}"#,
        r#"{"type":"message","id":"m1","timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
        r#"{"type":"title_change","id":"t1","previousTitle":"Converted name","title":"Renamed inside omp"}"#,
    ];
    assert_eq!(omp_title_from_lines(&lines).0, "Renamed inside omp");
}

// ---- the title reader must find a rename wherever it sits -----------------------------------
//
// A rename made inside the harness appends only a `title_change`; the 256-byte slot keeps the old
// name until the harness saves again. In a long journal that record can sit far from both ends, so
// reading one window at each end missed it, the caller fell back to the stale slot, and the TUI
// could then write that stale name back over the real one.

use std::io::Write;
use std::path::PathBuf;

fn long_journal(name: &str, slot: &str, before_kb: usize, rename: Option<&str>, after_kb: usize) -> PathBuf {
    let path = std::env::temp_dir().join(format!("sesync-titles-{}-{name}.jsonl", std::process::id()));
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, r#"{{"type":"title","v":1,"title":"{slot}","source":"user","updatedAt":"2026-08-18T13:59:45.153Z"}}"#).unwrap();
    writeln!(f, r#"{{"type":"session","version":3,"id":"s1","timestamp":"2026-07-27T10:00:00.000Z","cwd":"/tmp/p"}}"#).unwrap();
    let mut n = 0usize;
    let mut filler = |f: &mut std::fs::File, kb: usize, n: &mut usize| {
        for _ in 0..(kb * 1024 / 120) {
            *n += 1;
            writeln!(
                f,
                r#"{{"type":"message","id":"m{n}","timestamp":"2026-09-01T10:00:00.000Z","message":{{"role":"user","content":[{{"type":"text","text":"filler line padding out the journal to push records away from both ends of the file"}}]}}}}"#
            )
            .unwrap();
        }
    };
    filler(&mut f, before_kb, &mut n);
    if let Some(t) = rename {
        writeln!(f, r#"{{"type":"title_change","id":"tc1","timestamp":"2026-10-19T11:25:46.576Z","source":"user","title":"{t}"}}"#).unwrap();
    }
    filler(&mut f, after_kb, &mut n);
    path
}

#[test]
fn a_rename_far_from_both_ends_is_found() {
    // ~600 KB before and ~300 KB after: the record is deeper than one window from either end.
    let p = long_journal("far", "Old name", 600, Some("PC Config"), 300);
    let text = sesync::harness::journal::read_title_text(&p).expect("readable");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(sesync::harness::journal::omp_title_from_lines(&lines).0, "PC Config");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn without_a_rename_the_slot_still_names_the_session() {
    let p = long_journal("slot-only", "Old name", 700, None, 0);
    let text = sesync::harness::journal::read_title_text(&p).expect("readable");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(sesync::harness::journal::omp_title_from_lines(&lines).0, "Old name");
    let _ = std::fs::remove_file(&p);
}
