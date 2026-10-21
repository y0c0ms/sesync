//! The sessions-list filter: `/` types a fuzzy filter with live narrowing, Esc clears it,
//! and `f` cycles the scope (all / marked / mirrored / failing). Pure key-driven state; the
//! screen is drawn to a test buffer only where the footer matters.
use ratatui::{backend::TestBackend, Terminal};
use sesync::app::{App, Ev, Mode, View};
use sesync::manage::Roots;
use sesync::state::State;
use sesync::ui::draw;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const SID_A: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const SID_B: &str = "bbbbbbbb-1111-2222-3333-444444444444";
const SID_C: &str = "cccccccc-1111-2222-3333-444444444444";

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-filter-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn write_session(sb: &Path, sid: &str, title: &str) -> PathBuf {
    let dir = sb.join("omp").join("sessions").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let p = dir.join(format!("2026-10-01T10-00-00-000Z_{sid}.jsonl"));
    fs::write(&p, format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{sid}\",\"cwd\":\"/tmp\",\"title\":\"{title}\"}}\n")).unwrap();
    p
}

/// Three sessions: B is marked, C has a chosen mirror that keeps failing.
fn fixture(name: &str) -> (PathBuf, App) {
    let sb = sandbox(name);
    write_session(&sb, SID_A, "Keep me");
    let _b = write_session(&sb, SID_B, "Find me");
    let c = write_session(&sb, SID_C, "Failing one");
    let mut state = State::default();
    state.share(SID_B, "team-a");
    let mut src = sesync::state::Source::new("omp", c.to_str().unwrap());
    src.chosen_targets.insert("claude".to_string());
    src.failures = 1;
    state.sources.insert(c.to_string_lossy().to_string(), src);
    let app = App::new(
        Roots::under(&sb, sb.join("no-sesync-conv")),
        state,
        sb.join("locks"),
        sb.join("no-sesync_lock"),
    );
    (sb, app)
}

fn type_text(a: &mut App, s: &str) {
    for c in s.chars() {
        a.handle(Ev::Char(c), Instant::now());
    }
}

fn titles(a: &App) -> Vec<String> {
    a.visible_rows().iter().map(|g| g.title.clone()).collect()
}

fn footer(a: &App) -> String {
    let mut term = Terminal::new(TestBackend::new(120, 12)).unwrap();
    term.draw(|f| draw(f, a)).unwrap();
    let buf = term.backend().buffer();
    let y = 11;
    (0..120).map(|x| buf[(x, y)].symbol().to_string()).collect()
}

