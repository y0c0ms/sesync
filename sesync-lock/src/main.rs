//! sesync_lock: one harness writes a conversation at a time.
//!
//! State is a directory of lock files, one per conversation:
//!   <LOCK_DIR>/<conversation-id>.lock
//! containing a single line:
//!   <harness> <pid> <unix-seconds> <path-or-id of the file being written>
//!
//! Design points (each one decided against a measured failure):
//! * Locks live on disk, not behind a daemon, so locking keeps working when
//!   the sync daemon is down (its only job then is re-mirroring, which can
//!   wait). The hooks call this binary; nothing else is in the hot path.
//! * A lock is held only while a turn is in flight: claim on prompt submit,
//!   release on turn end. Between turns nothing is held, so an idle window
//!   never blocks another harness and there is no "when is it done" guess.
//! * Stale locks are detected by pid liveness. A harness killed mid-turn
//!   leaves a lock whose pid is dead; `check` treats that as free and `sweep`
//!   removes it. No timeouts, no clocks to disagree about.
//! * Creation is atomic (create_new). Two harnesses racing for the same
//!   conversation cannot both win.
//! * The conversation id is the SOURCE session id. Mirrors in other harnesses
//!   map back to it through the daemon's state; the hooks pass whatever id
//!   their harness has and the daemon resolves it before calling us (or the
//!   hook calls `resolve` first).
//!
//! Exit codes: 0 = ok / free, 2 = locked by someone else, 1 = error.
//! `check` and `claim` print the holder as `harness pid` on exit 2 so a hook
//! can show it without parsing a file.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

fn lock_dir() -> PathBuf {
    if let Ok(d) = env::var("SESYNC_LOCK_DIR") {
        return PathBuf::from(d);
    }
    let home = home_dir().unwrap_or_else(|| ".".into());
    // Stored state directory: existing lock files live here, so it keeps its value until a migration moves them.
    Path::new(&home).join(".local").join("share").join("harness-sync").join("locks")
}

/// The home directory, chosen exactly as `sesync::home::home_dir` chooses it: `HOME` when usable,
/// else `USERPROFILE` on Windows. The two must agree, or a hook's claim and the daemon's sweep
/// would look in different lock directories.
fn home_dir() -> Option<String> {
    home_from(&|k| env::var(k).ok(), cfg!(windows))
}

fn home_from(var: &dyn Fn(&str) -> Option<String>, windows: bool) -> Option<String> {
    let usable = |key: &str| var(key).filter(|v| !v.is_empty() && (!windows || windows_absolute(v)));
    usable("HOME").or_else(|| if windows { usable("USERPROFILE") } else { None })
}

/// An absolute drive (`C:\`, `C:/`) or UNC (`\\server\share`) path.
fn windows_absolute(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        || s.starts_with("\\\\")
        || s.starts_with("//")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Debug, Clone)]
struct Lock {
    harness: String,
    pid: u32,
    since: u64,
    target: String,
}

fn parse(s: &str) -> Option<Lock> {
    let mut it = s.trim().splitn(4, ' ');
    let harness = it.next()?.to_string();
    let pid = it.next()?.parse().ok()?;
    let since = it.next()?.parse().ok()?;
    let target = it.next().unwrap_or("").to_string();
    Some(Lock { harness, pid, since, target })
}

fn read_lock(p: &Path) -> Option<Lock> {
    fs::read_to_string(p).ok().and_then(|s| parse(&s))
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    // OpenProcess with PROCESS_QUERY_LIMITED_INFORMATION; a dead pid fails.
    // Pids are recycled, so a live pid of an unrelated process would look
    // held. Acceptable: the window is the lifetime of one turn, and the
    // false positive resolves itself on the next `sweep`/`claim --force`.
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
        fn CloseHandle(h: *mut core::ffi::c_void) -> i32;
        fn GetExitCodeProcess(h: *mut core::ffi::c_void, code: *mut u32) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE
    }
}

/// Unix (Linux and macOS): signal 0 probes existence without delivering a
/// signal. EPERM means the process exists but belongs to another user, so it
/// is alive. ESRCH means it is gone. /proc is not used: macOS has none.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        if kill(pid as i32, 0) == 0 {
            return true;
        }
    }
    io::Error::last_os_error().raw_os_error() == Some(1) // EPERM
}

#[cfg(not(any(windows, unix)))]
fn pid_alive(_pid: u32) -> bool {
    // Unknown platform: assume alive so no lock is silently stolen.
    true
}

