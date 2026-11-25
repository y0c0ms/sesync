//! Every route a conversation can take between harnesses, end to end through the real binary:
//! a conversation that starts in harness S (written in S's own format) is read back
//! (`from-S`), written into harness T (`to-T`) and read back again (`from-T`). Every ordered pair
//! of {omp, pi, spidair, opencode, claude, antigravity} is checked, including S == T.
//!
//! Must survive every route: each user prompt and each assistant reply, in order.
//! Tracked per route and printed as a table (known losses are listed, not hidden): tool calls,
//! tool results and thinking. spidair uses Pi's journal format (`to-pi`) in its own folder.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_sesync-conv");
const HARNESSES: &[&str] = &["omp", "pi", "spidair", "opencode", "claude", "antigravity"];

const PROMPTS: &[&str] = &["matrix prompt one: list the files", "matrix prompt two: now summarise them"];
const REPLIES: &[&str] = &["matrix reply one: there are two files", "matrix reply two: both are notes"];
const TOOL: &str = "matrix_tool_marker";
const THINK: &str = "matrix thinking marker";

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-matrix-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// The canonical journal every route starts from: two turns, a tool call with its result, thinking.
fn canonical(dir: &Path) -> PathBuf {
    let cwd = if cfg!(windows) { "C:\\\\matrix\\\\proj" } else { "/matrix/proj" };
    let lines = [
        format!(r#"{{"type":"session","version":3,"id":"11111111-2222-4333-8444-555555555555","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{cwd}"}}"#),
        format!(r#"{{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{}"}}]}}}}"#, PROMPTS[0]),
        format!(r#"{{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"{THINK}"}},{{"type":"toolCall","id":"call_1","name":"bash","arguments":{{"command":"ls {TOOL}"}}}}],"stopReason":"toolUse","provider":"anthropic","model":"claude-haiku-5-5","api":"anthropic-messages","usage":{{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}}}}}}}"#),
        format!(r#"{{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-01-01T00:00:03.000Z","message":{{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{{"type":"text","text":"a.md b.md {TOOL}"}}],"isError":false}}}}"#),
        format!(r#"{{"type":"message","id":"a2","parentId":"t1","timestamp":"2026-01-01T00:00:04.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{}"}}],"stopReason":"stop","provider":"anthropic","model":"claude-haiku-5-5","api":"anthropic-messages","usage":{{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}}}}}}}"#, REPLIES[0]),
        format!(r#"{{"type":"message","id":"u2","parentId":"a2","timestamp":"2026-01-01T00:00:05.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{}"}}]}}}}"#, PROMPTS[1]),
        format!(r#"{{"type":"message","id":"a3","parentId":"u2","timestamp":"2026-01-01T00:00:06.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{}"}}],"stopReason":"stop","provider":"anthropic","model":"claude-haiku-5-5","api":"anthropic-messages","usage":{{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}}}}}}}"#, REPLIES[1]),
    ];
    let p = dir.join("canonical.jsonl");
    fs::write(&p, lines.join("\n") + "\n").unwrap();
    p
}

fn run(args: &[&str]) -> Result<String, String> {
    let o = Command::new(BIN).args(args).output().map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(format!("sesync-conv {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().lines().last().unwrap_or("").to_string())
}

/// Write `journal` into harness `h` (its own format, in `dir`); returns what `from_native` reads.
fn to_native(h: &str, journal: &Path, dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).unwrap();
    let j = journal.to_str().unwrap();
    let d = dir.to_str().unwrap();
    let id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    match h {
        "pi" | "spidair" => run(&["to-pi", j, "--out-dir", d, "--id", id]).map(PathBuf::from),
        "omp" => run(&["to-omp", j, "--out-dir", d, "--home", d, "--id", id]).map(PathBuf::from),
        "opencode" => {
            let out = dir.join("bundle.json");
            run(&["to-opencode", j, "--out", out.to_str().unwrap(), "--id", "ses_matrix000000000000000000", "--title", "matrix"])?;
            Ok(out)
        }
        "claude" => run(&["to-claude", j, "--out-dir", d, "--id", id]).map(PathBuf::from),
        "antigravity" => run(&["to-antigravity", j, "--store", d, "--id", id, "--title", "matrix"]).map(PathBuf::from),
        _ => unreachable!(),
    }
}

/// Read harness `h`'s native copy back into a journal.
fn from_native(h: &str, native: &Path, dir: &Path) -> Result<PathBuf, String> {
    let out = dir.join("back.jsonl");
    let (n, o) = (native.to_str().unwrap(), out.to_str().unwrap());
    match h {
        "pi" | "spidair" | "omp" => Ok(native.to_path_buf()),
        "opencode" => run(&["from-opencode", n, "--out", o]).map(|_| out),
        "claude" => run(&["from-claude", n, "--out", o]).map(|_| out),
        "antigravity" => run(&["from-antigravity", n, "--out", o]).map(|_| out),
        _ => unreachable!(),
    }
}

#[derive(Default, Debug)]
struct Seen {
    texts: Vec<(String, String)>,
    tool_calls: usize,
    tool_results: usize,
    thinking: usize,
}

fn read_journal(p: &Path) -> Result<Seen, String> {
    let raw = fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut s = Seen::default();
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let m = &v["message"];
        let role = m["role"].as_str().unwrap_or("").to_string();
        if role == "toolResult" {
            s.tool_results += 1;
            continue;
        }
        let blocks: Vec<Value> = match &m["content"] {
            Value::String(t) => vec![serde_json::json!({"type":"text","text":t})],
            Value::Array(a) => a.clone(),
            _ => vec![],
        };
        for b in blocks {
            match b["type"].as_str() {
                Some("text") => {
                    let t = b["text"].as_str().unwrap_or("").to_string();
                    // Thinking a target cannot take as its own (signed elsewhere) is carried as a
                    // `<prior_reasoning>` text block (src/prior_reasoning.rs): it counts as thinking kept.
                    if t.starts_with("<prior_reasoning") {
                        if t.contains(THINK) {
                            s.thinking += 1;
                        }
                    } else if t.contains("matrix ") {
                        s.texts.push((role.clone(), t));
                    }
                }
                Some("toolCall") => s.tool_calls += 1,
                Some("thinking") => s.thinking += 1,
                _ => {}
            }
        }
    }
    Ok(s)
}

fn expected_texts() -> Vec<(String, String)> {
    vec![
        ("user".into(), PROMPTS[0].into()),
        ("assistant".into(), REPLIES[0].into()),
        ("user".into(), PROMPTS[1].into()),
        ("assistant".into(), REPLIES[1].into()),
    ]
}

#[test]
fn every_route_between_every_pair_of_harnesses_keeps_the_conversation() {
    let root = scratch("all");
    let canon = canonical(&root);
    let mut failures = Vec::new();
    let mut table = vec![format!("{:<12} {:<12} {:>6} {:>6} {:>6}  {}", "from", "to", "calls", "results", "think", "status")];
    let mut lost_tools = Vec::new();
    for &s in HARNESSES {
        // The conversation as it lives in S.
        let sdir = root.join(format!("src-{s}"));
        let start = match to_native(s, &canon, &sdir).and_then(|n| from_native(s, &n, &sdir)) {
            Ok(p) => p,
            Err(e) => {
                failures.push(format!("{s} (as a source): {e}"));
                continue;
            }
        };
        for &t in HARNESSES {
            let tdir = root.join(format!("{s}-to-{t}"));
            let route = to_native(t, &start, &tdir).and_then(|n| from_native(t, &n, &tdir)).and_then(|p| read_journal(&p));
            match route {
                Ok(seen) => {
                    let ok = seen.texts == expected_texts();
                    table.push(format!(
                        "{s:<12} {t:<12} {:>6} {:>6} {:>6}  {}",
                        seen.tool_calls,
                        seen.tool_results,
                        seen.thinking,
                        if ok { "ok" } else { "LOST TEXT" }
                    ));
                    if !ok {
                        failures.push(format!("{s} -> {t}: expected {:?}, got {:?}", expected_texts(), seen.texts));
                    }
                    if seen.tool_calls == 0 || seen.tool_results == 0 {
                        lost_tools.push(format!("{s} -> {t}"));
                    }
                }
                Err(e) => {
                    table.push(format!("{s:<12} {t:<12} {:>6} {:>6} {:>6}  ERROR", "-", "-", "-"));
                    failures.push(format!("{s} -> {t}: {e}"));
                }
            }
        }
    }
    eprintln!("\n{}\n", table.join("\n"));
    if !lost_tools.is_empty() {
        eprintln!("routes that drop the tool call or its result: {}", lost_tools.join(", "));
    }
    // Tool calls and their results must survive every route too: a resumed session that lost them
    // cannot explain its own earlier answers.
    for r in &lost_tools {
        failures.push(format!("{r}: tool call or result lost"));
    }
    let _ = fs::remove_dir_all(&root);
    assert!(failures.is_empty(), "{} route(s) failed:\n{}", failures.len(), failures.join("\n"));
}
