//! Round trips between every harness and spidair, through the real `sesync-conv` binary.
//!
//! spidair is Pi underneath: its files are Pi v3 journals, so a spidair copy is `to-pi` into
//! `<spidair agent>/sessions/<pi_bucket(cwd)>/<ts>_<id>.jsonl`, and a spidair file goes back
//! out with the verb of the target. Every file here is written under a temp sandbox; no real
//! harness store, CLI or database is read. OpenCode's export is a JSON fixture, not a call to
//! its CLI.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync-conv");
const CWD: &str = "/home/u/proj";
const HOME: &str = "/home/u";
/// spidair's session id: a UUIDv7, as spidair writes it.
const SPIDAIR_ID: &str = "01a20546-5d97-73e2-a862-187a4d9eb592";
const CLAUDE_ID: &str = "3f0c9a1e-6b2d-4c7a-9e55-1a2b3c4d5e6f";
/// Pi's project folder for `/home/u/proj` (`journal::pi_bucket`).
const PI_BUCKET: &str = "--home-u-proj--";
/// Claude's project folder for `/home/u/proj` (`claude::bucket`).
const CLAUDE_BUCKET: &str = "-home-u-proj";
/// Marks the spidair system prompt, so a test can see whether it leaked into another target.
const PREAMBLE_MARK: &str = "SPIDAIR-SYSTEM-PREAMBLE";

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("spidair-rt-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn conv(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("run sesync-conv")
}

/// The printed output path of a successful verb.
fn printed(o: &Output) -> PathBuf {
    assert!(o.status.success(), "sesync-conv failed: {}", String::from_utf8_lossy(&o.stderr));
    PathBuf::from(String::from_utf8_lossy(&o.stdout).trim())
}

fn write(path: &Path, text: &str) -> PathBuf {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    path.to_path_buf()
}

fn records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .collect()
}

/// `<ts>_<uuid>.jsonl`: the name omp, Pi and spidair list a session by.
fn assert_session_name(path: &Path, id: &str) {
    let name = path.file_name().unwrap().to_str().unwrap();
    let ts = &name[..24];
    assert!(
        ts.len() == 24 && ts.as_bytes()[10] == b'T' && ts.ends_with('Z') && ts.as_bytes()[13] == b'-',
        "timestamp part of {name} has the omp/Pi shape"
    );
    assert_eq!(&name[24..25], "_", "{name}");
    assert_eq!(&name[25..], format!("{id}.jsonl"), "{name} ends with the session id");
}

fn blocks_of(m: &Value) -> Vec<String> {
    match m.get("content") {
        Some(Value::String(_)) => vec!["text".into()],
        Some(Value::Array(a)) => a.iter().map(|b| b["type"].as_str().unwrap_or("?").to_string()).collect(),
        _ => vec![],
    }
}

// ----- fixtures -----------------------------------------------------------------------------

