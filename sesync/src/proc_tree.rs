//! Whether a program runs under a shell, read from the operating system's process list. herdr on
//! Windows reports only a pane's shell (`pane process-info` gives the shell and its pid, never the
//! program inside), so "Open in herdr" asks Windows itself whether that shell is idle.

/// Processes that do not count as a program running in a pane: the console hosts Windows attaches to
/// a console. Every other process below the pane's shell counts, a child shell and git included: a
/// `bash script.sh` or a nested pwsh is a running program, and so is git at a credential prompt.
const CONSOLE_HOSTS: [&str; 2] = ["conhost.exe", "openconsole.exe"];

/// How long a program must last to count: a prompt starts short-lived tools while it draws (git for
/// the branch, MSYS helpers), which one snapshot can catch, so a program must be in both snapshots.
const LASTS: std::time::Duration = std::time::Duration::from_millis(250);

/// Whether a program other than a console host runs below `pid` and is still there [`LASTS`] later.
/// None when the process list cannot be read, or off Windows (not needed there).
pub fn program_running_under(pid: u32) -> Option<bool> {
    let first = programs_below(&processes()?, pid);
    if first.is_empty() {
        return Some(false);
    }
    std::thread::sleep(LASTS);
    let second = programs_below(&processes()?, pid);
    Some(!first.is_disjoint(&second))
}

/// Whether one process list shows a program below `root` (see [`programs_below`]).
pub fn runs_a_program(procs: &[(u32, u32, String)], root: u32) -> bool {
    !programs_below(procs, root).is_empty()
}

/// The pids below `root` in a process list of (pid, parent pid, executable name), other than console
/// hosts.
pub fn programs_below(procs: &[(u32, u32, String)], root: u32) -> std::collections::HashSet<u32> {
    let mut found = std::collections::HashSet::new();
    let mut frontier = vec![root];
    let mut seen = std::collections::HashSet::from([root]);
    while let Some(parent) = frontier.pop() {
        for (pid, ppid, name) in procs {
            if *ppid != parent || !seen.insert(*pid) {
                continue;
            }
            if !CONSOLE_HOSTS.contains(&name.to_ascii_lowercase().as_str()) {
                found.insert(*pid);
            }
            frontier.push(*pid);
        }
    }
    found
}

#[cfg(windows)]
fn processes() -> Option<Vec<(u32, u32, String)>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    // SAFETY: a snapshot handle is closed on every path; the entry is a plain struct sized as the
    // API requires.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut out = Vec::new();
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snap, &mut entry) != 0;
        while more {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            out.push((entry.th32ProcessID, entry.th32ParentProcessID, String::from_utf16_lossy(&entry.szExeFile[..len])));
            more = Process32NextW(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
        Some(out)
    }
}

#[cfg(not(windows))]
fn processes() -> Option<Vec<(u32, u32, String)>> {
    None
}