fn safe_id(id: &str) -> String {
    // ids are uuids / ses_… tokens; refuse anything that could escape the dir
    id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

/// A hook inside a MIRROR passes the mirror's own id. The daemon writes
/// locks/map/<mirror-id> containing the source id; follow it (one hop is
/// enough, mirrors are never sources) so all copies lock the same name.
fn resolve(id: &str) -> String {
    let safe = safe_id(id);
    let alias = lock_dir().join("map").join(&safe);
    match fs::read_to_string(&alias) {
        Ok(s) if !s.trim().is_empty() => safe_id(s.trim()),
        _ => safe,
    }
}

fn lock_path(id: &str) -> PathBuf {
    lock_dir().join(format!("{}.lock", resolve(id)))
}

/// Returns Some(lock) if held by a LIVE process, None if free or stale.
/// Never deletes: a stale file is reclaimed only through `take_stale`, which
/// is atomic, so two claimers cannot both decide "stale, mine".
fn holder(id: &str) -> Option<Lock> {
    let p = lock_path(id);
    let l = read_lock(&p)?;
    if pid_alive(l.pid) { Some(l) } else { None }
}

/// A lock file that exists but does not parse (empty, truncated, or garbage)
/// has no live owner. It is treated as stale everywhere, so `check` and
/// `claim` agree. Before this, `check` said free while `claim` refused with
/// holder "? ?", and the conversation stayed wedged until a manual `sweep`.
fn lock_state(p: &Path) -> LockState {
    match fs::read_to_string(p) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => LockState::Absent,
        Err(_) => LockState::Corrupt,
        Ok(s) => match parse(&s) {
            Some(l) => LockState::Held(l),
            None => LockState::Corrupt,
        },
    }
}

enum LockState {
    Absent,
    Corrupt,
    Held(Lock),
}

/// Atomically replace a stale lock file. Rename of a freshly created temp
/// file over the stale one is atomic on NTFS and POSIX; whichever claimer's
/// rename lands last owns the file, and we verify ownership by reading it
/// back, so exactly one claimer sees its own line.
fn take_stale(p: &Path, line: &str, harness: &str, pid: u32) -> bool {
    // Unique temp name per call, so two claimers never write the same temp file.
    let tmp = p.with_extension(format!("tmp{pid}-{}", now_nanos()));
    if fs::write(&tmp, line).is_err() {
        return false;
    }
    if fs::rename(&tmp, p).is_err() {
        let _ = fs::remove_file(&tmp);
        return false;
    }
    matches!(read_lock(p), Some(l) if l.harness == harness && l.pid == pid)
}

fn now_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

fn claim(id: &str, harness: &str, pid: u32, target: &str, force: bool) -> i32 {
    let dir = lock_dir();
    if fs::create_dir_all(&dir).is_err() {
        eprintln!("sesync_lock: cannot create {}", dir.display());
        return 1;
    }
    let p = lock_path(id);
    let line = format!("{harness} {pid} {} {target}\n", now());
    match lock_state(&p) {
        LockState::Held(l) if l.harness == harness && l.pid == pid => {
            // re-entrant: next turn in the same window refreshes the stamp
            let _ = fs::write(&p, &line);
            println!("{} {}", l.harness, l.pid);
            return 0;
        }
        LockState::Held(l) if pid_alive(l.pid) && !force => {
            println!("{} {}", l.harness, l.pid);
            return 2;
        }
        LockState::Held(_) | LockState::Corrupt => {
            // stale (dead pid), unparseable, or --force: atomic takeover
            if take_stale(&p, &line, harness, pid) {
                println!("{harness} {pid}");
                return 0;
            }
            match lock_state(&p) {
                LockState::Held(l) => println!("{} {}", l.harness, l.pid),
                _ => println!("? ?"),
            }
            return 2;
        }
        LockState::Absent => {}
    }
    match fs::OpenOptions::new().write(true).create_new(true).open(&p) {
        Ok(mut f) => {
            if f.write_all(line.as_bytes()).is_err() {
                let _ = fs::remove_file(&p);
                return 1;
            }
            println!("{harness} {pid}");
            0
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // lost the race between holder() and create_new
            match read_lock(&p) {
                Some(l) => println!("{} {}", l.harness, l.pid),
                None => println!("? ?"),
            }
            2
        }
        Err(e) => {
            eprintln!("sesync_lock: {e}");
            1
        }
    }
}

fn release(id: &str, harness: &str, pid: u32) -> i32 {
    let p = lock_path(id);
    match read_lock(&p) {
        None => 0, // nothing to release is not an error
        Some(l) if l.harness == harness && l.pid == pid => {
            let _ = fs::remove_file(&p);
            0
        }
        Some(l) if !pid_alive(l.pid) => {
            let _ = fs::remove_file(&p);
            0
        }
        Some(l) => {
            // someone else holds it; refuse to release what we do not own
            println!("{} {}", l.harness, l.pid);
            2
        }
    }
}

fn check(id: &str) -> i32 {
    match holder(id) {
        None => 0,
        Some(l) => {
            println!("{} {}", l.harness, l.pid);
            2
        }
    }
}

fn sweep() -> i32 {
    let dir = lock_dir();
    let Ok(rd) = fs::read_dir(&dir) else { return 0 };
    let mut removed = 0;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "lock").unwrap_or(false) {
            match read_lock(&p) {
                Some(l) if pid_alive(l.pid) => {}
                _ => {
                    if fs::remove_file(&p).is_ok() {
                        removed += 1;
                    }
                }
            }
        }
    }
    println!("{removed}");
    0
}

