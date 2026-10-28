//! Opening a session in herdr, with a fake herdr: the resume command per harness, the match on a
//! running pane, the focus-or-create decision, and the menu wiring in the TUI. No herdr is started
//! and no real session store is read: every path is under a sandbox directory.
use rusqlite::Connection;
use sesync::app::{App, Ev, Mode};
use sesync::herdr_open::{self, Detect, Herdr, Opened, PaneProcs, Runner};
use sesync::manage::Roots;
use sesync::state::State;
use serde_json::json;
use parking_lot::Mutex;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

const SID: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const CLAUDE_ID: &str = "bbbbbbbb-1111-2222-3333-444444444444";

type Calls = Arc<Mutex<Vec<Vec<String>>>>;

/// A runner that answers from a script and records every call as `program args...`.
struct Fake {
    calls: Calls,
    answer: Arc<dyn Fn(&[String]) -> Result<String, String> + Send + Sync>,
}

impl Runner for Fake {
    fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
        let mut line = vec![program.to_string()];
        line.extend(args.iter().cloned());
        self.calls.lock().push(line);
        (self.answer)(args)
    }
}

fn fake_herdr(answer: impl Fn(&[String]) -> Result<String, String> + Send + Sync + 'static) -> (Herdr, Calls) {
    let calls = Calls::default();
    let runner = Fake { calls: calls.clone(), answer: Arc::new(answer) };
    (Herdr::new("herdr-bin", Some("w3".to_string()), Box::new(runner)), calls)
}

/// A herdr with these panes: (pane id, tab id, directory, foreground argument lists), and these
/// tabs: (tab id, label). A new tab is always `w9:t9` with root pane `w9:p9`. Every call succeeds
/// except a process-info for an unknown pane.
fn herdr_with(panes: Vec<(&str, &str, &str, Vec<Vec<&str>>)>, tabs: Vec<(&str, &str)>) -> (Herdr, Calls) {
    let panes: Vec<(String, String, String, Vec<Vec<String>>)> = panes
        .into_iter()
        .map(|(pane, tab, cwd, argvs)| (pane.to_string(), tab.to_string(), cwd.to_string(), argvs.iter().map(|a| words(a)).collect()))
        .collect();
    let tabs: Vec<_> = tabs.into_iter().map(|(tab, label)| json!({"tab_id": tab, "label": label})).collect();
    fake_herdr(move |args| {
        let argv_words: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv_words.as_slice() {
            ["pane", "list"] => {
                let listed: Vec<_> = panes.iter().map(|(pane, tab, cwd, _)| json!({"pane_id": pane, "tab_id": tab, "cwd": cwd})).collect();
                Ok(json!({"id": "cli:pane:list", "result": {"panes": listed}}).to_string())
            }
            ["pane", "process-info", "--pane", pane] => {
                let (_, _, _, argvs) = panes.iter().find(|(p, _, _, _)| p.as_str() == *pane).ok_or("pane not found")?;
                let procs: Vec<_> = argvs.iter().map(|argv| json!({"argv": argv, "cmdline": argv.join(" ")})).collect();
                Ok(json!({"result": {"process_info": {"foreground_processes": procs, "shell_pid": 4242}}}).to_string())
            }
            ["tab", "list"] => Ok(json!({"result": {"tabs": tabs}}).to_string()),
            ["tab", "create", ..] => Ok(json!({"result": {"tab": {"tab_id": "w9:t9"}, "root_pane": {"pane_id": "w9:p9"}}}).to_string()),
            ["pane", "run", ..] | ["tab", "focus", ..] | ["tab", "close", ..] => Ok(json!({"result": {}}).to_string()),
            other => Err(format!("unexpected herdr call {other:?}")),
        }
    })
}

