//! The Team tab: what each pulled row says, the "shared from this machine" section and its states,
//! and the pulls that happen without a ctrl+r (opening the tab, a push the daemon just made).

use sesync::app::{App, Ev, View};
use sesync::manage::Roots;
use sesync::state::{Source, State};
use sesync::team_watch::TeamCopy;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const LOCAL: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const REMOTE: &str = "bbbbbbbb-1111-2222-3333-444444444444";

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-teamtab-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// One local omp session; returns its key.
fn local_session(sb: &Path) -> String {
    let dir = sb.join("omp").join("sessions").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("2026-10-01T10-00-00-000Z_{LOCAL}.jsonl"));
    fs::write(&p, format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{LOCAL}\",\"cwd\":\"/tmp\",\"title\":\"Local one\"}}\n")).unwrap();
    p.display().to_string()
}

fn app(sb: &Path, state: State) -> App {
    let mut a = App::new(Roots::under(sb, sb.join("no-sesync-conv")), state, sb.join("locks"), sb.join("no-sesync_lock"));
    a.set_team_configured(true);
    a
}

fn copy(conversation: &str, title: &str, stamp: Option<&str>, mine: bool) -> TeamCopy {
    let mut text = format!("{{\"type\":\"session\",\"id\":\"{conversation}\",\"title\":\"{title}\"}}\n");
    if let Some(ts) = stamp {
        text.push_str(&format!("{{\"type\":\"message\",\"timestamp\":\"{ts}\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n"));
    }
    TeamCopy { space: "team".into(), conversation: conversation.into(), seq: 1, text: text.into_bytes(), mine }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()
}

#[test]
fn a_row_says_who_wrote_it_when_it_last_moved_and_whether_it_is_here() {
    let sb = sandbox("rows");
    local_session(&sb);
    let mut a = app(&sb, State::default());
    a.set_pull_result(vec![
        copy(LOCAL, "Local one", Some("2020-01-01T00:00:00Z"), true),
        copy(REMOTE, "From a teammate", None, false),
    ], 0);
    let rows = a.team_rows();
    let local = rows.iter().find(|r| r.title == "Local one").unwrap();
    assert_eq!((local.from, local.here, local.short_id.as_str()), ("this machine", true, "aaaaaaaa"));
    assert!(local.updated.ends_with("ago"), "the newest turn's time: {}", local.updated);
    let remote = rows.iter().find(|r| r.title == "From a teammate").unwrap();
    assert_eq!((remote.from, remote.here, remote.updated.as_str()), ("teammate", false, "-"));
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_shared_session_goes_from_waiting_to_on_the_team_to_changed() {
    let sb = sandbox("shared");
    let key = local_session(&sb);
    let mut st = State::default();
    st.share(LOCAL, "team");
    let mut a = app(&sb, st);
    let shared = a.shared_from_here();
    assert_eq!(shared.len(), 1);
    assert_eq!((shared[0].title.as_str(), shared[0].harness.as_str()), ("Local one", "omp"));
    assert!(shared[0].state.starts_with("team: waiting for the daemon"), "{}", shared[0].state);
    assert!(!a.take_refresh_request(), "nothing to pull yet");

    // The daemon pushed it: the row says so, and the Team list is pulled without a ctrl+r.
    let mut disk = State::default();
    disk.share(LOCAL, "team");
    let mut src = Source::new("omp", &key);
    src.last_seen_change = now_secs() - 600.0;
    disk.sources.insert(key.clone(), src.clone());
    disk.record_team_push("team", LOCAL, "hash", now_secs() - 60.0);
    a.absorb_daemon_state(&disk);
    assert!(a.shared_from_here()[0].state.starts_with("team: on the team, pushed"), "{}", a.shared_from_here()[0].state);
    assert!(a.take_refresh_request(), "a push the daemon made asks for a pull");

    // Changed after that push: it says it goes out again once quiet. No new push, no new pull.
    src.last_seen_change = now_secs();
    disk.sources.insert(key, src);
    a.absorb_daemon_state(&disk);
    assert!(a.shared_from_here()[0].state.contains("changed since its push"), "{}", a.shared_from_here()[0].state);
    assert!(!a.take_refresh_request());
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn opening_the_team_tab_pulls_only_when_the_list_is_stale() {
    let sb = sandbox("open-tab");
    let mut a = app(&sb, State::default());
    a.handle(Ev::Tab, Instant::now());
    assert_eq!(a.view(), View::Team);
    assert!(a.take_refresh_request(), "never pulled this run: opening the tab pulls");
    a.set_pull_result(vec![], 0);
    a.handle(Ev::Tab, Instant::now());
    a.handle(Ev::Tab, Instant::now());
    assert!(!a.take_refresh_request(), "pulled a moment ago: no new pull");

    let mut unconfigured = App::new(Roots::under(&sb, sb.join("no-sesync-conv")), State::default(), sb.join("locks"), sb.join("no-sesync_lock"));
    unconfigured.handle(Ev::Tab, Instant::now());
    assert!(!unconfigured.take_refresh_request(), "no team, nothing to pull");
    let _ = fs::remove_dir_all(&sb);
}
