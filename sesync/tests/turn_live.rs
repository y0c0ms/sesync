//! Turn-taking end to end: an omp session mirrored into claude; the user answers inside the
//! claude copy; the next pass promotes that copy to source and rewrites the omp side. Uses
//! the real sesync-conv and the real daemon binary against sandboxed stores; no harness CLI
//! is started and no real store is touched.
use sesync::state::{Source, State};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-turn-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn run(sb: &Path) -> Output {
    Command::new(BIN)
        .arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", sb.join("xdg").join("data").join("opencode").join("opencode.db"))
        .env("OPENCODE_BIN", "opencode")
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .env("SESYNC_TURN_DEBUG", "1")
        .env("XDG_DATA_HOME", sb.join("xdg").join("data"))
        .env("XDG_CONFIG_HOME", sb.join("xdg").join("cfg"))
        .env("XDG_STATE_HOME", sb.join("xdg").join("state"))
        .env("XDG_CACHE_HOME", sb.join("xdg").join("cache"))
        .output()
        .expect("run sesync")
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { out.extend(walk(&p)); } else if p.extension().map_or(false, |x| x == "jsonl") { out.push(p); }
        }
    }
    out
}

#[test]
fn an_answer_in_the_mirror_promotes_it_to_source() {
    let sb = sandbox("turn");
    let id = "aaaaaaaa-1111-2222-3333-444444444444";
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let msg = format!("{}", "hello turn ".repeat(60));
    let omp_path = dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl"));
    fs::write(&omp_path,
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-10-01T10:00:00.000Z\",\"cwd\":\"/tmp/proj\"}}\n\
{{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-10-01T10:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"{msg}\"}}}}\n")).unwrap();

    // The user chose claude as the mirror target for this session.
    fs::create_dir_all(sb.join("state")).unwrap();
    let key = omp_path.display().to_string();
    let mut state = State::default();
    let mut src = Source::new("omp", &key);
    src.chosen_targets.insert("claude".to_string());
    state.sources.insert(key.clone(), src);
    state.save(&sb.join("state").join("state.json")).unwrap();

    let o = run(&sb);
    assert!(o.status.success(), "first pass stderr: {}", String::from_utf8_lossy(&o.stderr));
    let mirrors = walk(&sb.join("claude").join("projects"));
    assert_eq!(mirrors.len(), 1, "the first pass installs one claude mirror");
    let claude_path = mirrors[0].clone();

    // The user answers inside the claude copy: one more user record in claude's own shape.
    let answer = r#"{"type":"user","uuid":"u9","parentUuid":"u1","sessionId":"s","cwd":"/tmp/proj","timestamp":"2026-10-01T10:00:05.000Z","message":{"role":"user","content":"answered in claude"}}"#;
    let mut text = fs::read_to_string(&claude_path).unwrap();
    if !text.ends_with('\n') { text.push('\n'); }
    text.push_str(answer);
    text.push('\n');
    fs::write(&claude_path, text).unwrap();

    let o = run(&sb);
    assert!(o.status.success(), "second pass stderr: {}", String::from_utf8_lossy(&o.stderr));

    // The claude copy is now the source; the omp side holds the answer and the old omp file
    // (which lost the turn) is replaced, not left as a stale second copy.
    let omp_files = walk(&sb.join("omp").join("sessions"));
    assert_eq!(omp_files.len(), 1, "exactly one omp copy survives the turn");
    let omp_text = fs::read_to_string(&omp_files[0]).unwrap();
    assert!(omp_text.contains("answered in claude"), "the answer reached omp:\n{omp_text}");
    assert!(omp_text.contains("hello turn"), "the old turns are still there:\n{omp_text}");

    let disk = State::load(&sb.join("state").join("state.json")).unwrap();
    let new_src = disk.sources.get(&claude_path.display().to_string())
        .expect("the promoted copy is the recorded source");
    assert_eq!(new_src.fmt, "claude", "the winner's harness is the format");
    assert!(new_src.chosen_targets.contains("omp"), "the losing harness is still a target");
    assert!(disk.mirror_keys.contains(&omp_files[0].display().to_string()),
        "the surviving omp copy is recorded as a mirror");
    assert!(!disk.mirror_keys.contains(&omp_path.display().to_string()),
        "the losing file is gone, and its key with it");
    assert!(!omp_path.exists(), "the losing omp file was replaced by the winner's copy");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_quiet_mirror_promotes_nothing_on_the_next_pass() {
    let sb = sandbox("quiet");
    let id = "aaaaaaaa-1111-2222-3333-444444444444";
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let msg = format!("{}", "hello quiet ".repeat(60));
    let omp_path = dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl"));
    fs::write(&omp_path,
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-10-01T10:00:00.000Z\",\"cwd\":\"/tmp/proj\"}}\n\
{{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-10-01T10:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"{msg}\"}}}}\n")).unwrap();

    fs::create_dir_all(sb.join("state")).unwrap();
    let key = omp_path.display().to_string();
    let mut state = State::default();
    let mut src = Source::new("omp", &key);
    src.chosen_targets.insert("claude".to_string());
    state.sources.insert(key.clone(), src);
    state.save(&sb.join("state").join("state.json")).unwrap();

    assert!(run(&sb).status.success());
    let before = walk(&sb.join("claude").join("projects"));
    assert_eq!(before.len(), 1);

    // Two more passes with nothing touched: the source stays where it was.
    assert!(run(&sb).status.success());
    assert!(run(&sb).status.success());
    let after = walk(&sb.join("claude").join("projects"));
    assert_eq!(after, before, "no new copies appear");
    let disk = State::load(&sb.join("state").join("state.json")).unwrap();
    assert!(disk.sources.contains_key(&key), "the omp source is still the source");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_second_real_answer_turns_the_session_back() {
    // The crucial chain: omp -> claude -> omp. After the first turn the rewritten omp copy is
    // the mirror of the claude source; the user answers inside THAT copy, and the next pass
    // must promote it back — a real turn — while the daemon's own rewrite of the claude copy
    // (same pass) must not itself read as an edit.
    let sb = sandbox("chain");
    let id = "aaaaaaaa-1111-2222-3333-444444444444";
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let msg = format!("{}", "hello chain ".repeat(60));
    let omp_path = dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl"));
    fs::write(&omp_path,
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-10-01T10:00:00.000Z\",\"cwd\":\"/tmp/proj\"}}\n\
{{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-10-01T10:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"{msg}\"}}}}\n")).unwrap();

    fs::create_dir_all(sb.join("state")).unwrap();
    let key = omp_path.display().to_string();
    let mut state = State::default();
    let mut src = Source::new("omp", &key);
    src.chosen_targets.insert("claude".to_string());
    state.sources.insert(key.clone(), src);
    state.save(&sb.join("state").join("state.json")).unwrap();

    // Pass 1: mirror to claude.
    assert!(run(&sb).status.success());
    let claude_path = walk(&sb.join("claude").join("projects"))[0].clone();

    // The user answers in claude; pass 2 turns the session to claude and rewrites omp.
    let mut text = fs::read_to_string(&claude_path).unwrap();
    if !text.ends_with('\n') { text.push('\n'); }
    text.push_str(r#"{"type":"user","uuid":"u9","parentUuid":"u1","sessionId":"s","cwd":"/tmp/proj","timestamp":"2026-10-01T10:00:05.000Z","message":{"role":"user","content":"answered in claude"}}"#);
    text.push('\n');
    fs::write(&claude_path, text).unwrap();
    assert!(run(&sb).status.success());

    // The omp copy that now exists was rewritten from claude; the source is the claude file.
    let disk = State::load(&sb.join("state").join("state.json")).unwrap();
    assert!(disk.sources.contains_key(&claude_path.display().to_string()), "claude holds the turn");

    // The user answers again, inside the rewritten omp copy: one more user record.
    let omp_now = walk(&sb.join("omp").join("sessions"))[0].clone();
    let mut otext = fs::read_to_string(&omp_now).unwrap();
    if !otext.ends_with('\n') { otext.push('\n'); }
    otext.push_str(r#"{"type":"message","id":"m9","parentId":"m1","timestamp":"2026-10-01T10:00:09.000Z","message":{"role":"user","content":"answered in omp again"}}"#);
    otext.push('\n');
    fs::write(&omp_now, otext).unwrap();
    assert!(run(&sb).status.success());

    // The session turned back: omp is the source, the claude copy holds the omp answer.
    let disk = State::load(&sb.join("state").join("state.json")).unwrap();
    assert!(disk.sources.contains_key(&omp_now.display().to_string()),
        "the second answer made omp the source again");
    // The claude copy was re-minted under the new source key and the loser replaced.
    let claude_now = walk(&sb.join("claude").join("projects"));
    assert_eq!(claude_now.len(), 1, "exactly one claude copy after the turn");
    let claude_text = fs::read_to_string(&claude_now[0]).unwrap();
    assert!(claude_text.contains("answered in omp again"), "the omp answer reached claude:\n{claude_text}");
    assert!(!claude_path.exists(), "the losing claude file was replaced, not duplicated");
    assert!(disk.mirror_keys.contains(&claude_now[0].display().to_string()),
        "the claude copy is recorded as a mirror after losing the turn");
    let _ = fs::remove_dir_all(&sb);
}