/// A spidair session in Pi's v3 shape: model and thinking records, a large system message with
/// sections, a user image, an assistant with thinking (spidair's `reasoning_content`), a read that
/// returns an image, a failing bash, custom records from extensions, and a final answer.
fn spidair_journal() -> String {
    let pre = format!("{PREAMBLE_MARK} rules and tool docs. ").repeat(400);
    let t = |n: u32| format!("2026-11-22T10:00:{:02}.{:03}Z", n / 1000, n % 1000);
    let ms = |n: i64| 1_795_000_000_000 + n;
    [
        json!({"type":"session","version":3,"id":SPIDAIR_ID,"timestamp":"2026-11-22T10:00:00.000Z","cwd":CWD}),
        json!({"type":"model_change","id":"mc1","parentId":null,"timestamp":t(100),"provider":"llama-swap","modelId":"qwen3-coder"}),
        json!({"type":"thinking_level_change","id":"tl1","parentId":"mc1","timestamp":t(200),"thinkingLevel":"medium"}),
        json!({"type":"message","id":"sys1","parentId":"tl1","timestamp":t(300),"message":{
            "role":"system","content":pre,
            "sections":{"preamble":pre,"tools":"read, bash, edit","rules":"be brief"},"timestamp":ms(300)}}),
        json!({"type":"message","id":"u1","parentId":"sys1","timestamp":t(400),"message":{"role":"user","timestamp":ms(400),
            "content":[{"type":"text","text":"look at this"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}]}}),
        json!({"type":"message","id":"a1","parentId":"u1","timestamp":t(500),"message":{"role":"assistant","timestamp":ms(500),
            "api":"openai-completions","provider":"llama-swap","model":"qwen3-coder","stopReason":"toolUse",
            "usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"total":0,"input":0,"output":0,"cacheRead":0,"cacheWrite":0}},
            "content":[{"type":"thinking","thinking":"Need to read the screenshot first.","thinkingSignature":"reasoning_content"},
                       {"type":"text","text":"Reading the screenshot."},
                       {"type":"toolCall","id":"call_read_1","name":"read","arguments":{"path":"/home/u/proj/shot.png"}}]}}),
        json!({"type":"message","id":"t1","parentId":"a1","timestamp":t(600),"message":{"role":"toolResult","timestamp":ms(600),
            "toolCallId":"call_read_1","toolName":"read","isError":false,
            "content":[{"type":"text","text":"Read image file"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}]}}),
        json!({"type":"message","id":"a2","parentId":"t1","timestamp":t(700),"message":{"role":"assistant","timestamp":ms(700),
            "api":"openai-completions","provider":"llama-swap","model":"qwen3-coder","stopReason":"toolUse",
            "usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"total":0,"input":0,"output":0,"cacheRead":0,"cacheWrite":0}},
            "content":[{"type":"toolCall","id":"call_bash_1","name":"bash","arguments":{"command":"false"}}]}}),
        json!({"type":"message","id":"t2","parentId":"a2","timestamp":t(800),"message":{"role":"toolResult","timestamp":ms(800),
            "toolCallId":"call_bash_1","toolName":"bash","isError":true,"content":[{"type":"text","text":"exit 1"}]}}),
        json!({"type":"message","id":"a3","parentId":"t2","timestamp":t(900),"message":{"role":"assistant","timestamp":ms(900),
            "api":"openai-completions","provider":"llama-swap","model":"qwen3-coder","stopReason":"stop",
            "usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"total":0,"input":0,"output":0,"cacheRead":0,"cacheWrite":0}},
            "content":[{"type":"text","text":"The screenshot shows a dialog."}]}}),
        json!({"type":"custom","customType":"node-guard-load","id":"c1","parentId":"a3","timestamp":t(950),"data":{"load1":0.4}}),
        json!({"type":"custom","customType":"swarm-settings","id":"c2","parentId":"c1","timestamp":t(960),"data":{"swarm":true}}),
        json!({"type":"custom","customType":"mascot","id":"c3","parentId":"c2","timestamp":t(970),"data":{"frame":3}}),
    ]
    .iter()
    .map(|v| serde_json::to_string(v).unwrap())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n"
}

