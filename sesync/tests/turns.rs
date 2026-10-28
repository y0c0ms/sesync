//! Completed turns: what counts as a turn, what a change is, and that a conversation hashes the
//! same whichever harness it was read through. The cross-harness cases run the real `sesync-conv`
//! (`to-claude`, `from-claude`) on temp files; no harness CLI runs.

use sesync::turns::{compare, fingerprint, turns_of, Relation, Turn};
use std::path::PathBuf;
use std::process::Command;

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-turns-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn converter() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv");
    assert!(p.exists(), "build the converter first: (cd sesync-conv && cargo build --release)");
    p
}

/// An omp journal of two completed turns. The first answers with a tool call and thinking; the
/// second is a plain reply. Carries the metadata records omp writes, which are not turns.
fn omp_journal(first_answer: &str) -> String {
    [
        r#"{"type":"title","v":1,"title":"Files","source":"auto","updatedAt":"2026-10-01T10:00:09.000Z","pad":""}"#,
        r#"{"type":"session","version":3,"id":"sess-1","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/c"}"#,
        r#"{"type":"model_change","id":"m0","parentId":null,"timestamp":"2026-10-01T10:00:00.500Z","model":"claude-opus"}"#,
        r#"{"type":"message","id":"u1","parentId":"m0","timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"list the files"}]}}"#,
        &format!(r#"{{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-10-01T10:00:02.000Z","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"weigh it","thinkingSignature":"OMP-SIG"}},{{"type":"text","text":"{first_answer}"}},{{"type":"toolCall","id":"call_1","name":"bash","arguments":{{"command":"ls","timeout":30}}}}],"usage":{{"input":1200,"output":80}},"stopReason":"toolUse"}}}}"#),
        r#"{"type":"message","id":"r1","parentId":"a1","timestamp":"2026-10-01T10:00:03.000Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{"type":"text","text":"a.txt"},{"type":"text","text":"b.txt"}],"isError":false}}"#,
        r#"{"type":"message","id":"a2","parentId":"r1","timestamp":"2026-10-01T10:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"There are two files."}],"stopReason":"stop"}}"#,
        r#"{"type":"model_change","id":"m1","parentId":"a2","timestamp":"2026-10-01T10:00:04.500Z","model":"claude-opus"}"#,
        r#"{"type":"message","id":"u2","parentId":"m1","timestamp":"2026-10-01T10:00:05.000Z","message":{"role":"user","content":"thanks"}}"#,
        r#"{"type":"message","id":"a3","parentId":"u2","timestamp":"2026-10-01T10:00:06.000Z","message":{"role":"assistant","content":[{"type":"text","text":"You're welcome."}],"stopReason":"stop"}}"#,
    ].join("\n") + "\n"
}

fn turns(text: &str) -> Vec<Turn> {
    turns_of(text.as_bytes()).expect("journal parses")
}

fn with_appended(base: &str, extra: &[&str]) -> String {
    let mut s = base.to_string();
    for e in extra {
        s.push_str(e);
        s.push('\n');
    }
    s
}

#[test]
fn a_completed_turn_is_a_prompt_and_its_final_answer() {
    let t = turns(&omp_journal("Listing."));
    assert_eq!(t.len(), 2);
    assert_eq!(t[0].user, "list the files");
    assert_eq!(t[0].reply, "Listing.\n[tool: bash]\nThere are two files.");
    assert_eq!(t[1].user, "thanks");
    assert_eq!(t[1].reply, "You're welcome.");
    assert_eq!(t[1].at_ms, 1_790_848_806_000, "the turn's last message time, in ms");
}

#[test]
fn metadata_records_do_not_change_the_turns() {
    let base = omp_journal("Listing.");
    let grown = with_appended(&base, &[
        r#"{"type":"session_info","id":"s1","parentId":"a3","name":"Renamed"}"#,
        r#"{"type":"title_change","id":"t1","parentId":"a3","timestamp":"2026-10-01T10:09:00.000Z","name":"Renamed"}"#,
        r#"{"type":"thinking_level_change","id":"l1","parentId":"a3","level":"high"}"#,
        r#"{"type":"mode","id":"mo1","parentId":"a3","mode":"plan"}"#,
    ]);
    assert_eq!(compare(&turns(&base), &turns(&grown)), Relation::Identical);
}

#[test]
fn a_trailing_prompt_without_a_reply_is_not_a_turn() {
    let base = omp_journal("Listing.");
    let asked = with_appended(&base, &[
        r#"{"type":"message","id":"u3","parentId":"a3","timestamp":"2026-10-01T10:10:00.000Z","message":{"role":"user","content":[{"type":"text","text":"and now?"}]}}"#,
    ]);
    assert_eq!(compare(&turns(&base), &turns(&asked)), Relation::Identical);
}

#[test]
fn an_aborted_or_failed_reply_with_no_content_is_not_a_turn() {
    let base = omp_journal("Listing.");
    let aborted = with_appended(&base, &[
        r#"{"type":"message","id":"u3","parentId":"a3","message":{"role":"user","content":[{"type":"text","text":"again"}]}}"#,
        r#"{"type":"message","id":"a4","parentId":"u3","message":{"role":"assistant","content":[],"stopReason":"aborted"}}"#,
        r#"{"type":"message","id":"a5","parentId":"a4","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"rate limited"}}"#,
    ]);
    assert_eq!(compare(&turns(&base), &turns(&aborted)), Relation::Identical);
}

#[test]
fn a_reply_still_running_tools_is_not_yet_a_turn() {
    let base = omp_journal("Listing.");
    let running = with_appended(&base, &[
        r#"{"type":"message","id":"u3","parentId":"a3","message":{"role":"user","content":[{"type":"text","text":"cat a.txt"}]}}"#,
        r#"{"type":"message","id":"a4","parentId":"u3","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_9","name":"bash","arguments":{"command":"cat a.txt"}}],"stopReason":"toolUse"}}"#,
    ]);
    assert_eq!(compare(&turns(&base), &turns(&running)), Relation::Identical);
    let done = with_appended(&running, &[
        r#"{"type":"message","id":"r4","parentId":"a4","message":{"role":"toolResult","toolCallId":"call_9","content":[{"type":"text","text":"hello"}]}}"#,
        r#"{"type":"message","id":"a6","parentId":"r4","message":{"role":"assistant","content":[{"type":"text","text":"It says hello."}],"stopReason":"stop"}}"#,
    ]);
    // `done` holds one more completed turn than `base`, so from the side of `done` it is Ahead.
    assert_eq!(compare(&turns(&done), &turns(&base)), Relation::Ahead(1), "the finished turn is one more turn");
}

#[test]
fn ids_timestamps_usage_and_thinking_do_not_change_a_turn() {
    let base = omp_journal("Listing.");
    let renumbered = base
        .replace("\"id\":\"u1\"", "\"id\":\"zz1\"")
        .replace("\"parentId\":\"u1\"", "\"parentId\":\"zz1\"")
        .replace("2026-10-01T10:00:01.000Z", "2031-01-01T00:00:00.000Z")
        .replace("\"weigh it\"", "\"a different line of thought\"")
        .replace("\"OMP-SIG\"", "\"OTHER-SIG\"")
        .replace("\"input\":1200", "\"input\":9");
    assert_eq!(turns(&base), turns(&renumbered));
}

#[test]
fn a_changed_answer_is_a_different_turn() {
    let a = turns(&omp_journal("Listing."));
    let b = turns(&omp_journal("Listing them now."));
    assert_ne!(a[0].hash, b[0].hash);
    assert_eq!(a[1].hash, b[1].hash, "the other turn is untouched");
    match compare(&a, &b) {
        Relation::Diverged { common, local_new, team_new } => {
            assert_eq!(common, 0);
            assert_eq!(local_new.len(), 2);
            assert_eq!(team_new.len(), 2);
        }
        other => panic!("expected Diverged, got {other:?}"),
    }
}

#[test]
fn ahead_behind_and_identical_are_prefix_relations() {
    let full = turns(&omp_journal("Listing."));
    let one = &full[..1];
    assert_eq!(compare(full.as_slice(), full.as_slice()), Relation::Identical);
    assert_eq!(compare(full.as_slice(), one), Relation::Ahead(1));
    assert_eq!(compare(one, full.as_slice()), Relation::Behind(1));
    assert_eq!(compare(&[], full.as_slice()), Relation::Behind(2));
    assert_eq!(compare(&[], &[]), Relation::Identical);
}

#[test]
fn a_fingerprint_follows_the_turn_list() {
    let full = turns(&omp_journal("Listing."));
    assert_eq!(fingerprint(&full), fingerprint(&turns(&omp_journal("Listing."))));
    assert_ne!(fingerprint(&full), fingerprint(&full[..1]));
}

#[test]
fn a_compaction_does_not_cut_the_turns_the_file_still_holds() {
    // The file keeps the archived turns before the compaction; a copy made earlier still has them, so
    // the live window must not be what gets compared.
    let base = omp_journal("Listing.");
    let compacted = with_appended(&base, &[
        r#"{"type":"compaction","id":"c1","parentId":"a3","timestamp":"2026-10-01T10:20:00.000Z","summary":"two files, thanks","firstKeptEntryId":"u2"}"#,
    ]);
    assert_eq!(compare(&turns(&base), &turns(&compacted)), Relation::Identical);
    assert_eq!(turns(&compacted).len(), 2);
}

#[test]
fn a_line_that_is_not_json_is_an_error_naming_the_line() {
    let err = turns_of(b"{\"type\":\"session\"}\nnot json\n").unwrap_err();
    assert!(err.contains("line 2"), "{err}");
}

/// Run `sesync-conv` and return its stdout path.
fn conv(args: &[&str]) -> String {
    let out = Command::new(converter()).args(args).output().expect("sesync-conv runs");
    assert!(out.status.success(), "sesync-conv {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// omp journal -> Claude transcript -> omp journal, through the real converter. The round trip must
/// keep the turns, including a prompt that starts with `<` (which `from-claude` drops on the way back).
#[test]
fn a_journal_round_trips_through_claude_with_the_same_turns() {
    let sb = sandbox("roundtrip");
    let mut journal = omp_journal("Listing.");
    journal.push_str(concat!(
        r#"{"type":"message","id":"u9","parentId":"a3","message":{"role":"user","content":[{"type":"text","text":"<b>bold</b> is not a wrapper here"}]}}"#, "\n",
        r#"{"type":"message","id":"a9","parentId":"u9","message":{"role":"assistant","content":[{"type":"text","text":"Noted."}],"stopReason":"stop"}}"#, "\n",
    ));
    let src = sb.join("omp.jsonl");
    std::fs::write(&src, &journal).unwrap();
    let claude_dir = sb.join("projects");
    let transcript = conv(&["to-claude", src.to_str().unwrap(), "--out-dir", claude_dir.to_str().unwrap(), "--id", "11111111-2222-3333-4444-555555555555"]);
    let back = sb.join("back.jsonl");
    conv(&["from-claude", &transcript, "--out", back.to_str().unwrap()]);
    let original = turns(&journal);
    let returned = turns(&std::fs::read_to_string(&back).unwrap());
    // A prompt starting with `<` is no turn on either side, so its reply joins the turn before it.
    assert_eq!(original.len(), 2, "two turns: the '<' prompt is dropped and its reply joins the turn before");
    assert_eq!(compare(&original, &returned), Relation::Identical);
}

/// The same conversation written by Claude Code by hand (thinking, a tool call, a result, a reply):
/// converted to a journal, it has the same turns as the omp journal that holds the same messages.
#[test]
fn a_claude_transcript_has_the_same_turns_as_the_omp_journal_it_came_from() {
    let sb = sandbox("claude-native");
    let transcript = concat!(
        r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","cwd":"/tmp/c","timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"user","content":"list the files"}}"#, "\n",
        r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"s","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"assistant","model":"claude-opus","content":[{"type":"thinking","thinking":"weigh it","signature":"CLAUDE-SIG"},{"type":"text","text":"Listing."},{"type":"tool_use","id":"toolu_1","name":"bash","input":{"command":"ls","timeout":30}}],"stop_reason":"tool_use","usage":{"input_tokens":5}}}"#, "\n",
        r#"{"type":"user","uuid":"r1","parentUuid":"a1","sessionId":"s","timestamp":"2026-10-01T10:00:03.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"a.txt"},{"type":"text","text":"b.txt"}]}]}}"#, "\n",
        r#"{"type":"assistant","uuid":"a2","parentUuid":"r1","sessionId":"s","timestamp":"2026-10-01T10:00:04.000Z","message":{"role":"assistant","model":"claude-opus","content":[{"type":"text","text":"There are two files."}],"stop_reason":"end_turn"}}"#, "\n",
        r#"{"type":"user","uuid":"u2","parentUuid":"a2","sessionId":"s","timestamp":"2026-10-01T10:00:05.000Z","message":{"role":"user","content":"thanks"}}"#, "\n",
        r#"{"type":"assistant","uuid":"a3","parentUuid":"u2","sessionId":"s","timestamp":"2026-10-01T10:00:06.000Z","message":{"role":"assistant","model":"claude-opus","content":[{"type":"text","text":"You're welcome."}],"stop_reason":"end_turn"}}"#, "\n",
    );
    let src = sb.join("claude.jsonl");
    std::fs::write(&src, transcript).unwrap();
    let journal = sb.join("from-claude.jsonl");
    conv(&["from-claude", src.to_str().unwrap(), "--out", journal.to_str().unwrap()]);
    let claude_turns = turns(&std::fs::read_to_string(&journal).unwrap());
    assert_eq!(compare(&turns(&omp_journal("Listing.")), &claude_turns), Relation::Identical);
}