/// Panes with their foreground argument lists, all in `/elsewhere`, and no labelled tabs.
fn herdr_with_panes(panes: Vec<(&str, &str, Vec<Vec<&str>>)>) -> (Herdr, Calls) {
    herdr_with(panes.into_iter().map(|(pane, tab, argvs)| (pane, tab, "/elsewhere", argvs)).collect(), vec![])
}

/// The herdr calls made, without the binary, as words: `["tab", "list"]`, ...
fn verbs(calls: &Calls) -> Vec<Vec<String>> {
    calls.lock().iter().map(|c| c[1..].to_vec()).collect()
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
    assert_eq!(plan.label, format!("omp: Old · {}", plan.tag));
}

#[test]
fn a_path_with_spaces_and_quotes_reaches_the_shell_as_one_argument() {
    let sb = sandbox("with space");
    let roots = roots(&sb);
    let cwd = project_dir(&sb);
    let path = omp_session(&roots, &cwd);
    let plan = plan_for(&roots, "omp", path.to_str().unwrap(), "Old").unwrap();
    let line = herdr_open::shell_line(&plan.argv, herdr_open::PaneShell::Posix).unwrap();
    assert_eq!(line, format!("omp --resume '{}'", path.display()));
    assert!(path.to_str().unwrap().contains("with space"), "the sandbox path has a space");

    assert_eq!(herdr_open::shell_quote("it's here"), r"'it'\''s here'");
    assert_eq!(herdr_open::shell_quote("plain-name_1.jsonl"), "plain-name_1.jsonl");
    assert_eq!(herdr_open::shell_quote(""), "''");
}

#[test]
fn an_argument_with_a_line_break_is_refused_before_anything_is_typed() {
    let argv = words(&["omp", "--resume", "/tmp/a\nb"]);
    let err = herdr_open::shell_line(&argv, herdr_open::PaneShell::Posix).unwrap_err();
    assert!(err.contains("control character"), "{err}");
}

#[test]
fn powershell_quotes_each_argument_and_calls_the_program_with_an_ampersand() {
    let argv = words(&["C:\\Program Files\\omp\\omp.exe", "--resume", "C:\\Users\\y0c0ms\\it's [5]\\x & y%z.jsonl"]);
    let line = herdr_open::shell_line(&argv, herdr_open::PaneShell::PowerShell).unwrap();
    assert_eq!(line, "& 'C:\\Program Files\\omp\\omp.exe' '--resume' 'C:\\Users\\y0c0ms\\it''s [5]\\x & y%z.jsonl'");
    assert_eq!(herdr_open::powershell_quote("é 日本"), "'é 日本'");
    assert_eq!(herdr_open::powershell_quote("a\u{2019}b"), "'a\u{2019}\u{2019}b'", "typographic quotes are doubled");
    assert_eq!(herdr_open::powershell_quote("$(x) `y` \"z\""), "'$(x) `y` \"z\"'", "PowerShell reads nothing else inside single quotes");
}

#[test]
fn cmd_quotes_spaces_and_escapes_operators_and_percent() {
    assert_eq!(herdr_open::cmd_quote("C:\\omp\\omp.exe").unwrap(), "C:\\omp\\omp.exe");
    assert_eq!(herdr_open::cmd_quote("C:\\a b\\c.jsonl").unwrap(), "\"C:\\a b\\c.jsonl\"");
    assert_eq!(herdr_open::cmd_quote("a&b%c").unwrap(), "\"a\"^&\"b\"^%\"c\"");
    assert_eq!(herdr_open::cmd_quote("x \\").unwrap(), "\"x \\\\\"", "trailing backslash doubled");
    assert!(herdr_open::cmd_quote("say \"hi\"").unwrap_err().contains("double quote"));
    let argv = words(&["C:\\Program Files\\omp.exe", "--resume", "C:\\a b\\x.jsonl"]);
    assert_eq!(
        herdr_open::shell_line(&argv, herdr_open::PaneShell::Cmd).unwrap(),
        "\"C:\\Program Files\\omp.exe\" --resume \"C:\\a b\\x.jsonl\""
    );
}

