//! The divergence dialog's five choices and "take theirs" against turn-taking mirrors. Every choice but
//! "decide later" keeps both turn histories reachable: the continued side keeps the conversation id,
//! the other side is a fork (a new omp session).

#[path = "support/team_list.rs"]
mod team_list;

use sesync::app::{App, Ev, Mode, Side};
use sesync::archive_store::read_session;
use sesync::manage::Roots;
use sesync::state::{Mirror, Source, State};
use sesync::turn::promote_changed;
use sesync::turns::{fingerprint, turns_of};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use team_list::*;

fn now() -> Instant {
    Instant::now()
}

/// The file this machine holds the diverged conversation in (written by `five_states`).
fn local_path(sb: &Path) -> PathBuf {
    Roots::under(sb, sb.join("no-sesync-conv")).locations.sessions_dir("omp").join("-tmp")
        .join(format!("2026-10-01T10-00-00-000Z_{DIVERGED}.jsonl"))
}

/// The diverged conversation, opened in the dialog: this machine holds [q, mine], the team [q, theirs].
fn open_dialog(sb: &Path) -> App {
    let mut a = five_states(sb);
    select_team_row(&mut a, DIVERGED);
    a.handle(Ev::Enter, now());
    assert_eq!(a.mode(), Mode::Resolve);
    a
}

/// This machine's copy as it stands once the fixture is written, and the omp files listed then.
fn start(sb: &Path) -> (Vec<u8>, Vec<PathBuf>) {
    let _ = five_states(sb);
    (fs::read(local_path(sb)).unwrap(), files_under(&sb.join("omp")))
}

fn team_bytes() -> Vec<u8> {
    journal(DIVERGED, "Diverged", &[("q", "a"), ("theirs", "t")])
}

/// The user prompts of a journal, in order.
fn prompts(bytes: &[u8]) -> Vec<String> {
    turns_of(bytes).unwrap().into_iter().map(|t| t.user).collect()
}

/// Every file under `dir`, sorted.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() { walk(&p, out) } else { out.push(p) }
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, &mut v);
    v.sort();
    v
}

/// The session id a session file is named for: `<timestamp>_<id>.jsonl`.
fn id_of_file(p: &Path) -> String {
    let name = p.file_name().unwrap().to_string_lossy().to_string();
    name.split_once('_').unwrap().1.trim_end_matches(".jsonl").to_string()
}

/// The one session file a choice added under omp: those not listed before it ran.
fn the_fork(sb: &Path, before: &[PathBuf]) -> PathBuf {
    let added: Vec<PathBuf> = files_under(&sb.join("omp")).into_iter().filter(|p| !before.contains(p)).collect();
    assert_eq!(added.len(), 1, "one fork written into omp: {added:?}");
    added.into_iter().next().unwrap()
}

