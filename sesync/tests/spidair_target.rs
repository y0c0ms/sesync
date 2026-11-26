//! spidair as a mirror target, through the real daemon and the real sesync-conv binary. Every
//! store is in a temp sandbox: spidair's agent dir, Pi's, Claude's and OpenCode's. The OpenCode
//! CLI is a fake that prints an export; the real database is never opened. spidair is offered by
//! its agent dir alone: there is no spidair command to probe.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(unix)]
#[path = "support/fake_cli.rs"]
mod fake_cli;

const BIN: &str = env!("CARGO_BIN_EXE_sesync");
const PREAMBLE_MARK: &str = "SPIDAIR-SYSTEM-PREAMBLE";
const SPIDAIR_ID: &str = "01a20546-5d97-73e2-a862-187a4d9eb592";

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("spidair-target-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// One pass of the daemon.
fn pass(sb: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("SPIDAIR_AGENT_DIR", sb.join("spidair"))
        .env("SESYNC_CONV", sesync_conv())
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", sb.join("no-opencode-cli"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .output()
        .expect("run sesync")
}

/// Every file under `root`, recursively.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files_under(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// Choose `targets` for the source recorded at `key`, the way the TUI saves a choice.
fn choose(sb: &Path, key: &str, targets: &[&str]) {
    let state_path = sb.join("state").join("state.json");
    let mut st: serde_json::Value = serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    st["sources"][key]["chosen_targets"] = serde_json::json!(targets);
    fs::write(&state_path, st.to_string()).unwrap();
}

/// `<ts>_<uuid>.jsonl` under the spidair sessions folder of the project `folder`.
fn spidair_copies(sb: &Path, folder: &str) -> Vec<PathBuf> {
    files_under(&sb.join("spidair").join("sessions").join(folder))
}

#[test]
fn an_opencode_session_mirrors_into_spidair_in_its_pi_bucket() {
    let sb = sandbox("opencode");
    let db = sb.join("opencode.db");
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(&format!(
            "create table session (id text primary key, directory text, title text,
                time_updated integer, time_archived integer);
             insert into session values ('ses_spidair', '/tmp/proj', 'OC to spidair', {now_ms}, null);"
        ))
        .unwrap();
    }
    let json = r#"{"info":{"id":"ses_spidair","directory":"/tmp/proj","title":"OC to spidair","time":{"created":1,"updated":1}},"messages":[{"info":{"id":"m1","role":"user","sessionID":"ses_spidair","time":{"created":1}},"parts":[{"id":"p1","type":"text","text":"opencode to spidair text","messageID":"m1","sessionID":"ses_spidair"}]}]}"#;
    let fake = sb.join("fake-opencode");
    fake_cli::stage(&fake, &format!("#!/bin/sh\ncat <<'EOF'\n{json}\nEOF\n"));
    let run = |max_age: &str| {
        Command::new(BIN)
            .arg("--once")
            .env("SESYNC_DIR", sb.join("state"))
            .env("OMP_AGENT_DIR", sb.join("omp"))
            .env("PI_AGENT_DIR", sb.join("pi"))
            .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
            .env("SPIDAIR_AGENT_DIR", sb.join("spidair"))
            .env("SESYNC_CONV", sesync_conv())
            .env("OPENCODE_DB", &db)
            .env("OPENCODE_BIN", &fake)
            .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
            .env("SESYNC_QUIET", "0")
            .env("SESYNC_MAX_AGE_DAYS", max_age)
            .output()
            .expect("run sesync")
    };
    assert!(run("60").status.success());
    choose(&sb, "ses_spidair", &["spidair"]);
    let o = run("0");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let copies = spidair_copies(&sb, "--tmp-proj--");
    assert_eq!(copies.len(), 1, "one copy in the spidair folder Pi lists /tmp/proj from: {copies:?}");
    let name = copies[0].file_name().unwrap().to_str().unwrap().to_string();
    // `<ts>_<uuid>.jsonl`: a 24-character timestamp, then the copy's UUID.
    assert_eq!(name.len(), 24 + 1 + 36 + ".jsonl".len(), "{name}");
    assert!(name.ends_with(".jsonl") && name.as_bytes()[24] == b'_', "{name}");
    let body = fs::read_to_string(&copies[0]).unwrap();
    assert!(body.contains("opencode to spidair text"), "the conversation is carried: {body}");
    assert!(body.contains("OC to spidair"), "the title is carried as Pi's session name: {body}");
}

#[test]
fn a_spidair_session_mirrors_into_pi_and_claude_without_its_system_prompt_in_claude() {
    let sb = sandbox("spidair-source");
    let folder = sb.join("spidair").join("sessions").join("--tmp-proj--");
    fs::create_dir_all(&folder).unwrap();
    let src = folder.join(format!("2026-11-22T10-00-00-000Z_{SPIDAIR_ID}.jsonl"));
    let pre = format!("{PREAMBLE_MARK} rules. ").repeat(200);
    let text = format!(
        concat!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-11-22T10:00:00.000Z","cwd":"/tmp/proj"}}"#, "\n",
            r#"{{"type":"model_change","id":"mc1","parentId":null,"timestamp":"2026-11-22T10:00:00.100Z","provider":"llama-swap","modelId":"qwen3-coder"}}"#, "\n",
            r#"{{"type":"message","id":"sys1","parentId":"mc1","timestamp":"2026-11-22T10:00:00.200Z","message":{{"role":"system","content":"{pre}","sections":{{"preamble":"{pre}"}},"timestamp":1795000000200}}}}"#, "\n",
            r#"{{"type":"message","id":"u1","parentId":"sys1","timestamp":"2026-11-22T10:00:01.000Z","message":{{"role":"user","content":"spidair question","timestamp":1795000001000}}}}"#, "\n",
            r#"{{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-11-22T10:00:02.000Z","message":{{"role":"assistant","provider":"llama-swap","model":"qwen3-coder","content":[{{"type":"thinking","thinking":"hm","thinkingSignature":"reasoning_content"}},{{"type":"text","text":"spidair answer"}}],"timestamp":1795000002000}}}}"#, "\n",
            r#"{{"type":"custom","customType":"node-guard-load","id":"c1","parentId":"a1","timestamp":"2026-11-22T10:00:03.000Z","data":{{"load1":0.4}}}}"#, "\n",
        ),
        id = SPIDAIR_ID,
        pre = pre,
    );
    fs::write(&src, text).unwrap();
    let key = src.display().to_string();
    assert!(pass(&sb, &["--once"]).status.success());
    choose(&sb, &key, &["pi", "claude"]);
    let o = pass(&sb, &["--once"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let pi_copies = files_under(&sb.join("pi").join("sessions").join("--tmp-proj--"));
    assert_eq!(pi_copies.len(), 1, "one Pi copy");
    let pi_body = fs::read_to_string(&pi_copies[0]).unwrap();
    assert!(pi_body.contains(PREAMBLE_MARK) && pi_body.contains("node-guard-load"), "Pi gets the system prompt and custom records");

    let claude_copies: Vec<PathBuf> = files_under(&sb.join("claude").join("projects")).into_iter().filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    assert_eq!(claude_copies.len(), 1, "one Claude copy: {claude_copies:?}");
    let claude_body = fs::read_to_string(&claude_copies[0]).unwrap();
    assert!(!claude_body.contains(PREAMBLE_MARK) && !claude_body.contains("node-guard-load"), "Claude gets only the turns");
    assert!(claude_body.contains("spidair answer") && claude_body.contains("spidair question"));
    assert!(!claude_body.contains("reasoning_content"), "a spidair thinking signature never reaches Claude");
}
