//! Title rules, ported from daemon/tests/test_titles.py. These are the spec.
//! A mirror name must never carry a date that is not the user's.

use sesync::titles::{clean_opencode_title, claude_title_from_lines, gemini_title_from_lines, omp_title_from_lines};

#[test]
fn default_timestamp_title_is_untitled() {
    assert_eq!(clean_opencode_title(Some("New session - 2026-10-14T16:52:39.241Z")), "");
}

#[test]
fn default_title_with_other_precision_is_untitled() {
    assert_eq!(clean_opencode_title(Some("New session - 2026-09-21T16:29:28.123Z")), "");
}

#[test]
fn real_title_is_kept() {
    assert_eq!(clean_opencode_title(Some("Research harness stores")), "Research harness stores");
}

#[test]
fn title_that_mentions_a_date_is_kept() {
    // Only the exact O-penCode default pattern is stripped.
    assert_eq!(clean_opencode_title(Some("Plan for 2026-10-14")), "Plan for 2026-10-14");
}

#[test]
fn missing_title_is_empty() {
    assert_eq!(clean_opencode_title(None), "");
    assert_eq!(clean_opencode_title(Some("   ")), "");
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

#[test]
fn gemini_summary_beats_first_prompt() {
    let lines = [
        r#"{"id":"u1","timestamp":"2026-10-01T10:00:00.000Z","type":"user","content":[{"text":"first prompt"}]}"#,
        r#"{"$set":{"summary":"Chosen later"}}"#,
    ];
    let (title, has_user) = gemini_title_from_lines(&lines);
    assert_eq!(title, "Chosen later");
    assert!(has_user);
}

#[test]
fn gemini_first_prompt_names_the_session_until_a_summary_exists() {
    let lines = [r#"{"id":"u1","timestamp":"2026-10-01T10:00:00.000Z","type":"user","content":[{"text":"Plan the move\nsecond line"}]}"#];
    let (title, has_user) = gemini_title_from_lines(&lines);
    assert_eq!(title, "Plan the move");
    assert!(has_user);
}

#[test]
fn gemini_tool_results_are_not_a_user_turn() {
    // A functionResponse-only record is a "user" record in the log but carries no text.
    let lines = [r#"{"id":"u1","timestamp":"2026-10-01T10:00:00.000Z","type":"user","content":[{"functionResponse":{"id":"c1","name":"shell","response":{"output":"ok"}}}]}"#];
    let (title, has_user) = gemini_title_from_lines(&lines);
    assert_eq!(title, "");
    assert!(!has_user);
}
