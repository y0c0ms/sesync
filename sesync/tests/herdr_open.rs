//! Opening a session in herdr, with a fake herdr: the resume command per harness, the match on a
//! running pane, the focus-or-create decision, and the menu wiring in the TUI. No herdr is started
//! and no real session store is read: every path is under a sandbox directory.
use rusqlite::Connection;
use sesync::app::{App, Ev, Mode};
use sesync::herdr_open::{self, Herdr, Opened, PaneProcs, Runner};
use sesync::manage::Roots;
use sesync::state::State;
use serde_json::json;
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

const SID: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const CLAUDE_ID: &str = "bbbbbbbb-1111-2222-3333-444444444444";

type Calls = Rc<RefCell<Vec<Vec<String>>>>;

/// A runner that answers from a script and records every call as `program args...`.
struct Fake {
    calls: Calls,
    answer: Rc<dyn Fn(&[String]) -> Result<String, String>>,
}

impl Runner for Fake {
    fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
        let mut line = vec![program.to_string()];
        line.extend(args.iter().cloned());
        self.calls.borrow_mut().push(line);
        (self.answer)(args)
    }
}

fn fake_herdr(answer: impl Fn(&[String]) -> Result<String, String> + 'static) -> (Herdr, Calls) {
    let calls = Calls::default();
    let runner = Fake { calls: calls.clone(), answer: Rc::new(answer) };
    (Herdr::new("herdr-bin", Some("w3".to_string()), Box::new(runner)), calls)
}

