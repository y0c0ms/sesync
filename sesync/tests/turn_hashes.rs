//! Turn hashes agree between a local read of a journal and the same conversation read back from its
//! Claude copy. The copy is written and read by the real `sesync-conv` binary, in a temp sandbox.

use sesync::turns::turns_of;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("turn-hash-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn conv(args: &[&str]) -> String {
    let o = Command::new(sesync_conv()).args(args).output().expect("run sesync-conv");
    assert!(o.status.success(), "sesync-conv {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// The turn hashes of the journal at `path`, read locally.
fn local_hashes(path: &Path) -> Vec<String> {
    turns_of(&fs::read(path).unwrap()).unwrap().into_iter().map(|t| t.hash).collect()
}

/// The turn hashes of `journal` after a trip through a Claude copy: `to-claude`, then `from-claude`.
fn claude_copy_hashes(sb: &Path, journal: &Path) -> Vec<String> {
    let copy = conv(&["to-claude", journal.to_str().unwrap(), "--out-dir", sb.join("claude").to_str().unwrap()]);
    let back = sb.join("back.jsonl");
    conv(&["from-claude", &copy, "--out", back.to_str().unwrap()]);
    local_hashes(&back)
}

fn write(path: &Path, lines: &[&str]) -> PathBuf {
    fs::write(path, lines.join("\n") + "\n").unwrap();
    path.to_path_buf()
}

/// A Pi journal: a prompt, a read call answered by `result` (its content JSON), and a final answer.
fn pi_journal(result: &str) -> Vec<String> {
    vec![
        r#"{"type":"session","version":3,"id":"dddddddd-1111-7222-8333-444444444444","timestamp":"2026-11-22T13:00:00.000Z","cwd":"/home/u/proj"}"#.into(),
        r#"{"type":"model_change","id":"mc0","parentId":null,"timestamp":"2026-11-22T13:00:00.100Z","provider":"llama-swap","modelId":"qwen3-coder"}"#.into(),
        r#"{"type":"message","id":"sys1","parentId":"mc0","timestamp":"2026-11-22T13:00:00.200Z","message":{"role":"system","content":"SYSTEM PROMPT","timestamp":1795000000200}}"#.into(),
        r#"{"type":"message","id":"u1","parentId":"sys1","timestamp":"2026-11-22T13:00:01.000Z","message":{"role":"user","content":"look at this","timestamp":1795000001000}}"#.into(),
        r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-11-22T13:00:02.000Z","message":{"role":"assistant","provider":"llama-swap","model":"qwen3-coder","content":[{"type":"thinking","thinking":"hm","thinkingSignature":"reasoning_content"},{"type":"text","text":"Reading it."},{"type":"toolCall","id":"call_1","name":"read","arguments":{"path":"/home/u/proj/shot.png"}}],"timestamp":1795000002000}}"#.into(),
        format!(r#"{{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-11-22T13:00:03.000Z","message":{{"role":"toolResult","toolCallId":"call_1","toolName":"read","isError":false,"content":{result},"timestamp":1795000003000}}}}"#),
        r#"{"type":"message","id":"a2","parentId":"t1","timestamp":"2026-11-22T13:00:04.000Z","message":{"role":"assistant","provider":"llama-swap","model":"qwen3-coder","content":[{"type":"text","text":"Done."}],"timestamp":1795000004000}}"#.into(),
    ]
}

#[test]
fn an_image_in_a_tool_result_hashes_the_same_in_the_claude_copy() {
    let sb = sandbox("image-result");
    let text = r#"[{"type":"text","text":"Read image file"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}]"#;
    let lines = pi_journal(text);
    let src = write(&sb.join("pi.jsonl"), &lines.iter().map(String::as_str).collect::<Vec<_>>());
    let local = local_hashes(&src);
    assert_eq!(local.len(), 1, "one completed turn");
    assert_eq!(claude_copy_hashes(&sb, &src), local, "the placeholder is not part of the result");
}

#[test]
fn an_image_only_tool_result_hashes_the_same_in_the_claude_copy() {
    let sb = sandbox("image-only");
    let text = r#"[{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}]"#;
    let lines = pi_journal(text);
    let src = write(&sb.join("pi.jsonl"), &lines.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(claude_copy_hashes(&sb, &src), local_hashes(&src));
}

#[test]
fn an_omp_journal_with_an_image_result_hashes_the_same_in_the_claude_copy() {
    let sb = sandbox("omp-image");
    let lines = [
        r#"{"type":"title","v":1,"title":"Omp title"}"#,
        r#"{"type":"session","version":3,"id":"eeeeeeee-1111-7222-8333-444444444444","timestamp":"2026-11-22T14:00:00.000Z","cwd":"/home/u/proj"}"#,
        r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-11-22T14:00:01.000Z","message":{"role":"user","content":"look","timestamp":1795000001000}}"#,
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-22T14:00:02.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-sonnet-5-5","content":[{"type":"toolCall","id":"omp_1","name":"read","arguments":{"path":"/home/u/proj/x.png"}}],"timestamp":1795000002000}}"#,
        r#"{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-11-22T14:00:03.000Z","message":{"role":"toolResult","toolCallId":"omp_1","toolName":"read","isError":false,"content":[{"type":"text","text":"image: x.png"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}],"timestamp":1795000003000}}"#,
        r#"{"type":"message","id":"m4","parentId":"m3","timestamp":"2026-11-22T14:00:04.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-sonnet-5-5","content":[{"type":"text","text":"Seen."}],"timestamp":1795000004000}}"#,
    ];
    let src = write(&sb.join("omp.jsonl"), &lines);
    assert_eq!(claude_copy_hashes(&sb, &src), local_hashes(&src));
}

#[test]
fn a_branch_switch_with_no_new_turn_hashes_the_branch_pi_resumes() {
    // The user went back to a1 and the switch was not followed by a turn. Pi resumes from the
    // model_change hanging off a1, so the abandoned answer is not part of the conversation.
    let sb = sandbox("branch-idle");
    let lines = [
        r#"{"type":"session","version":3,"id":"bbbbbbbb-1111-7222-8333-444444444444","timestamp":"2026-11-22T11:00:00.000Z","cwd":"/home/u/proj"}"#,
        r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-11-22T11:00:01.000Z","message":{"role":"user","content":"first"}}"#,
        r#"{"type":"message","id":"a1","parentId":"m1","timestamp":"2026-11-22T11:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"answer one"}]}}"#,
        r#"{"type":"message","id":"m2","parentId":"a1","timestamp":"2026-11-22T11:00:03.000Z","message":{"role":"user","content":"abandoned question"}}"#,
        r#"{"type":"message","id":"a2","parentId":"m2","timestamp":"2026-11-22T11:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"abandoned answer"}]}}"#,
        r#"{"type":"model_change","id":"mc9","parentId":"a1","timestamp":"2026-11-22T11:00:05.000Z","provider":"llama-swap","modelId":"qwen3-coder"}"#,
    ];
    let src = write(&sb.join("b.jsonl"), &lines);
    let local = local_hashes(&src);
    assert_eq!(local.len(), 1, "only the live turn: {local:?}");
    assert_eq!(claude_copy_hashes(&sb, &src), local, "the export and the local read agree");
}

#[test]
fn an_omp_active_leaf_is_read_the_way_omp_resumes_it() {
    // A model_usage record names activeLeafId m4; the live branch is q1 a1 q2 a2: two turns.
    let sb = sandbox("model-usage");
    let lines = [
        r#"{"type":"session","version":3,"id":"cccccccc-1111-7222-8333-444444444444","timestamp":"2026-11-22T12:00:00.000Z","cwd":"/home/u/proj"}"#,
        r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-11-22T12:00:01.000Z","message":{"role":"user","content":"q1"}}"#,
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-22T12:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"a1"}]}}"#,
        r#"{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-11-22T12:00:03.000Z","message":{"role":"user","content":"q2"}}"#,
        r#"{"type":"message","id":"m4","parentId":"m3","timestamp":"2026-11-22T12:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"a2"}]}}"#,
        r#"{"type":"model_usage","id":"mu","parentId":"m2","activeLeafId":"m4","timestamp":"2026-11-22T12:00:05.000Z","usage":{"input":1}}"#,
    ];
    let src = write(&sb.join("u.jsonl"), &lines);
    assert_eq!(local_hashes(&src).len(), 2);
    assert_eq!(claude_copy_hashes(&sb, &src), local_hashes(&src));
}
