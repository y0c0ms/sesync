//! Real check: after a rename, `claude --resume` must list the new name.
//! Ported from daemon/tests/test_rename.py (test_claude_resume_picker_shows_the_renamed_title),
//! which was lost in the Rust port.
//!
//! The session starts with a custom-title, as /rename or -n write. Claude prefers that
//! over ai-title, so a rename that writes only ai-title would not show. The rename runs
//! through the daemon's own manage::rename_group. The picker is driven in a pty against
//! the real `claude` binary.
//!
//! Skip rules (issue #11): when the claude CLI is not installed the test is skipped,
//! unless HS_REQUIRE_CLAUDE=1, in which case a missing CLI fails the test. A skip is
//! reported on stderr so it cannot pass silently.
//!
//! This check is local-only by decision: hosted CI has no logged-in claude CLI, so CI
//! does not set HS_REQUIRE_CLAUDE. Run it before a release with HS_REQUIRE_CLAUDE=1.
#![cfg(unix)]

use sesync::manage::{rename_group, Group, Roots};
use std::fs;
use std::io::Write;
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CID: &str = "bbbbbbbb-1111-2222-3333-444444444444";

fn find_claude() -> Option<PathBuf> {
    // Real harness CLIs run only when HS_E2E=1. Plain `cargo test` never starts them,
    // so a local run cannot touch a live session store or call a model.
    if std::env::var("HS_E2E").map_or(true, |v| v != "1") {
        return None;
    }
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p).map(|d| d.join("claude")).find(|c| c.is_file())
    })
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-claude-resume-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Strip terminal control sequences so the check reads the text the screen shows.
fn strip_ansi(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            match it.peek() {
                Some('[') => {
                    it.next();
                    while let Some(&n) = it.peek() {
                        it.next();
                        if ('@'..='~').contains(&n) { break; }
                    }
                }
                Some(']') => {
                    it.next();
                    while let Some(n) = it.next() {
                        if n == '\u{7}' { break; }
                    }
                }
                _ => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn claude_resume_picker_shows_the_renamed_title() {
    let require = std::env::var("HS_REQUIRE_CLAUDE").map_or(false, |v| v == "1");
    let Some(claude) = find_claude() else {
        if require {
            panic!("HS_REQUIRE_CLAUDE=1 but the claude CLI is not on PATH or HS_E2E=1 is not set");
        }
        eprintln!("SKIPPED claude_resume: claude CLI not installed (set HS_REQUIRE_CLAUDE=1 to fail instead)");
        return;
    };

    let sb = sandbox("picker");
    let cfg = sb.join("claude");
    let ws = sb.join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(cfg.join("projects")).unwrap();
    fs::write(cfg.join(".claude.json"),
        r#"{"hasCompletedOnboarding":true,"numStartups":5,"projects":{}}"#).unwrap();

    // Claude names each project folder by replacing non-alphanumerics with '-'.
    let ws_real = ws.canonicalize().unwrap();
    let key: String = ws_real.display().to_string().chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let proj = cfg.join("projects").join(&key);
    fs::create_dir_all(&proj).unwrap();
    let transcript = proj.join(format!("{CID}.jsonl"));
    let ws_s = ws_real.display().to_string();
    fs::write(&transcript, [
        format!(r#"{{"type":"user","uuid":"u1","sessionId":"{CID}","cwd":"{ws_s}","timestamp":"2026-10-01T10:00:00.000Z","message":{{"role":"user","content":"hello rename probe"}}}}"#),
        format!(r#"{{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"{CID}","cwd":"{ws_s}","timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"hi"}}]}}}}"#),
        format!(r#"{{"type":"custom-title","customTitle":"Old generated name","sessionId":"{CID}"}}"#),
    ].join("\n") + "\n").unwrap();

    // Rename through the daemon's own path.
    let mut roots = Roots::under(&sb, sb.join("no-sesync-conv"));
    roots.locations.set("claude", sesync::harness::Location { root: cfg.clone(), cli: None });
    fs::create_dir_all(&roots.locations.sessions_dir("omp")).unwrap();
    fs::create_dir_all(&roots.locations.sessions_dir("pi")).unwrap();
    let group = Group::file("claude", transcript.display().to_string(), "Old generated name");
    rename_group(&roots, &group, "Renamed for resume", &sb.join("locks"), &sb.join("no-sesync_lock"))
        .expect("the daemon's rename must succeed");

    // Drive `claude --resume` in a pty and look for the new name.
    let found = drive_picker(&claude, &cfg, &ws_real, "Renamed for resume");
    match found {
        Ok(()) => {}
        Err(screen) => panic!("claude --resume did not show the renamed title. Last screen:\n{}",
                              tail(&screen, 800)),
    }
    let _ = fs::remove_dir_all(&sb);
}

fn tail(s: &str, n: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    c[c.len().saturating_sub(n)..].iter().collect()
}

fn drive_picker(claude: &Path, cfg: &Path, ws: &Path, want: &str) -> Result<(), String> {
    use std::process::Command;
    // Open a pty pair and run claude on the slave end.
    extern "C" {
        fn openpty(m: *mut i32, s: *mut i32, name: *mut u8, t: *const u8, w: *const u8) -> i32;
        fn read(fd: i32, b: *mut u8, n: usize) -> isize;
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
        fn ioctl(fd: i32, req: u64, ...) -> i32;
    }
    let (mut m, mut s) = (0i32, 0i32);
    if unsafe { openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) } != 0 {
        return Err("openpty failed".into());
    }
    #[repr(C)]
    struct Winsize { row: u16, col: u16, x: u16, y: u16 }
    #[cfg(target_os = "linux")]
    const TIOCSWINSZ: u64 = 0x5414;
    #[cfg(not(target_os = "linux"))]
    const TIOCSWINSZ: u64 = 0x8008_7467;
    let size = Winsize { row: 24, col: 100, x: 0, y: 0 };
    unsafe { ioctl(m, TIOCSWINSZ, &size as *const Winsize) };

    // Each fd has exactly one owner. The slave is dup'd for stdout and stderr; the
    // original slave fd goes to stdin. The master is owned once, by `master` below,
    // which closes it on drop. Nothing else closes these fds.
    let slave_in = unsafe { std::fs::File::from_raw_fd(s) };
    let slave_out = unsafe { std::fs::File::from_raw_fd(libc_dup(libc_fd_of(&slave_in))) };
    let slave_err = unsafe { std::fs::File::from_raw_fd(libc_dup(libc_fd_of(&slave_in))) };
    let mut child = Command::new(claude)
        .arg("--resume")
        .current_dir(ws)
        .env("CLAUDE_CONFIG_DIR", cfg)
        .env("TERM", "xterm-256color")
        .stdin(slave_in)
        .stdout(slave_out)
        .stderr(slave_err)
        .spawn()
        .map_err(|e| format!("cannot start claude: {e}"))?;

    let mut master = unsafe { std::fs::File::from_raw_fd(m) };
    let mut buf: Vec<u8> = Vec::new();
    let mut sent_trust = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut chunk = [0u8; 4096];
    let want_nospace = want.replace(' ', "");
    let mut found = false;
    while Instant::now() < deadline {
        let n = unsafe {
            let flags = fcntl(m, 3);
            fcntl(m, 4, flags | 0o4000);
            read(m, chunk.as_mut_ptr(), chunk.len())
        };
        if n > 0 {
            buf.extend_from_slice(&chunk[..n as usize]);
        }
        let text = strip_ansi(&buf);
        // Answer the workspace trust dialog once it is on screen. The renderer drops
        // spaces, so match on the squashed text. Enter is sent once, as the probe does.
        let squashed = text.replace(' ', "");
        if !sent_trust && squashed.contains("trustthisfolder") {
            let _ = master.write_all(b"\r");
            sent_trust = true;
            buf.clear();
            std::thread::sleep(Duration::from_millis(1500));
            continue;
        }
        if text.replace(' ', "").contains(&want_nospace) {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    // `master` closes the master fd when it drops here; no explicit close.
    drop(master);
    let screen = strip_ansi(&buf);
    if found { Ok(()) } else { Err(screen) }
}

fn libc_fd_of(f: &std::fs::File) -> i32 {
    use std::os::unix::io::AsRawFd;
    f.as_raw_fd()
}

fn libc_dup(fd: i32) -> i32 {
    extern "C" { fn dup(fd: i32) -> i32; }
    unsafe { dup(fd) }
}
