//! The session preview: each harness gives its last turns oldest first (tool calls by name,
//! thinking and tool results left out), the worker reads a new message without being asked
//! again, and the details pane draws the newest turn last and scrolls back. Sandboxes only.

use ratatui::{backend::TestBackend, Terminal};
use sesync::app::{App, Ev};
use sesync::harness::{self, Location, Locations, Message, Role, Tools};
use sesync::preview::{Loaded, Worker};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static COUNTER: AtomicUsize = AtomicUsize::new(0);
const SID: &str = "aaaaaaaa-1111-2222-3333-444444444444";

fn sandbox(name: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("sesync-preview-test-{}-{n}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

/// The last turns of `key` in harness `id`, read the way the preview worker reads them.
fn turns(id: &str, root: &Path, key: &Path, scratch: &Path, max: usize) -> Vec<Message> {
    let conv = sesync_conv();
    let tools = Tools { home: root, sesync_conv: &conv };
    let loc = Location { root: root.to_path_buf(), cli: None };
    harness::get(id).unwrap().recent_messages(&loc, &tools, &key.display().to_string(), scratch, max).unwrap()
}

fn said(turns: &[Message]) -> Vec<(Role, String)> {
    turns.iter().map(|m| (m.role, m.text.clone())).collect()
}

fn omp_message(id: &str, role: &str, content: serde_json::Value) -> String {
    serde_json::json!({"type": "message", "id": id, "timestamp": "2026-10-01T10:00:00.000Z",
                       "message": {"role": role, "content": content}}).to_string()
}

fn omp_journal(dir: &Path, lines: &[String]) -> PathBuf {
    let path = dir.join("omp").join("sessions").join("-p").join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let header = format!(r#"{{"type":"session","version":3,"id":"{SID}","cwd":"/p"}}"#);
    fs::write(&path, format!("{header}\n{}\n", lines.join("\n"))).unwrap();
    path
}

#[test]
fn an_omp_journal_shows_its_last_turns_oldest_first() {
    let sb = sandbox("omp");
    let path = omp_journal(&sb, &[
        omp_message("m1", "user", "first question".into()),
        omp_message("m2", "assistant", serde_json::json!([
            {"type": "thinking", "thinking": "private reasoning"},
            {"type": "text", "text": "looking"},
            {"type": "toolCall", "name": "bash", "arguments": {}},
        ])),
        omp_message("m3", "toolResult", serde_json::json!([{"type": "text", "text": "tool output"}])),
        omp_message("m4", "assistant", serde_json::json!([{"type": "text", "text": "the answer"}])),
        omp_message("m5", "user", "second question".into()),
    ]);
    let all = turns("omp", &sb.join("omp"), &path, &sb.join("scratch"), 20);
    assert_eq!(said(&all), vec![
        (Role::User, "first question".to_string()),
        (Role::Assistant, "looking\n[tool: bash]\nthe answer".to_string()),
        (Role::User, "second question".to_string()),
    ], "one assistant turn across its tool round; thinking and tool output are not shown");
    let last_two = turns("omp", &sb.join("omp"), &path, &sb.join("scratch"), 2);
    assert_eq!(last_two.first().map(|m| m.role), Some(Role::Assistant), "max keeps the newest turns");
    assert_eq!(last_two.len(), 2);
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_turn_beyond_the_first_window_is_still_found_and_a_long_one_is_clipped() {
    let sb = sandbox("long");
    let mut lines: Vec<String> = (0..400).map(|i| omp_message(&format!("e{i}"), "user", format!("early {i}").into())).collect();
    lines.push(omp_message("q", "user", "before the long answer".into()));
    // 600 KB in one record: the first window from the end holds only part of it.
    lines.push(omp_message("a", "assistant", serde_json::json!([{"type": "text", "text": "x".repeat(600 * 1024)}])));
    lines.push(omp_message("z", "user", "after".into()));
    let path = omp_journal(&sb, &lines);
    let got = turns("omp", &sb.join("omp"), &path, &sb.join("scratch"), 3);
    assert_eq!(got.len(), 3, "{:?}", said(&got).iter().map(|(r, t)| (r, t.len())).collect::<Vec<_>>());
    assert_eq!(got[0].text, "before the long answer");
    assert!(got[1].text.chars().count() <= 2001 && got[1].text.starts_with('…'), "a long turn keeps its end");
    assert_eq!(got[2].text, "after");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_claude_transcript_shows_turns_without_meta_commands_or_subagent_work() {
    let sb = sandbox("claude");
    let path = sb.join("projects").join("-p").join(format!("{SID}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let rec = |kind: &str, extra: serde_json::Value, content: serde_json::Value| {
        let mut r = serde_json::json!({"type": kind, "sessionId": SID, "timestamp": "2026-10-01T10:00:00.000Z",
                                       "message": {"role": kind, "content": content}});
        r.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        r.to_string()
    };
    let none = serde_json::json!({});
    let lines = [
        rec("user", serde_json::json!({"isMeta": true}), "Caveat: meta".into()),
        rec("user", none.clone(), "<command-name>/clear</command-name>".into()),
        rec("user", none.clone(), "the real question".into()),
        rec("assistant", none.clone(), serde_json::json!([{"type": "text", "text": "part one"}])),
        rec("assistant", none.clone(), serde_json::json!([{"type": "tool_use", "name": "Read", "input": {}}])),
        rec("user", none.clone(), serde_json::json!([{"type": "tool_result", "tool_use_id": "t", "content": "file text"}])),
        rec("assistant", serde_json::json!({"isSidechain": true}), serde_json::json!([{"type": "text", "text": "subagent noise"}])),
        rec("assistant", none.clone(), serde_json::json!([{"type": "thinking", "thinking": "hmm"}, {"type": "text", "text": "part two"}])),
    ];
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    let got = turns("claude", &sb, &path, &sb.join("scratch"), 20);
    assert_eq!(said(&got), vec![
        (Role::User, "the real question".to_string()),
        (Role::Assistant, "part one\n[tool: Read]\npart two".to_string()),
    ]);
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn an_opencode_session_shows_its_text_and_tools_from_the_store() {
    let sb = sandbox("opencode");
    let db = sb.join("opencode.db");
    let con = rusqlite::Connection::open(&db).unwrap();
    con.execute_batch(
        "create table session (id text primary key, directory text, title text, time_created integer,
                               time_updated integer, time_archived integer, parent_id text);
         create table message (id text primary key, session_id text, time_created integer, time_updated integer, data text);
         create table part (id text primary key, message_id text, session_id text, time_created integer, time_updated integer, data text);
         insert into session values ('ses_p', '/p', 'Preview', 1, 1, null, null);",
    ).unwrap();
    let message = |id: &str, at: i64, role: &str, parts: &[serde_json::Value]| {
        con.execute("insert into message values (?1, 'ses_p', ?2, ?2, ?3)",
            rusqlite::params![id, at, serde_json::json!({"role": role}).to_string()]).unwrap();
        for (i, p) in parts.iter().enumerate() {
            con.execute("insert into part values (?1, ?2, 'ses_p', ?3, ?3, ?4)",
                rusqlite::params![format!("{id}-{i}"), id, at + i as i64, p.to_string()]).unwrap();
        }
    };
    message("m1", 1_000, "user", &[serde_json::json!({"type": "text", "text": "hello"})]);
    message("m2", 2_000, "assistant", &[
        serde_json::json!({"type": "reasoning", "text": "thinking it over"}),
        serde_json::json!({"type": "text", "text": "hi there"}),
        serde_json::json!({"type": "tool", "tool": "bash", "state": {}}),
    ]);
    message("m3", 3_000, "user", &[serde_json::json!({"type": "text", "text": "a system reminder", "synthetic": true})]);
    message("m4", 4_000, "user", &[serde_json::json!({"type": "text", "text": "follow up"})]);
    drop(con);
    let loc = Location { root: db.clone(), cli: None };
    let tools = Tools { home: &sb, sesync_conv: Path::new("/no/converter") };
    let got = harness::get("opencode").unwrap().recent_messages(&loc, &tools, "ses_p", &sb.join("scratch"), 20).unwrap();
    assert_eq!(said(&got), vec![
        (Role::User, "hello".to_string()),
        (Role::Assistant, "hi there\n[tool: bash]".to_string()),
        (Role::User, "follow up".to_string()),
    ], "reasoning and synthetic text are not turns");
    assert_eq!(got[2].at, 4.0, "times come from the store, in seconds");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn an_agy_conversation_shows_through_the_converter_and_leaves_no_journal_behind() {
    let sb = sandbox("agy");
    let store = sb.join("agy");
    let db = store.join("conversations").join("28b4f6a2-2ecf-4ad2-933f-25dfd05896bf.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    fs::copy(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/tests/fixtures/agy-1.4.4-read-file.db"), &db).unwrap();
    let scratch = sb.join("scratch");
    let got = turns("antigravity", &store, &db, &scratch, 20);
    assert!(got.iter().any(|m| m.role == Role::User && m.text.contains("TOOLREAD please read hello")), "{:?}", said(&got));
    assert!(got.iter().any(|m| m.role == Role::Assistant), "{:?}", said(&got));
    let left: Vec<_> = fs::read_dir(&scratch).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    assert!(left.is_empty(), "the converted journal is removed once read: {left:?}");
    let _ = fs::remove_dir_all(&sb);
}

/// The newest answer the worker gives within five seconds that satisfies `ok`.
fn wait_for(w: &Worker, ok: impl Fn(&Loaded) -> bool) -> Option<Loaded> {
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if let Some(l) = w.take() {
            if ok(&l) {
                return Some(l);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn the_worker_reads_a_new_message_without_being_asked_again() {
    let sb = sandbox("worker");
    let path = omp_journal(&sb, &[omp_message("m1", "user", "first question".into())]);
    let mut locations = Locations::default();
    locations.set("omp", Location { root: sb.join("omp"), cli: None });
    let w = Worker::start(locations, sb.clone(), PathBuf::from("/no/converter"));
    let key = path.display().to_string();
    w.select("omp", &key);
    let first = wait_for(&w, |l| l.key == key).expect("the selected session is read");
    assert_eq!(first.messages.unwrap().len(), 1);

    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str(&omp_message("m2", "assistant", serde_json::json!([{"type": "text", "text": "a new answer"}])));
    text.push('\n');
    fs::write(&path, text).unwrap();
    let again = wait_for(&w, |l| l.messages.as_ref().is_ok_and(|m| m.last().is_some_and(|t| t.text == "a new answer")));
    assert!(again.is_some(), "a change on disk is read again without a new selection");
    drop(w);
    let _ = fs::remove_dir_all(&sb);
}

fn screen(a: &App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| sesync::ui::draw(f, a)).unwrap();
    let buf = term.backend().buffer();
    (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect()
}

#[test]
fn the_details_pane_shows_the_newest_turn_last_and_scrolls_back() {
    let sb = sandbox("pane");
    let path = omp_journal(&sb, &[omp_message("m1", "user", "hello".into())]);
    let mut a = App::new(sesync::manage::Roots::under(&sb, sb.join("no-sesync-conv")),
        sesync::state::State::default(), sb.join("locks"), sb.join("no-sesync_lock"));
    let key = path.display().to_string();
    assert_eq!(a.preview_wanted(), Some(("omp".to_string(), key.clone())), "the highlighted session is previewed");
    a.preview_requested(&key);
    assert!(screen(&a, 140, 30).contains("reading…"), "the pane says it is reading");

    let many: Vec<Message> = (0..12).map(|i| Message {
        role: if i % 2 == 0 { Role::User } else { Role::Assistant },
        text: format!("turn number {i}"),
        at: 0.0,
    }).collect();
    a.apply_preview(Loaded { key: "some other session".into(), messages: Ok(Vec::new()) });
    assert!(screen(&a, 140, 30).contains("reading…"), "an answer for another session is dropped");
    a.apply_preview(Loaded { key: key.clone(), messages: Ok(many) });
    let text = screen(&a, 140, 30);
    assert!(text.contains("turn number 11"), "the newest turn is on screen:\n{text}");
    assert!(!text.contains("turn number 0 "), "the oldest is scrolled away:\n{text}");

    for _ in 0..30 {
        a.handle(Ev::PreviewUp, Instant::now());
    }
    let back = screen(&a, 140, 30);
    assert!(back.contains("turn number 0"), "scrolling back reaches the oldest turn:\n{back}");
    assert!(!back.contains("turn number 11"), "and the newest leaves the pane:\n{back}");
    let _ = fs::remove_dir_all(&sb);
}