/// A herdr with these panes: (pane id, tab id, foreground argument lists). A new tab is always
/// `w9:t9` with root pane `w9:p9`. Every call succeeds except a process-info for an unknown pane.
fn herdr_with_panes(panes: Vec<(&str, &str, Vec<Vec<&str>>)>) -> (Herdr, Calls) {
    let panes: Vec<(String, String, Vec<Vec<String>>)> = panes
        .into_iter()
        .map(|(pane, tab, argvs)| (pane.to_string(), tab.to_string(), argvs.iter().map(|a| words(a)).collect()))
        .collect();
    fake_herdr(move |args| {
        let argv_words: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv_words.as_slice() {
            ["pane", "list"] => {
                let listed: Vec<_> = panes.iter().map(|(pane, tab, _)| json!({"pane_id": pane, "tab_id": tab})).collect();
                Ok(json!({"id": "cli:pane:list", "result": {"panes": listed}}).to_string())
            }
            ["pane", "process-info", "--pane", pane] => {
                let (_, _, argvs) = panes.iter().find(|(p, _, _)| p.as_str() == *pane).ok_or("pane not found")?;
                let procs: Vec<_> = argvs.iter().map(|argv| json!({"argv": argv, "cmdline": argv.join(" ")})).collect();
                Ok(json!({"result": {"process_info": {"foreground_processes": procs}}}).to_string())
            }
            ["tab", "create", ..] => Ok(json!({"result": {"tab": {"tab_id": "w9:t9"}, "root_pane": {"pane_id": "w9:p9"}}}).to_string()),
            ["pane", "run", ..] | ["tab", "focus", ..] | ["tab", "close", ..] => Ok(json!({"result": {}}).to_string()),
            other => Err(format!("unexpected herdr call {other:?}")),
        }
    })
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-herdr-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn project_dir(sb: &Path) -> PathBuf {
    let dir = sb.join("project");
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// An omp session file under the sandbox whose first record names `cwd`.
fn omp_session(roots: &Roots, cwd: &Path) -> PathBuf {
    let dir = roots.locations.sessions_dir("omp").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"));
    let line = json!({"type": "session", "version": 3, "id": SID, "cwd": cwd, "title": "Old"});
    fs::write(&path, format!("{line}\n")).unwrap();
    path
}

fn roots(sb: &Path) -> Roots {
    Roots::under(sb, sb.join("no-sesync-conv"))
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn plan_for(roots: &Roots, fmt: &str, key: &str, title: &str) -> Result<herdr_open::Plan, String> {
    herdr_open::plan(&roots.locations, &roots.home, fmt, key, title)
}

// ----- the resume command per harness -------------------------------------------------------------

#[test]
fn omp_resumes_by_its_session_file_in_the_project_directory() {
    let sb = sandbox("omp");
    let roots = roots(&sb);
    let cwd = project_dir(&sb);
    let path = omp_session(&roots, &cwd);
    let plan = plan_for(&roots, "omp", path.to_str().unwrap(), "Old").unwrap();
    assert_eq!(plan.argv, words(&["omp", "--resume", path.to_str().unwrap()]));
    assert_eq!(plan.cwd, cwd);
    assert!(!plan.cwd_fallback);
    assert_eq!(plan.label, "omp: Old");
}

#[test]
fn a_path_with_spaces_and_quotes_reaches_the_shell_as_one_argument() {
    let sb = sandbox("with space");
    let roots = roots(&sb);
    let cwd = project_dir(&sb);
    let path = omp_session(&roots, &cwd);
    let plan = plan_for(&roots, "omp", path.to_str().unwrap(), "Old").unwrap();
    let line = herdr_open::shell_line(&plan.argv).unwrap();
    assert_eq!(line, format!("omp --resume '{}'", path.display()));
    assert!(path.to_str().unwrap().contains("with space"), "the sandbox path has a space");

    assert_eq!(herdr_open::shell_quote("it's here"), r"'it'\''s here'");
    assert_eq!(herdr_open::shell_quote("plain-name_1.jsonl"), "plain-name_1.jsonl");
    assert_eq!(herdr_open::shell_quote(""), "''");
}

#[test]
fn an_argument_with_a_line_break_is_refused_before_anything_is_typed() {
    let argv = words(&["omp", "--resume", "/tmp/a\nb"]);
    let err = herdr_open::shell_line(&argv).unwrap_err();
    assert!(err.contains("control character"), "{err}");
}

#[test]
fn claude_resumes_by_the_session_id_from_the_directory_it_ran_in() {
    let sb = sandbox("claude");
    let roots = roots(&sb);
    let cwd = project_dir(&sb);
    let dir = roots.locations.sessions_dir("claude").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{CLAUDE_ID}.jsonl"));
    fs::write(&path, format!("{}\n", json!({"type": "user", "cwd": cwd, "sessionId": CLAUDE_ID}))).unwrap();
    let plan = plan_for(&roots, "claude", path.to_str().unwrap(), "Chat").unwrap();
    assert_eq!(plan.argv, words(&["claude", "--resume", CLAUDE_ID]));
    assert_eq!(plan.cwd, cwd);
    assert!(!plan.cwd_fallback);
}

#[test]
fn claude_file_not_named_by_its_id_is_refused() {
    let sb = sandbox("claude-bad");
    let roots = roots(&sb);
    let err = plan_for(&roots, "claude", "/tmp/not-a-uuid.jsonl", "x").unwrap_err();
    assert!(err.contains("not a session file named by its id"), "{err}");
}

#[test]
fn opencode_resumes_by_session_id_never_forking_and_starts_in_the_recorded_directory() {
    let sb = sandbox("opencode");
    let roots = roots(&sb);
    let cwd = project_dir(&sb);
    let db = roots.locations.of("opencode").root.clone();
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let con = Connection::open(&db).unwrap();
    con.execute_batch("create table session (id text primary key, directory text);").unwrap();
    con.execute("insert into session (id, directory) values ('ses_abc', ?1)", [cwd.to_str().unwrap()]).unwrap();
    drop(con);

    let plan = plan_for(&roots, "opencode", "ses_abc", "Talk").unwrap();
    assert_eq!(plan.argv, words(&["opencode", "--session", "ses_abc"]));
    assert!(!plan.argv.iter().any(|a| a.contains("fork")), "never a fork");
    assert_eq!(plan.cwd, cwd);
    assert!(!plan.cwd_fallback);
}

#[test]
fn opencode_without_a_recorded_directory_starts_in_home_and_says_so() {
    let sb = sandbox("opencode-home");
    let roots = roots(&sb);
    let plan = plan_for(&roots, "opencode", "ses_none", "Talk").unwrap();
    assert_eq!(plan.cwd, sb, "home is the sandbox root here");
    assert!(plan.cwd_fallback);
    let opened = Opened::Created { tab_id: "w9:t9".into() };
    assert!(opened.message(&plan).contains("unknown or gone"), "{}", opened.message(&plan));
}

#[test]
fn a_recorded_directory_that_is_gone_falls_back_to_home() {
    let sb = sandbox("gone-dir");
    let roots = roots(&sb);
    let path = omp_session(&roots, &sb.join("deleted-project"));
    let plan = plan_for(&roots, "omp", path.to_str().unwrap(), "Old").unwrap();
    assert_eq!(plan.cwd, sb);
    assert!(plan.cwd_fallback);
}

#[test]
fn pi_resumes_by_its_session_file() {
    let sb = sandbox("pi");
    let roots = roots(&sb);
    let path = roots.locations.sessions_dir("pi").join("-tmp").join("2026-10-01T10-00-00-000Z_x.jsonl");
    let plan = plan_for(&roots, "pi", path.to_str().unwrap(), "Old").unwrap();
    assert_eq!(plan.argv, words(&["pi", "--session", path.to_str().unwrap()]));
}

#[test]
fn antigravity_and_unknown_harnesses_are_refused_with_a_reason() {
    let sb = sandbox("unsupported");
    let roots = roots(&sb);
    let err = plan_for(&roots, "antigravity", "/tmp/conv.db", "Agy").unwrap_err();
    assert!(err.contains("not supported"), "{err}");
    let err = plan_for(&roots, "gemini", "/tmp/x", "Gem").unwrap_err();
    assert!(err.contains("does not know the harness 'gemini'"), "{err}");
}

#[test]
fn tab_label_is_one_short_line() {
    assert_eq!(herdr_open::tab_label("omp", "Fix\n  the\tbug"), "omp: Fix the bug");
    assert_eq!(herdr_open::tab_label("omp", "   "), "omp session");
    let long = herdr_open::tab_label("claude", &"x".repeat(200));
    assert_eq!(long.chars().count(), 40);
    assert!(long.ends_with('…'));
}

// ----- which pane is the session ------------------------------------------------------------------

fn pane(pane_id: &str, tab_id: &str, argvs: &[&[&str]]) -> PaneProcs {
    PaneProcs {
        pane_id: pane_id.into(),
        tab_id: tab_id.into(),
        argvs: argvs.iter().map(|a| words(a)).collect(),
    }
}

#[test]
fn a_pane_whose_process_was_given_the_session_is_the_match() {
    let path = "/home/u/.omp/agent/sessions/-tmp/x.jsonl";
    let panes = [pane("w2:p1", "w2:t1", &[&["/bin/bash"]]), pane("w2:p4", "w2:t4", &[&["omp", "--resume", path]])];
    let hit = herdr_open::find_open_pane(&panes, path).expect("the omp pane matches");
    assert_eq!(hit.tab_id, "w2:t4");
}

#[test]
fn claude_and_opencode_match_by_their_id_flag() {
    let claude = [pane("w2:p2", "w2:t2", &[&["claude", "--resume", CLAUDE_ID]])];
    assert_eq!(herdr_open::find_open_pane(&claude, CLAUDE_ID).map(|p| p.tab_id.as_str()), Some("w2:t2"));
    let opencode = [pane("w2:p3", "w2:t3", &[&["opencode", "--session", "ses_abc"]])];
    assert_eq!(herdr_open::find_open_pane(&opencode, "ses_abc").map(|p| p.tab_id.as_str()), Some("w2:t3"));
}

#[test]
fn a_bare_picker_is_not_this_session() {
    let panes = [pane("w2:p4", "w2:t4", &[&["omp", "--resume"]])];
    assert!(herdr_open::find_open_pane(&panes, "/home/u/x.jsonl").is_none());
}

#[test]
fn another_session_is_not_this_session() {
    let panes = [pane("w2:p4", "w2:t4", &[&["omp", "--resume", "/home/u/other.jsonl"]])];
    assert!(herdr_open::find_open_pane(&panes, "/home/u/x.jsonl").is_none());
}

#[test]
fn a_program_that_only_mentions_the_file_is_not_this_session() {
    let panes = [pane("w2:p5", "w2:t5", &[&["less", "/home/u/x.jsonl"]])];
    assert!(herdr_open::find_open_pane(&panes, "/home/u/x.jsonl").is_none());
    assert!(herdr_open::find_open_pane(&panes, "").is_none(), "an empty identifier matches nothing");
}

// ----- focus or create ----------------------------------------------------------------------------

fn omp_plan(sb: &Path) -> (Roots, herdr_open::Plan) {
    let roots = roots(sb);
    let cwd = project_dir(sb);
    let path = omp_session(&roots, &cwd);
    let plan = plan_for(&roots, "omp", path.to_str().unwrap(), "Old").unwrap();
    (roots, plan)
}

#[test]
fn a_session_already_running_is_focused_and_nothing_starts() {
    let sb = sandbox("focus");
    let (_roots, plan) = omp_plan(&sb);
    let running = plan.argv.clone();
    let argv: Vec<&str> = running.iter().map(String::as_str).collect();
    let (herdr, calls) = herdr_with_panes(vec![
        ("w2:p1", "w2:t1", vec![vec!["/bin/bash"]]),
        ("w2:p4", "w2:t4", vec![argv]),
    ]);
    let opened = herdr_open::open(&herdr, &plan).unwrap();
    assert_eq!(opened, Opened::Focused { tab_id: "w2:t4".into() });
    let calls = calls.borrow();
    assert!(calls.contains(&words(&["herdr-bin", "tab", "focus", "w2:t4"])), "{calls:?}");
    assert!(!calls.iter().any(|c| c.contains(&"create".to_string())), "no tab is created: {calls:?}");
    assert!(opened.message(&plan).contains("already open"));
}

#[test]
fn a_session_not_running_gets_a_tab_in_its_directory_that_resumes_it() {
    let sb = sandbox("create");
    let (_roots, plan) = omp_plan(&sb);
    let (herdr, calls) = herdr_with_panes(vec![("w2:p1", "w2:t1", vec![vec!["/bin/bash"]])]);
    let opened = herdr_open::open(&herdr, &plan).unwrap();
    assert_eq!(opened, Opened::Created { tab_id: "w9:t9".into() });
    let calls = calls.borrow();
    let create = words(&[
        "herdr-bin", "tab", "create", "--cwd", plan.cwd.to_str().unwrap(),
        "--label", "omp: Old", "--focus", "--workspace", "w3",
    ]);
    assert!(calls.contains(&create), "{calls:?}");
    let command = format!("omp --resume {}", plan.ident());
    let run = words(&["herdr-bin", "pane", "run", "w9:p9", command.as_str()]);
    assert!(calls.contains(&run), "{calls:?}");
}

#[test]
fn a_pane_whose_processes_cannot_be_read_stops_the_open_before_anything_starts() {
    let sb = sandbox("unreadable");
    let (_roots, plan) = omp_plan(&sb);
    let (herdr, calls) = fake_herdr(|args| {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["pane", "list"] => Ok(json!({"result": {"panes": [{"pane_id": "w2:p1", "tab_id": "w2:t1"}]}}).to_string()),
            _ => Err("pane closed".to_string()),
        }
    });
    let err = herdr_open::open(&herdr, &plan).unwrap_err();
    assert!(err.contains("could not read the processes of pane w2:p1"), "{err}");
    assert!(!calls.borrow().iter().any(|c| c.contains(&"create".to_string())), "fails closed, no second copy");
}

#[test]
fn herdr_not_running_is_said_and_nothing_else_is_tried() {
    let sb = sandbox("no-herdr");
    let (_roots, plan) = omp_plan(&sb);
    let (herdr, calls) = fake_herdr(|_| Err("cannot run herdr-bin: No such file or directory".to_string()));
    let err = herdr_open::open(&herdr, &plan).unwrap_err();
    assert!(err.starts_with("herdr is not reachable"), "{err}");
    assert_eq!(calls.borrow().len(), 1, "only the pane list was asked");
}

#[test]
fn a_tab_is_closed_again_when_its_command_cannot_be_typed() {
    let sb = sandbox("rollback");
    let (_roots, plan) = omp_plan(&sb);
    let (herdr, calls) = fake_herdr(|args| {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["pane", "list"] => Ok(json!({"result": {"panes": []}}).to_string()),
            ["tab", "create", ..] => Ok(json!({"result": {"tab": {"tab_id": "w9:t9"}, "root_pane": {"pane_id": "w9:p9"}}}).to_string()),
            ["pane", "run", ..] => Err("pane gone".to_string()),
            ["tab", "close", ..] => Ok(json!({"result": {}}).to_string()),
            other => Err(format!("unexpected {other:?}")),
        }
    });
    let err = herdr_open::open(&herdr, &plan).unwrap_err();
    assert!(err.contains("could not start the resume command"), "{err}");
    assert!(calls.borrow().contains(&words(&["herdr-bin", "tab", "close", "w9:t9"])));
}

#[test]
fn a_herdr_error_message_is_passed_on_as_herdrs_words() {
    let sb = sandbox("herdr-error");
    let (_roots, plan) = omp_plan(&sb);
    let (herdr, _calls) = fake_herdr(|args| {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["pane", "list"] => Ok(json!({"result": {"panes": []}}).to_string()),
            _ => Ok(json!({"error": {"code": "workspace_not_found", "message": "workspace w3 not found"}}).to_string()),
        }
    });
    let err = herdr_open::open(&herdr, &plan).unwrap_err();
    assert_eq!(err, "workspace w3 not found");
}