#[test]
fn the_pane_shell_comes_from_the_terminal_table_then_the_environment_then_the_platform() {
    let config = "[ui]\ndefault_shell = \"C:\\\\nope.exe\"\n[terminal]\n# default_shell = \"nope\"\ndefault_shell = \"C:\\\\Program Files\\\\PowerShell\\\\7\\\\pwsh.exe\" # the shell\n";
    assert_eq!(herdr_open::configured_shell(config).as_deref(), Some("C:\\Program Files\\PowerShell\\7\\pwsh.exe"));
    assert_eq!(herdr_open::configured_shell("[terminal]\nshell_mode = \"auto\"\n"), None);
    assert_eq!(herdr_open::configured_shell("[terminal]\ndefault_shell = \"\"\n"), None);
    assert_eq!(herdr_open::PaneShell::from_exe("C:\\Windows\\System32\\cmd.exe"), herdr_open::PaneShell::Cmd);
    assert_eq!(herdr_open::PaneShell::from_exe("/usr/bin/fish"), herdr_open::PaneShell::Posix);
    assert_eq!(herdr_open::PaneShell::resolve(None, Some("/bin/zsh"), false), herdr_open::PaneShell::Posix);
    assert_eq!(herdr_open::PaneShell::resolve(None, Some("/bin/zsh"), true), herdr_open::PaneShell::PowerShell, "Windows ignores $SHELL");
    assert_eq!(herdr_open::PaneShell::resolve(Some("cmd"), None, true), herdr_open::PaneShell::Cmd);
    assert_eq!(herdr_open::PaneShell::resolve(None, None, false), herdr_open::PaneShell::Posix);
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
    assert_eq!(herdr_open::tab_label("omp", "Fix\n  the\tbug", ""), "omp: Fix the bug");
    assert_eq!(herdr_open::tab_label("omp", "   ", ""), "omp session");
    let long = herdr_open::tab_label("claude", &"x".repeat(200), "");
    assert_eq!(long.chars().count(), 40);
    assert!(long.ends_with('…'));
}

#[test]
fn the_tag_stays_in_a_long_tab_label_and_is_stable() {
    let tag = herdr_open::session_tag("/home/u/x.jsonl");
    assert_eq!(tag.len(), 8);
    assert_eq!(herdr_open::session_tag("/home/u/x.jsonl"), tag);
    assert_ne!(herdr_open::session_tag("/home/u/y.jsonl"), tag);
    let label = herdr_open::tab_label("omp", &"x".repeat(200), &tag);
    assert_eq!(label.chars().count(), 40);
    assert!(label.ends_with(&format!(" · {tag}")), "{label}");
}

// ----- which pane is the session ------------------------------------------------------------------

fn pane(pane_id: &str, tab_id: &str, argvs: &[&[&str]]) -> PaneProcs {
    PaneProcs {
        pane_id: pane_id.into(),
        tab_id: tab_id.into(),
        tab_label: String::new(),
        argvs: argvs.iter().map(|a| words(a)).collect(),
    }
}

#[test]
fn a_pane_whose_process_was_given_the_session_is_the_match() {
    let path = "/home/u/.omp/agent/sessions/-tmp/x.jsonl";
    let panes = [pane("w2:p1", "w2:t1", &[&["/bin/bash"]]), pane("w2:p4", "w2:t4", &[&["omp", "--resume", path]])];
    let hit = herdr_open::find_open_pane(&panes, path, "").expect("the omp pane matches");
    assert_eq!(hit.tab_id, "w2:t4");
}

#[test]
fn claude_and_opencode_match_by_their_id_flag() {
    let claude = [pane("w2:p2", "w2:t2", &[&["claude", "--resume", CLAUDE_ID]])];
    assert_eq!(herdr_open::find_open_pane(&claude, CLAUDE_ID, "").map(|p| p.tab_id.as_str()), Some("w2:t2"));
    let opencode = [pane("w2:p3", "w2:t3", &[&["opencode", "--session", "ses_abc"]])];
    assert_eq!(herdr_open::find_open_pane(&opencode, "ses_abc", "").map(|p| p.tab_id.as_str()), Some("w2:t3"));
}