fn list() -> i32 {
    let dir = lock_dir();
    let Ok(rd) = fs::read_dir(&dir) else { return 0 };
    let t = now();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "lock").unwrap_or(false) {
            if let Some(l) = read_lock(&p) {
                let id = p.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
                let state = if pid_alive(l.pid) { "held" } else { "stale" };
                println!("{id} {state} {} {} {}s {}", l.harness, l.pid, t.saturating_sub(l.since), l.target);
            }
        }
    }
    0
}

/// Value of a flag like `--pid 123` in the argument list.
fn flag<'a>(a: &'a [String], name: &str) -> Option<&'a str> {
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1)).map(String::as_str)
}

/// Read a top-level string or bool field from a JSON object without a parser.
/// Only used for the few fields the hook needs (`role`, `willContinue`).
fn json_field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let at = body.find(&needle)? + needle.len();
    let rest = body[at..].trim_start().strip_prefix(':')?.trim_start();
    if let Some(r) = rest.strip_prefix('"') {
        r.find('"').map(|e| &r[..e])
    } else {
        let e = rest.find(|c: char| c == ',' || c == '}' ).unwrap_or(rest.len());
        Some(rest[..e].trim())
    }
}

/// The harness hook. Replaces the Node extension that was removed.
///
/// `sesync_lock hook <event> <harness> --pid <pid> [--session <id>]`, JSON payload on
/// stdin. Exit 2 means the claim was refused; the holder is printed on stdout as
/// "<harness> <pid>" so the harness can show who holds the conversation.
/// With no session id the conversation is unprotected by decision (#15): nothing
/// is taken and the exit is 0.
fn hook(event: &str, harness: &str, a: &[String]) -> i32 {
    let pid: u32 = match flag(a, "--pid").and_then(|p| p.parse().ok()) {
        Some(p) => p,
        None => { eprintln!("sesync_lock hook: --pid is required"); return 1; }
    };
    let Some(session) = flag(a, "--session").filter(|s| !s.is_empty()) else {
        return 0; // unprotected by decision: no session file, nothing to lock
    };
    let mut body = String::new();
    let _ = io::Read::read_to_string(&mut io::stdin(), &mut body);
    match event {
        "turn_start" => claim(session, harness, pid, "", false),
        "turn_end" | "session_stop" | "session_switch" | "session_shutdown" => release(session, harness, pid),
        "agent_end" => {
            if json_field(&body, "willContinue") == Some("true") { 0 } else { release(session, harness, pid) }
        }
        "message_end" => {
            if json_field(&body, "role") == Some("assistant") { release(session, harness, pid) } else { 0 }
        }
        _ => { eprintln!("sesync_lock hook: unknown event {event}"); 1 }
    }
}

fn usage() -> ! {
    eprintln!(
        "sesync_lock claim <id> <harness> <pid> [target] [--force]\n\
         sesync_lock release <id> <harness> <pid>\n\
         sesync_lock check <id>\n\
         sesync_lock resolve <id>        (mirror id -> source id)\n\
         sesync_lock sweep | list\n\
         exit: 0 ok/free, 2 locked (prints 'harness pid'), 1 error"
    );
    process::exit(1)
}

fn main() {
    let a: Vec<String> = env::args().skip(1).collect();
    let code = match a.first().map(String::as_str) {
        Some("claim") if a.len() >= 4 => {
            let force = a.iter().any(|x| x == "--force");
            let target = a.get(4).filter(|x| *x != "--force").cloned().unwrap_or_default();
            claim(&a[1], &a[2], a[3].parse().unwrap_or_else(|_| usage()), &target, force)
        }
        Some("release") if a.len() >= 4 => release(&a[1], &a[2], a[3].parse().unwrap_or_else(|_| usage())),
        Some("check") if a.len() >= 2 => check(&a[1]),
        Some("resolve") if a.len() >= 2 => { println!("{}", resolve(&a[1])); 0 }
        Some("sweep") => sweep(),
        Some("list") => list(),
        Some("hook") if a.len() >= 3 => hook(&a[1], &a[2], &a[3..]),
        _ => usage(),
    };
    process::exit(code);
}

#[cfg(test)]
mod home_tests {
    use super::home_from;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| owned.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn windows_without_home_uses_userprofile() {
        assert_eq!(home_from(&env(&[("USERPROFILE", "C:\\Users\\u")]), true).as_deref(), Some("C:\\Users\\u"));
    }

    #[test]
    fn windows_skips_a_msys_home() {
        let e = env(&[("HOME", "/c/Users/u"), ("USERPROFILE", "C:\\Users\\u")]);
        assert_eq!(home_from(&e, true).as_deref(), Some("C:\\Users\\u"));
    }

    #[test]
    fn unix_uses_home_and_ignores_userprofile() {
        let e = env(&[("HOME", "/home/u"), ("USERPROFILE", "C:\\Users\\u")]);
        assert_eq!(home_from(&e, false).as_deref(), Some("/home/u"));
        assert_eq!(home_from(&env(&[("USERPROFILE", "C:\\Users\\u")]), false), None);
    }
}