// ----- the TUI's menu and key -----------------------------------------------------------------------

fn app_with(sb: &Path, herdr: Herdr) -> App {
    let roots = roots(sb);
    let cwd = project_dir(sb);
    omp_session(&roots, &cwd);
    let mut app = App::new(roots, State::default(), sb.join("locks"), sb.join("no-sesync_lock"));
    app.set_herdr(herdr);
    app
}

fn press(app: &mut App, ev: Ev) {
    app.handle(ev, Instant::now());
}

#[test]
fn the_first_menu_row_opens_the_session_in_herdr_and_leaves_the_tui() {
    let sb = sandbox("menu");
    let (herdr, calls) = herdr_with_panes(vec![]);
    let mut app = app_with(&sb, herdr);
    press(&mut app, Ev::Enter); // the session's menu
    press(&mut app, Ev::Enter); // row 0: Open in herdr
    assert_eq!(app.mode(), Mode::Browse);
    assert!(app.should_quit(), "success closes the popup's TUI");
    assert!(app.status().contains("opened"), "status: {}", app.status());
    assert!(calls.borrow().iter().any(|c| c.get(1..3) == Some(&["tab".to_string(), "create".to_string()][..])));
}

#[test]
fn the_o_key_does_the_same_from_the_list() {
    let sb = sandbox("o-key");
    let (herdr, _calls) = herdr_with_panes(vec![]);
    let mut app = app_with(&sb, herdr);
    press(&mut app, Ev::Char('o'));
    assert!(app.should_quit());
}