#[test]
fn a_bare_picker_is_not_this_session() {
    let panes = [pane("w2:p4", "w2:t4", &[&["omp", "--resume"]])];
    assert!(herdr_open::find_open_pane(&panes, "/home/u/x.jsonl", "").is_none());
}

#[test]
fn another_session_is_not_this_session() {
    let panes = [pane("w2:p4", "w2:t4", &[&["omp", "--resume", "/home/u/other.jsonl"]])];
    assert!(herdr_open::find_open_pane(&panes, "/home/u/x.jsonl", "").is_none());
}

#[test]
fn a_program_that_only_mentions_the_file_is_not_this_session() {
    let panes = [pane("w2:p5", "w2:t5", &[&["less", "/home/u/x.jsonl"]])];
    assert!(herdr_open::find_open_pane(&panes, "/home/u/x.jsonl", "").is_none());
    assert!(herdr_open::find_open_pane(&panes, "", "").is_none(), "an empty identifier matches nothing");
}

#[test]
fn a_tab_labelled_with_the_session_tag_is_the_session_when_no_argv_is_reported() {
    let tag = herdr_open::session_tag("/home/u/x.jsonl");
    let mut tagged = pane("w2:p6", "w2:t6", &[]);
    tagged.tab_label = herdr_open::tab_label("omp", "Old", &tag);
    let panes = [tagged];
    let hit = herdr_open::find_open_pane(&panes, "/home/u/x.jsonl", &tag).expect("the tagged tab matches");
    assert_eq!(hit.tab_id, "w2:t6");
    assert!(herdr_open::find_open_pane(&panes, "/home/u/x.jsonl", "00000000").is_none(), "another tag matches nothing");
}

#[test]
fn argv_matching_comes_first_and_a_tag_never_matches_a_pane_running_something_else() {
    let tag = herdr_open::session_tag("/home/u/x.jsonl");
    let mut busy = pane("w2:p7", "w2:t7", &[&["less", "x"]]);
    busy.tab_label = herdr_open::tab_label("omp", "Old", &tag);
    assert!(herdr_open::find_open_pane(&[busy.clone()], "/home/u/x.jsonl", &tag).is_none());
    let matched = pane("w2:p8", "w2:t8", &[&["omp", "--resume", "/home/u/x.jsonl"]]);
    let panes = [busy, matched];
    assert_eq!(herdr_open::find_open_pane(&panes, "/home/u/x.jsonl", &tag).map(|p| p.tab_id.as_str()), Some("w2:t8"));
}

// ----- focus or create ----------------------------------------------------------------------------

fn omp_plan(sb: &Path) -> (Roots, herdr_open::Plan) {
    let roots = roots(sb);
    let cwd = project_dir(sb);
    let path = omp_session(&roots, &cwd);
    let plan = plan_for(&roots, "omp", path.to_str().unwrap(), "Old").unwrap();
    (roots, plan)
}

/// herdr on Linux and macOS: process info shows the programs running in a pane's shell.
const SHOWS_PROGRAMS: Detect<'static> = Detect::Programs;
fn running(_shell_pid: u32) -> Option<bool> {
    Some(true)
}
fn idle(_shell_pid: u32) -> Option<bool> {
    Some(false)
}
fn unknown(_shell_pid: u32) -> Option<bool> {
    None
}
/// herdr on Windows: process info shows only the pane's shell; the shell runs nothing.
const SHELL_ONLY: Detect<'static> = Detect::ShellOnly(&idle);

