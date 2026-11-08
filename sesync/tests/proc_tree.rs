//! Whether a program runs below a pane's shell, decided on a process list (pid, parent, name). On
//! Windows herdr reports only the shell, so "Open in herdr" reads this to tell a running session from
//! a tab that is back to its prompt (after a herdr restart). Only the shell and its console hosts are
//! idle: a child shell, git or any other process below it is a running program.

use sesync::proc_tree::{programs_below, runs_a_program};

fn list(entries: &[(u32, u32, &str)]) -> Vec<(u32, u32, String)> {
    entries.iter().map(|(pid, ppid, name)| (*pid, *ppid, name.to_string())).collect()
}

#[test]
fn a_shell_with_only_its_console_host_is_idle() {
    let bash = list(&[(10, 1, "bash.exe"), (13, 10, "conhost.exe")]);
    assert!(!runs_a_program(&bash, 10));
    let pwsh = list(&[(10, 1, "pwsh.exe"), (13, 10, "OpenConsole.exe")]);
    assert!(!runs_a_program(&pwsh, 10));
}

#[test]
fn a_script_the_shell_runs_is_busy() {
    // `bash script.sh`: the pane's shell starts a second bash, which runs the script.
    let procs = list(&[(10, 1, "bash.exe"), (11, 10, "bash.exe"), (13, 10, "conhost.exe")]);
    assert!(runs_a_program(&procs, 10));
}

#[test]
fn a_nested_powershell_is_busy() {
    let nested = list(&[(10, 1, "bash.exe"), (11, 10, "pwsh.exe"), (12, 11, "conhost.exe")]);
    assert!(runs_a_program(&nested, 10));
    let bare = list(&[(10, 1, "pwsh.exe"), (11, 10, "powershell.exe")]);
    assert!(runs_a_program(&bare, 10));
}

#[test]
fn git_waiting_at_a_credential_prompt_is_busy() {
    let helper = list(&[(10, 1, "bash.exe"), (11, 10, "git.exe"), (12, 11, "git-credential-manager.exe")]);
    assert!(runs_a_program(&helper, 10));
    let bare = list(&[(10, 1, "bash.exe"), (11, 10, "git.exe")]);
    assert!(runs_a_program(&bare, 10));
}

#[test]
fn claude_started_through_a_cmd_shim_is_busy_while_the_shim_is_the_only_child() {
    // claude.cmd runs through cmd.exe, which can be the only child for a moment.
    let shim = list(&[(10, 1, "bash.exe"), (11, 10, "cmd.exe")]);
    assert!(runs_a_program(&shim, 10));
    let claude = list(&[(10, 1, "bash.exe"), (11, 10, "cmd.exe"), (12, 11, "claude.exe")]);
    assert!(runs_a_program(&claude, 10));
}

#[test]
fn a_git_bash_launcher_counts_its_real_shell_as_busy() {
    // Git Bash's launcher starts the real bash as a child. That child is a shell, so the pane counts as
    // busy and sesync focuses the tab rather than typing into it. The cost: an idle Git Bash tab is
    // focused, not resumed.
    let procs = list(&[(10, 1, "bash.exe"), (11, 10, "bash.exe"), (12, 11, "git.exe"), (13, 10, "conhost.exe")]);
    assert!(runs_a_program(&procs, 10));
}

#[test]
fn an_agent_below_the_shell_is_a_program() {
    let procs = list(&[(10, 1, "bash.exe"), (11, 10, "bash.exe"), (20, 11, "node.exe"), (21, 20, "node.exe")]);
    assert!(runs_a_program(&procs, 10));
    let claude = list(&[(10, 1, "bash.exe"), (11, 10, "bash.exe"), (30, 11, "claude.exe")]);
    assert!(runs_a_program(&claude, 10));
}

#[test]
fn only_processes_below_this_shell_count() {
    // Another pane's agent, and a parent above this shell, are not this pane's.
    let procs = list(&[(5, 1, "node.exe"), (10, 5, "bash.exe"), (11, 10, "conhost.exe"), (40, 2, "omp.exe")]);
    assert!(!runs_a_program(&procs, 10));
}

#[test]
fn names_are_compared_without_case_and_a_pid_loop_ends() {
    // A console host in any case is idle, and a parent loop in the list does not hang the walk.
    let procs = list(&[(10, 11, "Bash.EXE"), (11, 10, "CONHOST.exe")]);
    assert!(!runs_a_program(&procs, 10));
}

#[test]
fn a_helper_seen_in_one_snapshot_only_is_not_a_running_program() {
    // The prompt's `uname` in the first snapshot, gone in the second: the shell is idle. The agent
    // with the same pid in both lasts.
    let first = list(&[(10, 1, "bash.exe"), (50, 10, "uname.exe")]);
    let second = list(&[(10, 1, "bash.exe")]);
    assert!(programs_below(&first, 10).is_disjoint(&programs_below(&second, 10)));
    let busy = list(&[(10, 1, "bash.exe"), (60, 10, "node.exe")]);
    assert!(!programs_below(&busy, 10).is_disjoint(&programs_below(&busy, 10)));
}