#[test]
fn slash_narrows_the_list_live_and_esc_clears_it() {
    let (sb, mut a) = fixture("slash");
    assert_eq!(titles(&a).len(), 3);
    a.handle(Ev::Char('/'), Instant::now());
    assert_eq!(a.mode(), Mode::Filter);
    type_text(&mut a, "find");
    assert_eq!(titles(&a), vec!["Find me"], "the list narrows as it is typed");
    assert!(a.status().contains("1 of 3"), "status counts the visible rows: {}", a.status());
    a.handle(Ev::Esc, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert_eq!(titles(&a).len(), 3, "esc clears the filter");
    assert_eq!(a.status(), "filter cleared");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn enter_keeps_the_typed_filter() {
    let (sb, mut a) = fixture("keep");
    a.handle(Ev::Char('/'), Instant::now());
    type_text(&mut a, "find");
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Browse);
    assert_eq!(titles(&a), vec!["Find me"], "the filter stays applied after enter");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn f_cycles_all_marked_mirrored_failing() {
    let (sb, mut a) = fixture("cycle");
    let mut seen = Vec::new();
    for _ in 0..4 {
        a.handle(Ev::Char('f'), Instant::now());
        seen.push((a.footer(), titles(&a)));
    }
    assert_eq!(seen[0].1, vec!["Find me"], "marked: only the marked session");
    assert_eq!(seen[1].1, vec!["Failing one"], "mirrored: the one with a chosen target");
    assert_eq!(seen[2].1, vec!["Failing one"], "failing: the one whose write failed");
    assert_eq!(seen[3].1.len(), 3, "and back to all");
    assert_eq!(seen[0].0.contains("f view:marked"), true, "footer names the scope: {}", seen[0].0);
    assert_eq!(seen[1].0.contains("f view:mirrored"), true, "footer names the scope: {}", seen[1].0);
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_footer_names_the_scope_and_the_status_counts_rows() {
    let (sb, mut a) = fixture("footer");
    assert_eq!(a.view(), View::Sessions);
    a.handle(Ev::Char('f'), Instant::now());
    assert!(footer(&a).contains("f view:marked"), "footer shows the active scope");
    assert!(a.status().contains("1 of 3"), "status: {}", a.status());
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_stale_failure_counter_without_mirror_intent_is_not_failing() {
    // The old auto-mirror binary left `failures` counters on sessions that now have nothing
    // chosen and no mirrors. They cannot fail anything; they must not show in the scope.
    let sb = sandbox("stale");
    let p = write_session(&sb, SID_A, "Keep me");
    let mut state = State::default();
    let mut src = sesync::state::Source::new("omp", p.to_str().unwrap());
    src.failures = 2770;
    state.sources.insert(p.to_string_lossy().to_string(), src);
    let mut a = App::new(
        Roots::under(&sb, sb.join("no-sesync-conv")),
        state,
        sb.join("locks"),
        sb.join("no-sesync_lock"),
    );
    a.handle(Ev::Char('f'), Instant::now());
    a.handle(Ev::Char('f'), Instant::now());
    a.handle(Ev::Char('f'), Instant::now()); // scope: failing
    assert_eq!(titles(&a), Vec::<String>::new(), "no intent, no mirror: nothing can be failing");
    // The same kind of session counts again once it has a mirror intent.
    let mut state2 = State::default();
    let mut src2 = sesync::state::Source::new("omp", p.to_str().unwrap());
    src2.failures = 2770;
    src2.chosen_targets.insert("pi".to_string());
    state2.sources.insert(p.to_string_lossy().to_string(), src2);
    let mut a2 = App::new(
        Roots::under(&sb, sb.join("no-sesync-conv")),
        state2,
        sb.join("locks"),
        sb.join("no-sesync_lock"),
    );
    a2.handle(Ev::Char('f'), Instant::now());
    a2.handle(Ev::Char('f'), Instant::now());
    a2.handle(Ev::Char('f'), Instant::now()); // scope: failing
    assert_eq!(titles(&a2).len(), 1, "with intent and a failing count it is failing again");
}

#[test]
fn the_team_mark_survives_a_real_turn() {
    // The bug: the mark was keyed on the source file's uuid, which changes when a mirror copy
    // is promoted — so a shared session silently stopped pushing after a turn. The id is now
    // the conversation's stable one and moves with the turn.
    let sb = sandbox("team-turn");
    let p = write_session(&sb, SID_A, "Keep me");
    let key = p.to_string_lossy().to_string();
    let mirror = sb.join("claude-mirror.jsonl");
    fs::write(&mirror, "installed by the daemon\n").unwrap();
    let mut state = State::default();
    let mut src = sesync::state::Source::new("omp", p.to_str().unwrap());
    src.conv = "conversation-uuid-1".to_string();
    src.chosen_targets.insert("claude".to_string());
    src.mirrors.insert("claude".to_string(), sesync::state::Mirror {
        target: "claude".into(),
        path: mirror.display().to_string(),
        source_hash: "old".into(),
        installed_hash: sesync::mirror::file_hash(&mirror).unwrap(),
        diverged: false,
        edit_stamp: String::new(),
        target_updated: 0.0,
    });
    state.sources.insert(key.clone(), src);
    state.share("conversation-uuid-1", "team-a");
    state.record_team_push("team-a", "conversation-uuid-1", "hash-before", 100.0);
    state.mirror_keys.insert(mirror.display().to_string());

    // The user answers in the claude copy: a prompt the source does not have. The next pass
    // turns the session to it.
    fs::write(&mirror, concat!(
        "installed by the daemon\n",
        r#"{"type":"user","timestamp":"2026-10-01T10:05:00.000Z","message":{"role":"user","content":"the user's answer"}}"#, "\n",
    )).unwrap();
    let locations = sesync::harness::Locations::default();
    let prompt_of = |fmt: &str, key: &str| sesync::harness::get(fmt)?.newest_prompt(locations.of(fmt), key);
    let promoted = sesync::turn::promote_changed(&mut state, &Default::default(), &prompt_of);
    assert_eq!(promoted, vec![mirror.display().to_string()], "the copy with the answer wins");

    let new_key = mirror.display().to_string();
    let new_src = &state.sources[&new_key];
    assert_eq!(new_src.conv, "conversation-uuid-1", "the conversation id moves with the turn");
    assert!(state.sources.get(&key).is_none(), "the old source entry is gone");
    assert!(state.is_shared("conversation-uuid-1"), "the share survives the turn");
    assert_eq!(state.team_id_of(&new_key), "conversation-uuid-1");
    assert_eq!(state.key_of_team_id("conversation-uuid-1").as_deref(), Some(new_key.as_str()),
        "the team pass finds the new source by the stable id");
    assert_eq!(state.team_hash("team-a", "conversation-uuid-1"), Some("hash-before".to_string()),
        "the pushed hash is keyed on the conversation, so the next push sees the change");
    let _ = fs::remove_dir_all(&sb);
}