#[test]
fn a_session_already_running_is_focused_and_only_panes_in_its_directory_are_asked() {
    let sb = sandbox("focus");
    let (_roots, plan) = omp_plan(&sb);
    let running = plan.argv.clone();
    let argv: Vec<&str> = running.iter().map(String::as_str).collect();
    let project = plan.cwd.to_str().unwrap().to_string();
    let (herdr, calls) = herdr_with(
        vec![("w2:p1", "w2:t1", "/elsewhere", vec![vec!["/bin/bash"]]), ("w2:p4", "w2:t4", project.as_str(), vec![argv])],
        vec![],
    );
    let opened = herdr_open::open(&herdr, &plan, herdr_open::PaneShell::Posix, SHOWS_PROGRAMS).unwrap();
    assert_eq!(opened, Opened::Focused { tab_id: "w2:t4".into() });
    let verbs = verbs(&calls);
    assert!(verbs.contains(&words(&["tab", "focus", "w2:t4"])), "{verbs:?}");
    assert!(!verbs.iter().any(|c| c.contains(&"create".to_string())), "no tab is created: {verbs:?}");
    assert!(!verbs.contains(&words(&["pane", "process-info", "--pane", "w2:p1"])), "a pane elsewhere is never asked: {verbs:?}");
    assert!(opened.message(&plan).contains("already open"));
}

#[test]
fn a_session_not_running_gets_a_tab_that_is_focused_only_once_its_command_is_typed() {
    let sb = sandbox("create");
    let (_roots, plan) = omp_plan(&sb);
    let (herdr, calls) = herdr_with_panes(vec![("w2:p1", "w2:t1", vec![vec!["/bin/bash"]])]);
    let opened = herdr_open::open(&herdr, &plan, herdr_open::PaneShell::Posix, SHOWS_PROGRAMS).unwrap();
    assert_eq!(opened, Opened::Created { tab_id: "w9:t9".into() });
    let verbs = verbs(&calls);
    let create = words(&["tab", "create", "--cwd", plan.cwd.to_str().unwrap(), "--label", plan.label.as_str(), "--no-focus", "--workspace", "w3"]);
    let command = format!("omp --resume {}", plan.ident());
    let run = words(&["pane", "run", "w9:p9", command.as_str()]);
    let focus = words(&["tab", "focus", "w9:t9"]);
    let at = |call: &Vec<String>| verbs.iter().position(|c| c == call).unwrap_or_else(|| panic!("{call:?} missing: {verbs:?}"));
    assert!(at(&create) < at(&run) && at(&run) < at(&focus), "created unfocused, typed, then focused: {verbs:?}");
    assert!(!verbs.iter().any(|c| c.get(..2) == Some(&words(&["pane", "process-info"])[..])), "no pane is in the session's directory: {verbs:?}");
}

#[test]
fn where_herdr_shows_only_the_shell_the_tagged_tab_running_a_program_is_focused() {
    let sb = sandbox("tagged");
    let (_roots, plan) = omp_plan(&sb);
    let label = herdr_open::tab_label("omp", "Renamed since", &plan.tag);
    for (seen, what) in [(running as fn(u32) -> Option<bool>, "running"), (unknown, "unknown")] {
        let (herdr, calls) = herdr_with(
            vec![("w2:p1", "w2:t1", "/elsewhere", vec![vec!["bash.exe"]]), ("w2:p6", "w2:t6", "/elsewhere", vec![vec!["bash.exe"]])],
            vec![("w2:t1", "omp: another · 00000000"), ("w2:t6", label.as_str())],
        );
        let opened = herdr_open::open(&herdr, &plan, herdr_open::PaneShell::Posix, Detect::ShellOnly(&seen)).unwrap();
        assert_eq!(opened, Opened::Focused { tab_id: "w2:t6".into() }, "{what}");
        assert_eq!(
            verbs(&calls),
            vec![words(&["tab", "list"]), words(&["pane", "list"]), words(&["pane", "process-info", "--pane", "w2:p6"]), words(&["tab", "focus", "w2:t6"])],
            "only the tagged tab's pane is asked about ({what}; unknown fails closed)"
        );
    }
}

