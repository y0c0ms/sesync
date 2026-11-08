//! Fixtures for the Team tab's tests: local omp sessions with completed turns, team copies as a pull
//! returns them, and the comparison the worker runs (`team_compare::evaluate`), done synchronously.
#![allow(dead_code)]

use sesync::app::App;
use sesync::harness::Tools;
use sesync::manage::Roots;
use sesync::state::State;
use sesync::team_compare::evaluate;
use sesync::team_watch::TeamCopy;
use std::fs;
use std::path::{Path, PathBuf};

pub const SAME: &str = "c1c1c1c1-1111-2222-3333-444444444444";
pub const AHEAD: &str = "c2c2c2c2-1111-2222-3333-444444444444";
pub const BEHIND: &str = "c3c3c3c3-1111-2222-3333-444444444444";
pub const DIVERGED: &str = "c4c4c4c4-1111-2222-3333-444444444444";
pub const SHARED: &str = "c5c5c5c5-1111-2222-3333-444444444444";
pub const TEAM_ONLY: &str = "c6c6c6c6-1111-2222-3333-444444444444";

pub fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-teamlist-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// An omp journal of completed turns: each (prompt, answer) is one user message and one final
/// assistant message, chained by parent ids. No turns when `turns` is empty.
pub fn journal(id: &str, title: &str, turns: &[(&str, &str)]) -> Vec<u8> {
    let mut lines = vec![format!(
        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp","title":"{title}"}}"#)];
    let mut parent = "null".to_string();
    for (i, (prompt, answer)) in turns.iter().enumerate() {
        let u = format!("u{i}-{id}");
        let a = format!("a{i}-{id}");
        lines.push(format!(
            r#"{{"type":"message","id":"{u}","parentId":{parent},"timestamp":"2026-10-01T10:{:02}:00.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{prompt}"}}]}}}}"#,
            i * 2 + 1));
        lines.push(format!(
            r#"{{"type":"message","id":"{a}","parentId":"{u}","timestamp":"2026-10-01T10:{:02}:00.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{answer}"}}],"stopReason":"stop"}}}}"#,
            i * 2 + 2));
        parent = format!("\"{a}\"");
    }
    (lines.join("\n") + "\n").into_bytes()
}

/// Write a local omp session for `id` with these turns, as this machine's copy. Returns its key.
pub fn write_local(sb: &Path, id: &str, title: &str, turns: &[(&str, &str)]) -> String {
    let dir = Roots::under(sb, sb.join("no-sesync-conv")).locations.sessions_dir("omp").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl"));
    fs::write(&p, journal(id, title, turns)).unwrap();
    p.display().to_string()
}

/// A verified team copy, as one pull returns it.
pub fn team_copy(conv: &str, seq: u64, text: Vec<u8>, mine: bool, by: &str) -> TeamCopy {
    TeamCopy { space: "team".into(), conversation: conv.into(), seq, text, mine, by: by.into() }
}

pub fn app_for(sb: &Path, state: State) -> App {
    let mut a = App::new(Roots::under(sb, sb.join("no-sesync-conv")), state, sb.join("locks"), sb.join("no-sesync_lock"));
    a.set_team_configured(true);
    a
}

/// Run the comparisons the list asked for, the way the worker does, and apply the answers.
pub fn compare_all(a: &mut App, sb: &Path) {
    let locations = Roots::under(sb, sb.join("no-sesync-conv")).locations;
    let conv_dir = sb.join("no-sesync-conv");
    let tools = Tools { home: sb, sesync_conv: &conv_dir };
    let scratch = sb.join("scratch");
    let results: Vec<_> = a.take_compare_jobs().iter()
        .map(|j| evaluate(j, &locations, &tools, &scratch))
        .collect();
    a.apply_compared(results);
}

/// One local session per conversation, shared with team "team", and a pull that holds a copy for
/// each, so the Team list shows all five states plus a team-only row:
/// in step (`=`), waiting to push (`↑`), behind (`↓`), diverged (`≠`), shared with nothing to
/// compare (`•`), and a team copy this machine does not hold (`↓`, not here).
pub fn five_states(sb: &Path) -> App {
    let mut st = State::default();
    for conv in [SAME, AHEAD, BEHIND, DIVERGED, SHARED] {
        st.share(conv, "team");
    }
    write_local(sb, SAME, "Same", &[("ask one", "answer one")]);
    write_local(sb, AHEAD, "Ahead", &[("q", "a"), ("q2", "a2")]);
    write_local(sb, BEHIND, "Behind", &[("q", "a")]);
    write_local(sb, DIVERGED, "Diverged", &[("q", "a"), ("mine", "m")]);
    write_local(sb, SHARED, "Shared", &[]);
    st.share(TEAM_ONLY, "team");
    let mut a = app_for(sb, st);
    a.set_pull_result(vec![
        team_copy(SAME, 1, journal(SAME, "Same", &[("ask one", "answer one")]), false, "fp-same"),
        team_copy(AHEAD, 1, journal(AHEAD, "Ahead", &[("q", "a")]), true, ""),
        team_copy(BEHIND, 2, journal(BEHIND, "Behind", &[("q", "a"), ("q2", "a2")]), false, "fp-behind"),
        team_copy(DIVERGED, 3, journal(DIVERGED, "Diverged", &[("q", "a"), ("theirs", "t")]), false, "fp-diverged"),
        team_copy(TEAM_ONLY, 4, journal(TEAM_ONLY, "Team only", &[("q", "a")]), false, "fp-only"),
    ], 0);
    a.refresh_rows();
    compare_all(&mut a, sb);
    a
}

/// Move the Team cursor onto `conv`'s row, switching to the Team tab first.
pub fn select_team_row(a: &mut App, conv: &str) {
    use sesync::app::{Ev, View};
    if a.view() != View::Team {
        a.handle(Ev::Tab, std::time::Instant::now());
    }
    let rows = a.team_rows().len();
    let idx = a.team_rows().iter().position(|r| r.conversation == conv)
        .unwrap_or_else(|| panic!("no Team row for {conv}"));
    for _ in 0..rows {
        a.handle(Ev::Up, std::time::Instant::now());
    }
    for _ in 0..idx {
        a.handle(Ev::Down, std::time::Instant::now());
    }
    assert_eq!(a.team_index(), idx);
}

/// The screen as text, one line per row, for the render tests.
pub fn screen_text(a: &App, w: u16, h: u16) -> String {
    use ratatui::{backend::TestBackend, Terminal};
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| sesync::ui::draw(f, a)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        for x in 0..w {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}
