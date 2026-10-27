//! Whether a program runs below a pane's shell, decided on a process list (pid, parent, name). On
//! Windows herdr reports only the shell, so "Open in herdr" reads this to tell a running session
//! from a tab that is back to its prompt (after a herdr restart).

use sesync::proc_tree::runs_a_program;

fn list(entries: &[(u32, u32, &str)]) -> Vec<(u32, u32, String)> {
    entries.iter().map(|(pid, ppid, name)| (*pid, *ppid, name.to_string())).collect()
}

#[test]
fn git_bash_at_its_prompt_runs_nothing() {
    // Git Bash's launcher starts the real bash; the prompt runs git for the branch name.
    let procs = list(&[(10, 1, "bash.exe"), (11, 10, "bash.exe"), (12, 11, "git.exe"), (13, 10, "conhost.exe")]);
    assert!(!runs_a_program(&procs, 10));
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
    let procs = list(&[(5, 1, "node.exe"), (10, 5, "bash.exe"), (11, 10, "bash.exe"), (40, 2, "omp.exe")]);
    assert!(!runs_a_program(&procs, 10));
}

#[test]
fn names_are_compared_without_case_and_a_pid_loop_ends() {
    let procs = list(&[(10, 11, "Bash.EXE"), (11, 10, "BASH.exe")]);
    assert!(!runs_a_program(&procs, 10));
}

#[test]
fn a_helper_seen_in_one_snapshot_only_is_not_a_running_program() {
    use sesync::proc_tree::programs_below;
    // The prompt's `uname` in the first snapshot, gone in the second: the shell is idle. The agent
    // with the same pid in both lasts.
    let first = list(&[(10, 1, "bash.exe"), (11, 10, "bash.exe"), (50, 11, "uname.exe")]);
    let second = list(&[(10, 1, "bash.exe"), (11, 10, "bash.exe")]);
    assert!(programs_below(&first, 10).is_disjoint(&programs_below(&second, 10)));
    let busy = list(&[(10, 1, "bash.exe"), (11, 10, "bash.exe"), (60, 11, "node.exe")]);
    assert!(!programs_below(&busy, 10).is_disjoint(&programs_below(&busy, 10)));
}