/// Claude Code transcript: a rename, a user turn with a pasted image, a thinking block, a Read
/// call answered in text, and a closing answer.
fn claude_transcript() -> String {
    [
        json!({"type":"custom-title","customTitle":"Shot debugging","sessionId":CLAUDE_ID}),
        json!({"type":"user","uuid":"c-u1","parentUuid":null,"sessionId":CLAUDE_ID,"cwd":CWD,"timestamp":"2026-11-22T09:00:00.000Z",
            "message":{"role":"user","content":[{"type":"text","text":"look at this"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]}}),
        json!({"type":"assistant","uuid":"c-a1","parentUuid":"c-u1","sessionId":CLAUDE_ID,"timestamp":"2026-11-22T09:00:01.000Z",
            "message":{"role":"assistant","model":"claude-sonnet-5-5","content":[
                {"type":"thinking","thinking":"Read the file first.","signature":"SIG=="},
                {"type":"text","text":"Reading it."},
                {"type":"tool_use","id":"toolu_01READ","name":"Read","input":{"file_path":"/home/u/proj/shot.png"}}]}}),
        json!({"type":"user","uuid":"c-r1","parentUuid":"c-a1","sessionId":CLAUDE_ID,"timestamp":"2026-11-22T09:00:02.000Z",
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01READ","content":"ok"}]}}),
        json!({"type":"assistant","uuid":"c-a2","parentUuid":"c-r1","sessionId":CLAUDE_ID,"timestamp":"2026-11-22T09:00:03.000Z",
            "message":{"role":"assistant","model":"claude-sonnet-5-5","content":[{"type":"text","text":"Done."}]}}),
    ]
    .iter()
    .map(|v| serde_json::to_string(v).unwrap())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n"
}

/// An omp journal: a 256-byte title slot, then a Pi-shaped chain.
fn omp_journal() -> String {
    let base = r#"{"type":"title","v":1,"title":"Omp title","source":"auto","updatedAt":"2026-11-22T10:00:00.000Z","pad":""}"#;
    let slot = base.replace(r#""pad":"""#, &format!(r#""pad":"{}""#, " ".repeat(255 - base.len())));
    assert_eq!(slot.len(), 255);
    let rec = [
        r#"{"type":"session","version":3,"id":"aaaaaaaa-1111-7222-8333-444444444444","timestamp":"2026-11-22T10:00:00.000Z","cwd":"/home/u/proj"}"#,
        r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-11-22T10:00:01.000Z","message":{"role":"user","content":"hello omp"}}"#,
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-22T10:00:02.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-sonnet-5-5","content":[{"type":"thinking","thinking":"Plan it.","thinkingSignature":"omp-sig"},{"type":"text","text":"On it."},{"type":"toolCall","id":"omp_call_1","name":"bash","arguments":{"command":"ls"}}]}}"#,
        r#"{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-11-22T10:00:03.000Z","message":{"role":"toolResult","toolCallId":"omp_call_1","toolName":"bash","isError":false,"content":[{"type":"text","text":"a.txt"}]}}"#,
        r#"{"type":"message","id":"m4","parentId":"m3","timestamp":"2026-11-22T10:00:04.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-sonnet-5-5","content":[{"type":"text","text":"Listed."}]}}"#,
    ];
    format!("{slot}\n{}\n", rec.join("\n"))
}

/// The JSON `opencode export <id>` prints: a user turn, an assistant turn with reasoning and a
/// finished bash tool, and a closing answer.
fn opencode_export() -> String {
    json!({
        "info": {"id": "ses_rt_0001", "directory": CWD, "title": "Ls check", "time": {"created": 1795000000000i64}},
        "messages": [
            {"info": {"id": "msg_u1", "role": "user", "time": {"created": 1795000000100i64}},
             "parts": [{"type": "text", "text": "list the files"}]},
            {"info": {"id": "msg_a1", "role": "assistant", "time": {"created": 1795000000200i64}},
             "parts": [
                {"type": "reasoning", "text": "Use ls."},
                {"type": "text", "text": "Listing."},
                {"type": "tool", "callID": "call_ls_1", "tool": "bash",
                 "state": {"status": "completed", "input": {"command": "ls"}, "output": "a.txt", "time": {"start": 1795000000210i64, "end": 1795000000220i64}}}]},
            {"info": {"id": "msg_a2", "role": "assistant", "time": {"created": 1795000000300i64}},
             "parts": [{"type": "text", "text": "One file: a.txt."}]}
        ]
    })
    .to_string()
}

// ----- (a) Claude Code -> spidair -> Claude ---------------------------------------------------

#[test]
fn claude_to_spidair_and_back_keeps_turns_pairs_and_titles() {
    let sb = sandbox("claude");
    let transcript = write(&sb.join("claude-src.jsonl"), &claude_transcript());
    let journal = printed(&conv(&["from-claude", transcript.to_str().unwrap(), "--out", sb.join("journal.jsonl").to_str().unwrap()]));

    // Claude -> spidair: `to-pi` into the spidair agent's sessions folder.
    let spidair_root = sb.join("spidair").join("sessions");
    let copy = printed(&conv(&["to-pi", journal.to_str().unwrap(), "--out-dir", spidair_root.to_str().unwrap()]));
    assert_eq!(copy.parent().unwrap().file_name().unwrap(), PI_BUCKET, "filed under pi_bucket(cwd)");
    assert_session_name(&copy, CLAUDE_ID);
    let recs = records(&copy);
    assert_eq!(recs[0]["id"], CLAUDE_ID);
    let name = recs.iter().rev().find(|r| r["type"] == "session_info").expect("session_info name record");
    assert_eq!(name["name"], "Shot debugging", "the Claude rename is the name spidair shows");
    let msgs: Vec<&Value> = recs.iter().filter(|r| r["type"] == "message").collect();
    let roles: Vec<&str> = msgs.iter().map(|m| m["message"]["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "toolResult", "assistant"], "turn order preserved");
    let user = &msgs[0]["message"]["content"];
    assert_eq!(user[0]["text"], "look at this");
    assert_eq!(user[1]["type"], "image", "the pasted image survives into spidair");
    assert_eq!(user[1]["data"], "iVBORw0KGgo=");
    let a1 = &msgs[1]["message"]["content"];
    let marked = a1[0]["text"].as_str().unwrap();
    assert!(marked.starts_with("<prior_reasoning") && marked.contains("Read the file first."), "thinking kept as marked text");
    assert!(a1[0].get("thinkingSignature").is_none(), "Claude's signature is never copied");
    let call = a1.as_array().unwrap().iter().find(|b| b["type"] == "toolCall").unwrap();
    assert_eq!(msgs[2]["message"]["toolCallId"], call["id"], "result paired with its call id");
    assert_eq!(msgs[2]["message"]["toolName"], "Read");

    // spidair -> Claude: `to-claude` from the spidair copy.
    let claude_root = sb.join("claude").join("projects");
    let back = printed(&conv(&["to-claude", copy.to_str().unwrap(), "--out-dir", claude_root.to_str().unwrap()]));
    assert_eq!(back.parent().unwrap().file_name().unwrap(), CLAUDE_BUCKET);
    let out = records(&back);
    assert_eq!(out[0]["type"], "custom-title");
    assert_eq!(out[0]["customTitle"], "Shot debugging");
    let kinds: Vec<&str> = out[1..].iter().map(|r| r["message"]["role"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["user", "assistant", "user", "assistant"]);
    assert_eq!(blocks_of(&out[1]["message"]), ["text", "image"]);
    let tool_use = out[2]["message"]["content"].as_array().unwrap().iter().find(|b| b["type"] == "tool_use").unwrap();
    let tool_result = &out[3]["message"]["content"][0];
    assert_eq!(tool_result["tool_use_id"], tool_use["id"], "tool_use and tool_result share the id Claude needs");
    assert!(tool_result["content"].as_str().unwrap().contains("ok"), "the result text is kept");
}

// ----- (b) omp -> spidair -> omp --------------------------------------------------------------

#[test]
fn omp_to_spidair_and_back_keeps_title_turns_and_pairs() {
    let sb = sandbox("omp");
    let omp = write(&sb.join("omp").join("sessions").join("-proj").join("2026-11-22T10-00-00-000Z_aaaaaaaa-1111-7222-8333-444444444444.jsonl"), &omp_journal());
    let spidair_root = sb.join("spidair").join("sessions");
    let copy = printed(&conv(&["to-pi", omp.to_str().unwrap(), "--out-dir", spidair_root.to_str().unwrap()]));
    assert_eq!(copy.parent().unwrap().file_name().unwrap(), PI_BUCKET);
    assert_session_name(&copy, "aaaaaaaa-1111-7222-8333-444444444444");
    let recs = records(&copy);
    assert_eq!(recs[0]["type"], "session", "the title slot is dropped: Pi starts at the header");
    assert_eq!(recs.iter().rev().find(|r| r["type"] == "session_info").unwrap()["name"], "Omp title");

    let omp_root = sb.join("omp2").join("sessions");
    let back = printed(&conv(&["to-omp", copy.to_str().unwrap(), "--out-dir", omp_root.to_str().unwrap(), "--home", HOME]));
    assert_eq!(back.parent().unwrap().file_name().unwrap(), "-proj", "omp files a home project under -<relative>");
    assert_session_name(&back, "aaaaaaaa-1111-7222-8333-444444444444");
    let text = fs::read_to_string(&back).unwrap();
    let slot: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(slot["type"], "title");
    assert_eq!(slot["title"], "Omp title");
    let msgs: Vec<Value> = records(&back).into_iter().filter(|r| r["type"] == "message").collect();
    let roles: Vec<&str> = msgs.iter().map(|m| m["message"]["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "toolResult", "assistant"]);
    assert_eq!(msgs[1]["message"]["content"][0]["thinkingSignature"], "omp-sig", "omp to omp keeps its own signature");
    assert_eq!(msgs[2]["message"]["toolCallId"], "omp_call_1", "result paired with its call id");
}

// ----- (c) OpenCode -> spidair -> OpenCode ----------------------------------------------------

#[test]
fn opencode_to_spidair_and_back_keeps_turns_tools_and_title() {
    let sb = sandbox("opencode");
    let export = write(&sb.join("export.json"), &opencode_export());
    let journal = printed(&conv(&["from-opencode", export.to_str().unwrap(), "--out", sb.join("journal.jsonl").to_str().unwrap()]));
    let spidair_root = sb.join("spidair").join("sessions");
    let copy = printed(&conv(&["to-pi", journal.to_str().unwrap(), "--out-dir", spidair_root.to_str().unwrap()]));
    assert_eq!(copy.parent().unwrap().file_name().unwrap(), PI_BUCKET);
    assert_session_name(&copy, "ses_rt_0001");
    let recs = records(&copy);
    assert_eq!(recs.iter().rev().find(|r| r["type"] == "session_info").unwrap()["name"], "Ls check");
    let roles: Vec<String> = recs.iter().filter(|r| r["type"] == "message").map(|r| r["message"]["role"].as_str().unwrap().to_string()).collect();
    assert_eq!(roles, ["user", "assistant", "toolResult", "assistant"]);

    let bundle_path = sb.join("bundle.json");
    let bundle = printed(&conv(&[
        "to-opencode", copy.to_str().unwrap(), "--out", bundle_path.to_str().unwrap(), "--id", "ses_back_0001", "--title", "Ls check",
    ]));
    let b: Value = serde_json::from_str(&fs::read_to_string(bundle).unwrap()).unwrap();
    let msgs = b["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["info"]["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "assistant"], "the tool result is folded into its call, not a message");
    let tool = msgs[1]["parts"].as_array().unwrap().iter().find(|p| p["type"] == "tool").unwrap();
    assert_eq!(tool["callID"], "call_ls_1");
    assert_eq!(tool["state"]["status"], "completed");
    assert_eq!(tool["state"]["output"], "a.txt", "the result is paired with its call");
}

// ----- (d) Pi <-> spidair ---------------------------------------------------------------------

#[test]
fn pi_and_spidair_round_trip_is_record_for_record() {
    let sb = sandbox("pi");
    let pi_src = write(&sb.join("pi").join("sessions").join(PI_BUCKET).join(format!("2026-11-22T10-00-00-000Z_{SPIDAIR_ID}.jsonl")), &spidair_journal());
    let spidair_root = sb.join("spidair").join("sessions");
    let copy = printed(&conv(&["to-pi", pi_src.to_str().unwrap(), "--out-dir", spidair_root.to_str().unwrap()]));
    let pi_back_root = sb.join("pi2").join("sessions");
    let back = printed(&conv(&["to-pi", copy.to_str().unwrap(), "--out-dir", pi_back_root.to_str().unwrap()]));
    assert_session_name(&back, SPIDAIR_ID);
    let before = records(&pi_src);
    let after = records(&back);
    // Each copy appends one session_info name; everything before it is the source, unchanged.
    assert_eq!(&after[..before.len()], &before[..], "every source record survives both copies, in order");
    let extra: Vec<&Value> = after[before.len()..].iter().collect();
    assert!(extra.iter().all(|r| r["type"] == "session_info"), "only name records are appended");
}

// ----- (iii) spidair's system and custom records --------------------------------------------

#[test]
fn spidair_system_and_custom_records_reach_pi_untouched_and_drop_cleanly_elsewhere() {
    let sb = sandbox("system");
    let src = write(&sb.join("spidair").join("sessions").join(PI_BUCKET).join(format!("2026-11-22T10-00-00-000Z_{SPIDAIR_ID}.jsonl")), &spidair_journal());
    let to_pi = printed(&conv(&["to-pi", src.to_str().unwrap(), "--out-dir", sb.join("pi").join("sessions").to_str().unwrap()]));
    let all = fs::read_to_string(&to_pi).unwrap();
    assert!(all.contains(PREAMBLE_MARK) && all.contains("node-guard-load") && all.contains("swarm-settings") && all.contains("mascot"),
        "the system prompt and every custom record are carried to Pi");

    // Claude: the system message and custom records are not turns; nothing errors.
    let claude = printed(&conv(&["to-claude", src.to_str().unwrap(), "--out-dir", sb.join("claude").join("projects").to_str().unwrap()]));
    let text = fs::read_to_string(&claude).unwrap();
    assert!(!text.contains(PREAMBLE_MARK) && !text.contains("node-guard-load") && !text.contains("mascot"));

    // OpenCode: the bundle never carries a system role.
    let ob = conv(&["to-opencode", src.to_str().unwrap(), "--id", "ses_sys_0001", "--title", "t"]);
    assert!(ob.status.success(), "to-opencode: {}", String::from_utf8_lossy(&ob.stderr));
    let bundle_text = String::from_utf8_lossy(&ob.stdout).into_owned();
    assert!(!bundle_text.contains(PREAMBLE_MARK) && !bundle_text.contains("node-guard-load"));
    assert!(!bundle_text.contains("\"role\":\"system\""));

    // Antigravity: written to its store and read back; the system prompt is not in the copy.
    let store = sb.join("agy");
    fs::create_dir_all(store.join("conversations")).unwrap();
    let ag = conv(&["to-antigravity", src.to_str().unwrap(), "--store", store.to_str().unwrap(), "--id", "00000000-0000-4000-8000-000000000001", "--title", "t"]);
    assert!(ag.status.success(), "to-antigravity: {}", String::from_utf8_lossy(&ag.stderr));
    let ag_db = store.join("conversations").join("00000000-0000-4000-8000-000000000001.db");
    let read_back = sb.join("agy-back.jsonl");
    let rb = conv(&["from-antigravity", ag_db.to_str().unwrap(), "--out", read_back.to_str().unwrap()]);
    assert!(rb.status.success(), "from-antigravity: {}", String::from_utf8_lossy(&rb.stderr));
    let agt = fs::read_to_string(&read_back).unwrap();
    assert!(!agt.contains(PREAMBLE_MARK), "the system prompt does not reach Antigravity");
}

// ----- (iv) model ids, (v) thinking signatures ----------------------------------------------

#[test]
fn spidair_thinking_is_never_a_claude_signature_and_images_are_placeholders() {
    let sb = sandbox("thinking");
    let src = write(&sb.join("s.jsonl"), &spidair_journal());
    let back = printed(&conv(&["to-claude", src.to_str().unwrap(), "--out-dir", sb.join("claude").to_str().unwrap()]));
    let text = fs::read_to_string(&back).unwrap();
    assert!(!text.contains("reasoning_content") && !text.contains("thinkingSignature") && !text.contains("\"signature\""),
        "a spidair thinking signature is never written to Claude");
    assert!(!text.contains("\"type\":\"thinking\""), "Claude never receives a thinking block");
    assert!(text.contains("<prior_reasoning harness=\\\"omp\\\" model=\\\"qwen3-coder\\\">"),
        "earlier reasoning is kept as marked text");
    let recs = records(&back);
    let assistant_models: Vec<&str> = recs.iter().filter(|r| r["type"] == "assistant").map(|r| r["message"]["model"].as_str().unwrap()).collect();
    assert!(!assistant_models.is_empty() && assistant_models.iter().all(|m| *m == "qwen3-coder"),
        "the Claude copy names the model the spidair turn was answered by");
}

#[test]
fn a_claude_session_opened_in_spidair_has_no_provider_on_its_assistant_turns() {
    // Claude's transcript names only a model id. The journal and the spidair copy inherit that:
    // Pi restores its model from a model_change or from an assistant's provider and model.
    let sb = sandbox("claude-model");
    let transcript = write(&sb.join("claude-src.jsonl"), &claude_transcript());
    let journal = printed(&conv(&["from-claude", transcript.to_str().unwrap(), "--out", sb.join("journal.jsonl").to_str().unwrap()]));
    let copy = printed(&conv(&["to-pi", journal.to_str().unwrap(), "--out-dir", sb.join("spidair").to_str().unwrap()]));
    let recs = records(&copy);
    let a = recs.iter().find(|r| r["type"] == "message" && r["message"]["role"] == "assistant").unwrap();
    assert_eq!(a["message"]["model"], "claude-sonnet-5-5");
    assert!(a["message"].get("provider").is_none(), "no provider is known for a Claude transcript");
}

// ----- (ii) branch switch: which branch is exported ----------------------------------------

/// A Pi session where the user went back to `a1` and asked for another answer.
/// `new_turn` decides whether a turn was typed on the new branch.
fn branched(new_turn: bool) -> String {
    let mut lines = vec![
        r#"{"type":"session","version":3,"id":"bbbbbbbb-1111-7222-8333-444444444444","timestamp":"2026-11-22T11:00:00.000Z","cwd":"/home/u/proj"}"#.to_string(),
        r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-11-22T11:00:01.000Z","message":{"role":"user","content":"first"}}"#.to_string(),
        r#"{"type":"message","id":"a1","parentId":"m1","timestamp":"2026-11-22T11:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"answer one"}]}}"#.to_string(),
        r#"{"type":"message","id":"m2","parentId":"a1","timestamp":"2026-11-22T11:00:03.000Z","message":{"role":"user","content":"abandoned question"}}"#.to_string(),
        r#"{"type":"message","id":"a2","parentId":"m2","timestamp":"2026-11-22T11:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"abandoned answer"}]}}"#.to_string(),
    ];
    // The switch: the next entry hangs off a1, not off the abandoned a2.
    lines.push(r#"{"type":"model_change","id":"mc9","parentId":"a1","timestamp":"2026-11-22T11:00:05.000Z","provider":"llama-swap","modelId":"qwen3-coder"}"#.to_string());
    if new_turn {
        lines.push(r#"{"type":"message","id":"m3","parentId":"mc9","timestamp":"2026-11-22T11:00:06.000Z","message":{"role":"user","content":"second question"}}"#.to_string());
        lines.push(r#"{"type":"message","id":"a3","parentId":"m3","timestamp":"2026-11-22T11:00:07.000Z","message":{"role":"assistant","content":[{"type":"text","text":"second answer"}]}}"#.to_string());
    }
    lines.join("\n") + "\n"
}

fn claude_texts(path: &Path) -> Vec<String> {
    records(path)
        .iter()
        .filter(|r| r["type"] == "user" || r["type"] == "assistant")
        .flat_map(|r| {
            let c = &r["message"]["content"];
            match c {
                Value::String(s) => vec![s.clone()],
                Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str().map(str::to_string)).collect(),
                _ => vec![],
            }
        })
        .collect()
}

#[test]
fn a_branch_switch_with_no_new_turn_exports_the_branch_pi_resumes() {
    // Pi resumes from the last entry in the file (SessionManager._buildIndex): the model_change
    // hanging off a1. The abandoned answer a2 is not part of the conversation.
    let sb = sandbox("branch-idle");
    let src = write(&sb.join("b.jsonl"), &branched(false));
    let out = printed(&conv(&["to-claude", src.to_str().unwrap(), "--out-dir", sb.join("claude").to_str().unwrap()]));
    assert_eq!(claude_texts(&out), ["first", "answer one"], "the abandoned branch is not exported");
    let copy = printed(&conv(&["to-pi", src.to_str().unwrap(), "--out-dir", sb.join("pi").to_str().unwrap()]));
    assert_eq!(records(&copy).iter().filter(|r| r["type"] == "message").count(), 4, "a Pi copy keeps the whole file, abandoned answer included");
}

#[test]
fn a_branch_switch_followed_by_a_turn_exports_the_new_branch() {
    let sb = sandbox("branch-turn");
    let src = write(&sb.join("b.jsonl"), &branched(true));
    let out = printed(&conv(&["to-claude", src.to_str().unwrap(), "--out-dir", sb.join("claude").to_str().unwrap()]));
    assert_eq!(claude_texts(&out), ["first", "answer one", "second question", "second answer"]);
}

/// omp's `model_usage` record, written after the leaf moved past its `parentId` (a subagent usage
/// callback with a stale parent). omp resumes at `activeLeafId`, so the whole chain is live.
fn omp_model_usage_journal() -> String {
    [
        r#"{"type":"session","version":3,"id":"cccccccc-1111-7222-8333-444444444444","timestamp":"2026-11-22T12:00:00.000Z","cwd":"/home/u/proj"}"#,
        r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-11-22T12:00:01.000Z","message":{"role":"user","content":"q1"}}"#,
        r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-22T12:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"a1"}]}}"#,
        r#"{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-11-22T12:00:03.000Z","message":{"role":"user","content":"q2"}}"#,
        r#"{"type":"message","id":"m4","parentId":"m3","timestamp":"2026-11-22T12:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"a2"}]}}"#,
        r#"{"type":"model_usage","id":"mu","parentId":"m2","activeLeafId":"m4","timestamp":"2026-11-22T12:00:05.000Z","usage":{"input":1}}"#,
    ]
    .join("\n")
        + "\n"
}

#[test]
fn an_omp_model_usage_record_does_not_move_the_leaf_off_its_active_branch() {
    let sb = sandbox("model-usage");
    let src = write(&sb.join("u.jsonl"), &omp_model_usage_journal());
    let out = printed(&conv(&["to-claude", src.to_str().unwrap(), "--out-dir", sb.join("claude").to_str().unwrap()]));
    assert_eq!(claude_texts(&out), ["q1", "a1", "q2", "a2"], "the active leaf is activeLeafId, m4");
}

#[test]
fn to_pi_chains_its_session_info_to_the_active_leaf() {
    // Pi's appendSessionInfo: the new record's parent is the leaf, and it becomes the last record,
    // which is the leaf Pi resumes from (SessionManager._buildIndex).
    let sb = sandbox("to-pi-leaf");
    for (name, text, leaf) in [
        ("branch", branched(false), "mc9"),
        ("model-usage", omp_model_usage_journal(), "m4"),
    ] {
        let src = write(&sb.join(format!("{name}.jsonl")), &text);
        let copy = printed(&conv(&["to-pi", src.to_str().unwrap(), "--out-dir", sb.join(name).to_str().unwrap()]));
        let recs = records(&copy);
        let last = recs.last().unwrap();
        assert_eq!(last["type"], "session_info", "{name}");
        assert_eq!(last["parentId"], leaf, "{name}: the name hangs off the leaf the engine resumes from");
        let id = last["id"].as_str().expect("the name record has an id");
        assert_eq!(recs.iter().filter(|r| r["id"] == id).count(), 1, "{name}: the id is new");
    }
}

#[test]
fn an_image_in_a_tool_result_is_a_note_line_in_the_claude_copy() {
    // The note is the attachment note for an image the copy does not carry. sesync's turn hashing
    // drops that line (`turns::without_image_placeholders`), so the copy hashes like the original.
    let sb = sandbox("placeholder-form");
    let src = write(&sb.join("s.jsonl"), &spidair_journal());
    let out = printed(&conv(&["to-claude", src.to_str().unwrap(), "--out-dir", sb.join("claude").to_str().unwrap()]));
    let text = fs::read_to_string(&out).unwrap();
    let result = text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|r| r["message"]["content"].to_string().contains("Read image file"))
        .expect("the Read result");
    let body = result["message"]["content"][0]["content"].as_str().unwrap();
    assert!(body.starts_with("Read image file\n[attachment not carried: image (image/png, 8 B) \u{2014} an image in a tool result, which this copy does not carry; kept in the source session]"), "{body}");
}