#[test]
fn where_herdr_shows_only_the_shell_an_idle_tagged_tab_gets_the_command_again() {
    let sb = sandbox("tagged-idle");
    let (_roots, plan) = omp_plan(&sb);
    let label = herdr_open::tab_label("omp", "Old", &plan.tag);
    let (herdr, calls) = herdr_with(vec![("w2:p6", "w2:t6", "/elsewhere", vec![vec!["bash.exe"]])], vec![("w2:t6", label.as_str())]);
    let opened = herdr_open::open(&herdr, &plan, herdr_open::PaneShell::Posix, SHELL_ONLY).unwrap();
    assert_eq!(opened, Opened::Resumed { tab_id: "w2:t6".into() });
    let command = format!("omp --resume {}", plan.ident());
    assert_eq!(
        verbs(&calls),
        vec![
            words(&["tab", "list"]),
            words(&["pane", "list"]),
            words(&["pane", "process-info", "--pane", "w2:p6"]),
            words(&["pane", "run", "w2:p6", command.as_str()]),
            words(&["tab", "focus", "w2:t6"]),
        ],
        "typed into its own tab, no second tab"
    );
    assert!(opened.message(&plan).starts_with("resumed"));
}

#[test]
fn where_herdr_shows_only_the_shell_a_new_tab_takes_four_calls_whatever_the_number_of_panes() {
    let sb = sandbox("shell-only");
    let (_roots, plan) = omp_plan(&sb);
    let many: Vec<(String, String)> = (0..16).map(|i| (format!("w1:p{i}"), format!("w1:t{i}"))).collect();
    let (herdr, calls) = herdr_with(
        many.iter().map(|(p, t)| (p.as_str(), t.as_str(), plan.cwd.to_str().unwrap(), vec![vec!["bash.exe"]])).collect(),
        vec![],
    );
    let opened = herdr_open::open(&herdr, &plan, herdr_open::PaneShell::Posix, SHELL_ONLY).unwrap();
    assert_eq!(opened, Opened::Created { tab_id: "w9:t9".into() });
    let firsts: Vec<String> = verbs(&calls).iter().map(|c| c[..2].join(" ")).collect();
    assert_eq!(firsts, vec!["tab list", "tab create", "pane run", "tab focus"]);
}

#[test]
fn a_pane_in_the_sessions_directory_whose_processes_cannot_be_read_stops_the_open() {
    let sb = sandbox("unreadable");
    let (_roots, plan) = omp_plan(&sb);
    let project = plan.cwd.to_str().unwrap().to_string();
    let (herdr, calls) = fake_herdr(move |args| {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["pane", "list"] => Ok(json!({"result": {"panes": [{"pane_id": "w2:p1", "tab_id": "w2:t1", "cwd": project}]}}).to_string()),
            ["tab", "list"] => Ok(json!({"result": {"tabs": []}}).to_string()),
            _ => Err("pane closed".to_string()),
        }
    });
    let err = herdr_open::open(&herdr, &plan, herdr_open::PaneShell::Posix, SHOWS_PROGRAMS).unwrap_err();
    assert!(err.contains("could not read the processes of pane w2:p1"), "{err}");
    assert!(!calls.lock().iter().any(|c| c.contains(&"create".to_string())), "fails closed, no second copy");
}

#[test]
fn herdr_not_running_is_said_and_nothing_else_is_tried() {
    let sb = sandbox("no-herdr");
    let (_roots, plan) = omp_plan(&sb);
    for shows_programs in [SHOWS_PROGRAMS, SHELL_ONLY] {
        let (herdr, calls) = fake_herdr(|_| Err("cannot run herdr-bin: No such file or directory".to_string()));
        let err = herdr_open::open(&herdr, &plan, herdr_open::PaneShell::Posix, shows_programs).unwrap_err();
        assert!(err.starts_with("herdr is not reachable"), "{err}");
        assert_eq!(verbs(&calls), vec![words(&["tab", "list"])], "only the tab list was asked");
    }
}

