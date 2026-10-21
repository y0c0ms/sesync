//! Title edge cases the main title tests do not reach: malformed O-penCode default
//! names, list-form message content, and the Claude continuation banner. Each rule
//! protects a name the user sees, so a regression here shows a date or a banner as a
//! title.

use sesync::harness::claude::claude_title_from_lines;
use sesync::harness::journal::omp_title_from_lines;
use sesync::harness::opencode::store::clean_title;

#[test]
fn default_name_with_a_bad_timestamp_is_kept_as_a_real_title() {
    // Only the exact "New session - <ISO timestamp>" shape is the default. Anything
    // that is not that shape is a user title and must be kept.
    for name in [
        "New session - 2026-10-14 16:52:39.241Z", // space, not T
        "New session - 2026-10-14T16:52:39.241",   // no Z
        "New session - 2026/10/14T16:52:39Z",      // slashes
        "New session - 2026-10-14T16-52-39Z",      // dashes in time
        "New session - 2026-10-14T16:52:39.Z",     // empty fraction
        "New session - 2026-10-14T16:52:39.24xZ",  // non-digit fraction
        "New session -",                           // truncated
    ] {
        assert_eq!(clean_title(name), name, "{name:?} must be kept");
    }
}

#[test]
fn default_name_with_no_fraction_is_untitled() {
    assert_eq!(clean_title("New session - 2026-10-14T16:52:39Z"), "");
}

#[test]
fn omp_user_message_with_list_content_is_the_fallback_title() {
    // A user message whose content is a list of blocks: the first text block is the name.
    let lines = [
        r#"{"type":"message","message":{"role":"user","content":[{"type":"image","source":"x"},{"type":"text","text":"from a list block"}]}}"#,
    ];
    assert_eq!(omp_title_from_lines(&lines).0, "from a list block");
}

#[test]
fn omp_user_message_with_no_text_block_gives_no_title() {
    let lines = [
        r#"{"type":"message","message":{"role":"user","content":[{"type":"image","source":"x"}]}}"#,
    ];
    assert_eq!(omp_title_from_lines(&lines).0, "");
}

#[test]
fn omp_title_is_cut_at_the_first_line_and_80_characters() {
    let long = "a".repeat(120);
    let lines = [format!(r#"{{"type":"message","message":{{"role":"user","content":"first line\nsecond {long}"}}}}"#)];
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    assert_eq!(omp_title_from_lines(&refs).0, "first line");
    let one = [format!(r#"{{"type":"message","message":{{"role":"user","content":"{long}"}}}}"#)];
    let refs: Vec<&str> = one.iter().map(String::as_str).collect();
    assert_eq!(omp_title_from_lines(&refs).0.chars().count(), 80);
}

#[test]
fn claude_continuation_banner_is_not_a_title_even_when_it_is_the_only_prompt() {
    let lines = [
        r#"{"type":"user","isMeta":false,"message":{"role":"user","content":"This session is being continued from a previous conversation"}}"#,
    ];
    let (title, has_user) = claude_title_from_lines(&lines);
    assert_eq!(title, "", "a banner must never become the title");
    // has_user follows the Python rule: any non-empty user text not starting with '<'
    // counts as a turn, banner included. That keeps which sessions sync unchanged.
    assert!(has_user);
}

#[test]
fn claude_meta_and_wrapper_text_is_not_a_title() {
    let lines = [
        r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"meta text"}}"#,
        r#"{"type":"user","isMeta":false,"message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#,
    ];
    let (title, has_user) = claude_title_from_lines(&lines);
    assert_eq!(title, "");
    assert!(!has_user);
}

#[test]
fn claude_list_form_user_content_uses_its_text_block() {
    let lines = [
        r#"{"type":"user","isMeta":false,"message":{"role":"user","content":[{"type":"text","text":"list prompt"}]}}"#,
    ];
    let (title, has_user) = claude_title_from_lines(&lines);
    assert_eq!(title, "list prompt");
    assert!(has_user);
}