#[test]
fn the_dialog_shows_the_common_history_and_each_sides_new_turns() {
    let sb = sandbox("dialog-content");
    let a = open_dialog(&sb);
    let r = a.resolve().expect("the dialog is open");
    assert_eq!(r.common, 1, "both copies share the first turn");
    assert_eq!((r.local_new.len(), r.team_new.len()), (1, 1));
    assert_eq!(r.local_new[0].user, "mine");
    assert_eq!(r.team_new[0].user, "theirs");
    assert!(!r.behind);
    assert_eq!(r.focus, Side::Local);
    assert_eq!(r.team_seq, 3, "the team record the comparison read");
    assert_eq!(r.team_who, "fp-diverged", "the team copy's writer, as LATEST BY shows it");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn enter_without_a_choice_runs_nothing_and_says_so() {
    let sb = sandbox("no-choice");
    let mut a = open_dialog(&sb);
    a.handle(Ev::Enter, now());
    assert_eq!(a.mode(), Mode::Resolve);
    assert!(a.status().contains("choose an option"), "{}", a.status());
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn decide_later_and_esc_change_nothing() {
    let sb = sandbox("later");
    let (before, _) = start(&sb);
    let mut a = open_dialog(&sb);
    a.handle(Ev::Esc, now());
    assert_eq!(a.mode(), Mode::Browse);
    assert!(a.resolve().is_none());

    a.handle(Ev::Enter, now());
    a.handle(Ev::Char('5'), now());
    a.handle(Ev::Char('y'), now());
    assert_eq!(a.mode(), Mode::Browse);
    assert!(a.state().team_force("team", DIVERGED).is_none(), "nothing was asked of the daemon");
    assert!(a.state().superseded_at(DIVERGED).is_none(), "nothing was replaced");
    assert_eq!(fs::read(local_path(&sb)).unwrap(), before, "this machine's copy is untouched");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn keep_mine_keep_theirs_here_pushes_this_copy_and_keeps_the_team_version_as_a_local_fork() {
    let sb = sandbox("keep-here");
    let (before, listed) = start(&sb);
    let mut a = open_dialog(&sb);
    a.handle(Ev::Char('3'), now());
    a.handle(Ev::Char('y'), now());

    let force = a.state().team_force("team", DIVERGED).expect("a keep-mine request");
    assert_eq!(force.team_seq, 3, "only the team record reviewed");
    assert_eq!(force.local, fingerprint(&turns_of(&before).unwrap()), "only this machine's turns reviewed");
    assert_eq!(fs::read(local_path(&sb)).unwrap(), before, "this machine's copy keeps its id and its turns");

    let fork = the_fork(&sb, &listed);
    assert_eq!(prompts(&fs::read(&fork).unwrap()), vec!["q", "theirs"], "the team's turns are reachable as a fork");
    assert!(fs::read_to_string(&fork).unwrap().contains("Diverged (fp-diverged, forked)"));
    assert!(a.state().shares_of(&id_of_file(&fork)).is_empty(), "the fork is not shared");
    assert!(a.state().shares_of(DIVERGED).contains("team"), "the original stays shared");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn keep_mine_keep_theirs_on_team_also_shares_the_team_fork_as_its_own_conversation() {
    let sb = sandbox("keep-team");
    let (_, listed) = start(&sb);
    let mut a = open_dialog(&sb);
    a.handle(Ev::Char('4'), now());
    a.handle(Ev::Char('y'), now());
    assert!(a.state().team_force("team", DIVERGED).is_some(), "this machine's copy is pushed");
    let fork = the_fork(&sb, &listed);
    assert_eq!(prompts(&fs::read(&fork).unwrap()), vec!["q", "theirs"]);
    assert!(a.state().shares_of(&id_of_file(&fork)).contains("team"), "the team's fork is shared as its own conversation");
    let _ = fs::remove_dir_all(&sb);
}

/// Take theirs has run (option `choice`). Returns the app and the state file the daemon reads after it.
fn take_theirs(sb: &Path, choice: char) -> (App, State) {
    let state_path = sb.join("state.json");
    let mut a = open_dialog(sb);
    a.set_archive_dir(sb.join("archive"));
    a.handle(Ev::Char(choice), now());
    a.handle(Ev::Char('y'), now());
    a.run_pending_replace(&state_path);
    let st = State::load(&state_path).unwrap();
    (a, st)
}

#[test]
fn take_theirs_keep_mine_here_replaces_the_copy_archives_the_old_one_and_keeps_mine_as_a_fork() {
    let sb = sandbox("take-here");
    let (before, listed) = start(&sb);
    let (a, saved) = take_theirs(&sb, '1');

    assert_eq!(fs::read(local_path(&sb)).unwrap(), team_bytes(), "the conversation's copy is the team's, under the same id");
    assert!(a.status().starts_with("took theirs"), "{}", a.status());
    assert!(saved.superseded_at(DIVERGED).is_some_and(|at| at > 0.0), "the replacement is recorded in the state file");
    let kept = files_under(&sb.join("archive")).into_iter().filter_map(|p| read_session(&p).ok()).any(|b| b == before);
    assert!(kept, "the replaced copy's bytes are in the archive, read back");

    let fork = the_fork(&sb, &listed);
    assert_eq!(prompts(&fs::read(&fork).unwrap()), vec!["q", "mine"], "this machine's turns are reachable as a fork");
    assert!(fs::read_to_string(&fork).unwrap().contains("Diverged (mine, forked)"));
    assert!(saved.shares_of(&id_of_file(&fork)).is_empty(), "the fork is not shared");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn take_theirs_keep_mine_on_team_also_shares_the_fork_as_its_own_conversation() {
    let sb = sandbox("take-team");
    let (_, listed) = start(&sb);
    let (_a, saved) = take_theirs(&sb, '2');
    assert_eq!(fs::read(local_path(&sb)).unwrap(), team_bytes());
    let fork = the_fork(&sb, &listed);
    assert_eq!(prompts(&fs::read(&fork).unwrap()), vec!["q", "mine"]);
    assert!(saved.shares_of(&id_of_file(&fork)).contains("team"), "this machine's fork is shared as its own conversation");
    assert!(saved.shares_of(DIVERGED).contains("team"), "the continued conversation stays shared");
    let _ = fs::remove_dir_all(&sb);
}

/// Take theirs has run. The source is the team's copy; the mirror is the old local copy, which the user
/// answered in before the replacement (its bytes are not the ones sesync wrote).
fn replaced_with_a_claude_mirror(sb: &Path) -> (State, String, f64) {
    let (_a, mut st) = take_theirs(sb, '1');
    let replaced = st.superseded_at(DIVERGED).expect("recorded");
    let mirror = sb.join("claude-copy.jsonl");
    fs::write(&mirror, b"the old local copy, answered in claude").unwrap();
    let key = local_path(sb).display().to_string();
    let mut src = Source::new("omp", &key);
    src.conv = DIVERGED.into();
    src.mirrors.insert("claude".into(), Mirror {
        target: "claude".into(),
        path: mirror.display().to_string(),
        installed_hash: "what sesync wrote".into(),
        ..Default::default()
    });
    st.sources.insert(key, src);
    (st, mirror.display().to_string(), replaced)
}

#[test]
fn a_claude_mirror_answered_before_take_theirs_is_not_promoted_back_over_the_team_copy() {
    let sb = sandbox("mirror-guard");
    let (mut st, _mirror, replaced) = replaced_with_a_claude_mirror(&sb);
    // The answer was typed in Claude before the replacement; the team copy's last prompt is older.
    let prompt_of = move |fmt: &str, _key: &str| match fmt {
        "claude" => Some(replaced - 5.0),
        "omp" => Some(replaced - 100.0),
        _ => None,
    };
    let promoted = promote_changed(&mut st, &Default::default(), &prompt_of);
    assert!(promoted.is_empty(), "the superseded answer must not replace the team's copy: {promoted:?}");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_claude_answer_typed_after_take_theirs_is_still_a_turn() {
    let sb = sandbox("mirror-after");
    let (mut st, mirror, replaced) = replaced_with_a_claude_mirror(&sb);
    let prompt_of = move |fmt: &str, _key: &str| match fmt {
        "claude" => Some(replaced + 5.0),
        "omp" => Some(replaced - 100.0),
        _ => None,
    };
    let promoted = promote_changed(&mut st, &Default::default(), &prompt_of);
    assert_eq!(promoted, vec![mirror], "an answer after the replacement is a new turn, promoted as usual");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_dialog_scrolls_each_side_and_switches_focus_with_tab() {
    let sb = sandbox("scroll");
    write_local(&sb, DIVERGED, "Diverged", &[("q", "a"), ("m1", "x"), ("m2", "x"), ("m3", "x")]);
    let mut st = State::default();
    st.share(DIVERGED, "team");
    let mut a = app_for(&sb, st);
    a.set_pull_result(vec![team_copy(DIVERGED, 5,
        journal(DIVERGED, "Diverged", &[("q", "a"), ("t1", "x"), ("t2", "x"), ("t3", "x")]), false, "fp")], 0);
    a.refresh_rows();
    compare_all(&mut a, &sb);
    select_team_row(&mut a, DIVERGED);
    a.handle(Ev::Enter, now());
    assert_eq!(a.mode(), Mode::Resolve);

    a.handle(Ev::Down, now());
    a.handle(Ev::Down, now());
    a.handle(Ev::Down, now());
    assert_eq!(a.resolve().unwrap().scroll, [2, 0], "the focused side scrolls, clamped to its last turn");
    a.handle(Ev::Tab, now());
    assert_eq!(a.resolve().unwrap().focus, Side::Team);
    a.handle(Ev::Down, now());
    assert_eq!(a.resolve().unwrap().scroll, [2, 1]);
    a.handle(Ev::Up, now());
    a.handle(Ev::Up, now());
    assert_eq!(a.resolve().unwrap().scroll, [2, 0]);
    let _ = fs::remove_dir_all(&sb);
}