#[test]
fn a_tab_is_closed_again_when_its_command_cannot_be_typed() {
    let sb = sandbox("rollback");
    let (_roots, plan) = omp_plan(&sb);
    let (herdr, calls) = fake_herdr(|args| {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["pane", "list"] => Ok(json!({"result": {"panes": []}}).to_string()),
            ["tab", "list"] => Ok(json!({"result": {"tabs": []}}).to_string()),
            ["tab", "create", ..] => Ok(json!({"result": {"tab": {"tab_id": "w9:t9"}, "root_pane": {"pane_id": "w9:p9"}}}).to_string()),
            ["pane", "run", ..] => Err("pane gone".to_string()),
            ["tab", "close", ..] => Ok(json!({"result": {}}).to_string()),
            other => Err(format!("unexpected {other:?}")),
        }
    });
    let err = herdr_open::open(&herdr, &plan, herdr_open::PaneShell::Posix, SHOWS_PROGRAMS).unwrap_err();
    assert!(err.contains("could not start the resume command"), "{err}");
    let verbs = verbs(&calls);
    assert!(verbs.contains(&words(&["tab", "close", "w9:t9"])));
    assert!(!verbs.contains(&words(&["tab", "focus", "w9:t9"])), "a tab that is closed again is never shown");
}

#[test]
fn a_herdr_error_message_is_passed_on_as_herdrs_words() {
    let sb = sandbox("herdr-error");
    let (_roots, plan) = omp_plan(&sb);
    let (herdr, _calls) = fake_herdr(|args| {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["pane", "list"] => Ok(json!({"result": {"panes": []}}).to_string()),
            ["tab", "list"] => Ok(json!({"result": {"tabs": []}}).to_string()),
            _ => Ok(json!({"error": {"code": "workspace_not_found", "message": "workspace w3 not found"}}).to_string()),
        }
    });
    let err = herdr_open::open(&herdr, &plan, herdr_open::PaneShell::Posix, SHOWS_PROGRAMS).unwrap_err();
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

/// A key, then what the TUI's loop does next: draw, and run an "Open in herdr" the key asked for.
fn press(app: &mut App, ev: Ev) {
    app.handle(ev, Instant::now());
    app.run_pending_open();
}

#[test]
fn the_open_says_it_started_before_any_herdr_call_is_made() {
    let sb = sandbox("feedback");
    let (herdr, calls) = herdr_with_panes(vec![]);
    let mut app = app_with(&sb, herdr);
    app.handle(Ev::Char('o'), Instant::now());
    assert!(app.has_pending_open());
    assert!(app.status().starts_with("opening '"), "the frame drawn now says so: {}", app.status());
    assert!(calls.lock().is_empty(), "nothing is asked of herdr before that frame");
    app.run_pending_open();
    assert!(!app.has_pending_open());
    assert!(app.should_quit());
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
    assert!(calls.lock().iter().any(|c| c.get(1..3) == Some(&["tab".to_string(), "create".to_string()][..])));
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
            ["tab", "list"] => Ok(json!({"result": {"tabs": []}}).to_string()),
            ["tab", "create", ..] => Ok(json!({"result": {"tab": {"tab_id": "w9:t9"}, "root_pane": {"pane_id": "w9:p9"}}}).to_string()),
            ["pane", "run", ..] | ["tab", "focus", ..] => Ok(String::new()),
            other => Err(format!("unexpected {other:?}")),
        }
    });
    assert_eq!(herdr_open::open(&herdr, &plan, herdr_open::PaneShell::Posix, SHOWS_PROGRAMS).unwrap(), Opened::Created { tab_id: "w9:t9".into() });
    assert!(calls.lock().iter().any(|c| c.get(1..3) == Some(&["pane".to_string(), "run".to_string()][..])));
}