#[test]
fn outside_herdr_the_tui_says_so_and_stays_open() {
    let sb = sandbox("o-outside");
    let (herdr, _calls) = fake_herdr(|_| Err("cannot run herdr-bin: No such file or directory".to_string()));
    let mut app = app_with(&sb, herdr);
    press(&mut app, Ev::Char('o'));
    assert!(!app.should_quit());
    assert!(app.status_is_error());
    assert!(app.status().contains("herdr is not reachable"), "status: {}", app.status());
}

#[test]
fn a_run_that_answers_nothing_is_a_success() {
    let sb = sandbox("empty-answer");
    let (_roots, plan) = omp_plan(&sb);
    let (herdr, calls) = fake_herdr(|args| {
        let argv_words: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv_words.as_slice() {
            ["pane", "list"] => Ok(json!({"result": {"panes": []}}).to_string()),
            ["tab", "create", ..] => Ok(json!({"result": {"tab": {"tab_id": "w9:t9"}, "root_pane": {"pane_id": "w9:p9"}}}).to_string()),
            ["pane", "run", ..] => Ok(String::new()),
            other => Err(format!("unexpected {other:?}")),
        }
    });
    assert_eq!(herdr_open::open(&herdr, &plan).unwrap(), Opened::Created { tab_id: "w9:t9".into() });
    assert!(calls.borrow().iter().any(|c| c.get(1..3) == Some(&["pane".to_string(), "run".to_string()][..])));
}
