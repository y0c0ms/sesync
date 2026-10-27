//! Titles of long journals, read through the same window the TUI and the daemon use. The rules
//! skip, before parsing, every line that cannot hold a record they need (most of a journal is
//! assistant turns and tool output); these cases pin that the skip never loses a name: a rename
//! deep in the tail, the header, the first prompt, and Claude tool results, which are "user"
//! records but never a prompt.

use sesync::harness::claude::claude_title_from_lines;
use sesync::harness::journal::{omp_title_from_lines, title};
use std::fs;
use std::path::PathBuf;

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-title-long-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// About `mib` MiB of records no title rule uses: assistant turns with a tool call, and tool
/// results, nested and full of escaped text like real omp and Claude journals.
fn bulk(out: &mut String, mib: usize, claude: bool) {
    let output: String = (0..60).map(|i| format!("drwxr-xr-x  2 user user 4096 Oct 27 10:{i:02} \\\"dir {i}\\\"\\n")).collect();
    let target = out.len() + mib * 1024 * 1024;
    let mut n = 0;
    while out.len() < target {
        n += 1;
        let call = format!(r#"[{{"type":"text","text":"Listing the folder, step {n}.\nThen reading it."}},{{"type":"toolCall","id":"call_{n}","name":"bash","arguments":{{"command":"ls -la \"dir {n}\"","timeout":30}}}}]"#);
        let (assistant, result) = if claude {
            (
                format!(r#"{{"type":"assistant","uuid":"a{n}","parentUuid":"r{n}","message":{{"id":"msg_{n}","role":"assistant","model":"m","content":{call},"stop_reason":"tool_use","usage":{{"input_tokens":1200,"output_tokens":80}}}}}}"#),
                format!(r#"{{"type":"user","uuid":"r{n}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"call_{n}","content":"{output}"}}]}}}}"#),
            )
        } else {
            (
                format!(r#"{{"type":"message","id":"a{n}","parentId":"r{n}","timestamp":"2026-10-27T10:00:00Z","message":{{"role":"assistant","content":{call},"usage":{{"input":1200,"output":80}},"stopReason":"toolUse"}}}}"#),
                format!(r#"{{"type":"message","id":"r{n}","parentId":"a{n}","timestamp":"2026-10-27T10:00:01Z","message":{{"role":"toolResult","toolCallId":"call_{n}","content":[{{"type":"text","text":"{output}"}}]}}}}"#),
            )
        };
        out.push_str(&assistant);
        out.push('\n');
        out.push_str(&result);
        out.push('\n');
    }
}

fn written(sb: &PathBuf, name: &str, text: &str) -> PathBuf {
    let p = sb.join(name);
    fs::write(&p, text).unwrap();
    p
}

#[test]
fn a_long_omp_journal_keeps_its_header_rename_and_prompt_names() {
    let sb = sandbox("omp");
    let header = r#"{"type":"session","version":3,"id":"a","cwd":"/tmp","title":"From the header"}"#;
    let bare_header = r#"{"type":"session","version":3,"id":"b","cwd":"/tmp"}"#;
    let prompt = r#"{"type":"message","message":{"role":"user","content":"first prompt"}}"#;

    // No rename: the walk reads the whole 8 MiB tail window and the header names it.
    let mut text = format!("{header}\n{prompt}\n");
    bulk(&mut text, 12, false);
    assert_eq!(title(&written(&sb, "plain.jsonl", &text), omp_title_from_lines), "From the header");

    // A rename made in omp about 6 MiB before the end.
    let mut text = format!("{header}\n{prompt}\n");
    bulk(&mut text, 6, false);
    text.push_str(r#"{"type":"title_change","title":"Renamed in omp","timestamp":"2026-10-27T10:00:00Z"}"#);
    text.push('\n');
    bulk(&mut text, 6, false);
    assert_eq!(title(&written(&sb, "renamed.jsonl", &text), omp_title_from_lines), "Renamed in omp");

    // No name record at all: the first prompt, not a tool result or an assistant turn. It sits
    // past the head window, inside the tail window.
    let mut text = format!("{bare_header}\n");
    bulk(&mut text, 1, false);
    text.push_str(prompt);
    text.push('\n');
    bulk(&mut text, 6, false);
    assert_eq!(title(&written(&sb, "prompt.jsonl", &text), omp_title_from_lines), "first prompt");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_long_claude_transcript_keeps_its_rename_and_its_real_prompt() {
    let sb = sandbox("claude");
    let prompt = r#"{"type":"user","message":{"role":"user","content":"first prompt in claude"}}"#;

    // Tool results are "user" records too; none of them is the prompt.
    let mut text = format!("{prompt}\n");
    bulk(&mut text, 12, true);
    assert_eq!(title(&written(&sb, "plain.jsonl", &text), claude_title_from_lines), "first prompt in claude");

    let mut text = format!("{prompt}\n");
    bulk(&mut text, 6, true);
    text.push_str(r#"{"type":"custom-title","customTitle":"Renamed in Claude"}"#);
    text.push('\n');
    bulk(&mut text, 6, true);
    assert_eq!(title(&written(&sb, "renamed.jsonl", &text), claude_title_from_lines), "Renamed in Claude");

    // An agent name late in a long transcript still beats the first prompt.
    let mut text = format!("{prompt}\n");
    bulk(&mut text, 6, true);
    text.push_str(r#"{"type":"agent-name","agentName":"reviewer"}"#);
    text.push('\n');
    bulk(&mut text, 1, true);
    assert_eq!(title(&written(&sb, "agent.jsonl", &text), claude_title_from_lines), "reviewer");
    let _ = fs::remove_dir_all(&sb);
}
