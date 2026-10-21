//! The --tui entry point checked in a real pseudo-terminal. The binary must draw the
//! session list, quit on 'q', and leave the terminal usable. POSIX only.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-tuipty-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn tui_draws_the_session_and_quits_on_q() {
    let sb = sandbox("draw");
    let sid = "aaaaaaaa-1111-2222-3333-444444444444";
    let dir = sb.join("omp").join("sessions").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    fs::write(dir.join(format!("2026-10-01T10-00-00-000Z_{sid}.jsonl")),
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{sid}\",\"cwd\":\"/tmp\",\"title\":\"PtyVisibleTitle\"}}\n")).unwrap();

    let (mut master, slave) = {
        let mut m: libc_fd::Pair = libc_fd::open_pty();
        let _ = &mut m;
        (m.master, m.slave)
    };
    let mut child = std::process::Command::new(BIN)
        .arg("--tui")
        .env("SESYNC_DIR", sb.join("state"))
        .env("SESYNC_CONFIG_DIR", sb.join("config"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        // Point O-penCode at an empty sandbox path so the TUI never lists the real store.
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", sb.join("no-opencode-bin"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_TRACE", sb.join("tui-trace.log"))
        .env("TERM", "xterm-256color")
        .stdin(unsafe_stdio(slave))
        .stdout(unsafe_stdio(slave))
        .stderr(unsafe_stdio(slave))
        .spawn()
        .expect("spawn --tui");

    // A pty starts at 0x0, which gives the TUI an empty frame. Set a real size first.
    set_winsize(master, 24, 100);

    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        if let Ok(n) = read_nonblocking(master, &mut buf) {
            seen.extend_from_slice(&buf[..n]);
        }
        if strip_ansi(&seen).contains("PtyVisibleTitle") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let text = strip_ansi(&seen);
    assert!(text.contains("PtyVisibleTitle"), "the TUI must draw the session title, got:\n{text}");

    let mut m = unsafe_file(master);
    m.write_all(b"q").unwrap();
    let status = wait_with_timeout_reading(&mut child, master, &mut seen, Duration::from_secs(10));
    let trace = fs::read_to_string(sb.join("tui-trace.log")).unwrap_or_default();
    assert!(status.is_some(),
        "the TUI must exit on q; the last frames were:\n{}\nthe child's own trace:\n{}",
        strip_ansi(&seen), trace);
    assert!(status.unwrap().success(), "a clean quit must exit 0");

}

/// Enter and ctrl+p open the command list; tab shows the Team pane; ctrl+c still quits.
/// This checks the raw-mode key map end to end, not just the app model.
#[test]
fn tui_opens_the_command_list_and_the_team_tab() {
    let sb = sandbox("palette");
    let sid = "aaaaaaaa-1111-2222-3333-444444444444";
    let dir = sb.join("omp").join("sessions").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    fs::write(dir.join(format!("2026-10-01T10-00-00-000Z_{sid}.jsonl")),
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{sid}\",\"cwd\":\"/tmp\",\"title\":\"PtyPaletteTitle\"}}\n")).unwrap();

    let (mut master, slave) = {
        let mut m: libc_fd::Pair = libc_fd::open_pty();
        let _ = &mut m;
        (m.master, m.slave)
    };
    let mut child = std::process::Command::new(BIN)
        .arg("--tui")
        .env("SESYNC_DIR", sb.join("state"))
        .env("SESYNC_CONFIG_DIR", sb.join("config"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", sb.join("no-opencode-bin"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_TRACE", sb.join("tui-trace.log"))
        .env("TERM", "xterm-256color")
        .stdin(unsafe_stdio(slave))
        .stdout(unsafe_stdio(slave))
        .stderr(unsafe_stdio(slave))
        .spawn()
        .expect("spawn --tui");
    set_winsize(master, 30, 110);

    let wait_for = |needle: &str, master: i32, seen: &mut Vec<u8>| -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline {
            if let Ok(n) = read_nonblocking(master, &mut buf) {
                seen.extend_from_slice(&buf[..n]);
            }
            let text = strip_ansi(seen);
            if text.contains(needle) {
                return text;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        strip_ansi(seen)
    };

    let text = wait_for("PtyPaletteTitle", master, &mut Vec::new());
    assert!(text.contains("PtyPaletteTitle"), "the list must be drawn:\n{text}");

    let checked: &mut Vec<u8> = &mut Vec::new();
    let mut m = unsafe_file(master);
    m.write_all(b"\x10").unwrap(); // ctrl+p opens the command list (Enter opens the session menu)
    let text = wait_for("Rename session", master, checked);
    assert!(text.contains("command:"), "the command list must show its input line:\n{text}");
    m.write_all(b"\x10").unwrap(); // ctrl+p closes it again
    let _ = wait_for("PtyPaletteTitle", master, checked);

    m.write_all(b"\t").unwrap(); // tab moves to the Team pane
    let text = wait_for("Not in a team yet", master, checked);
    assert!(text.contains("Join a team"), "the Team pane must say how to join a team:\n{text}");

    m.write_all(b"\x03").unwrap(); // ctrl+c copies the path and reminds that q quits
    let text = wait_for("press q to quit", master, checked);
    assert!(text.contains("copied"), "ctrl+c must say what was copied:\n{text}");
    m.write_all(b"q").unwrap();
    let status = wait_with_timeout_reading(&mut child, master, checked, Duration::from_secs(10));
    assert!(status.is_some(), "the TUI must exit on q after a copy");
    assert!(status.unwrap().success(), "a clean quit must exit 0");
}

/// Enter on a session opens its menu; the mirror row opens the picker; unticking a harness and
/// confirming shows the chosen set and returns to the list. This is the new per-session flow end
/// to end, not just the app model.
#[test]
fn tui_session_menu_opens_the_mirror_picker_and_confirms() {
    let sb = sandbox("menu");
    let sid = "aaaaaaaa-1111-2222-3333-444444444444";
    let dir = sb.join("omp").join("sessions").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    fs::create_dir_all(sb.join("claude").join("projects")).unwrap();
    let text = "hello from the tui mirror test ".repeat(30);
    fs::write(dir.join(format!("2026-10-01T10-00-00-000Z_{sid}.jsonl")),
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{sid}\",\"cwd\":\"/tmp\",\"title\":\"PtyMenuTitle\"}}\n\
{{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-10-01T10:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n")).unwrap();

    let (mut master, slave) = {
        let mut m: libc_fd::Pair = libc_fd::open_pty();
        let _ = &mut m;
        (m.master, m.slave)
    };
    let mut child = std::process::Command::new(BIN)
        .arg("--tui")
        .env("SESYNC_DIR", sb.join("state"))
        .env("SESYNC_CONFIG_DIR", sb.join("config"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", sb.join("no-opencode-bin"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_TRACE", sb.join("tui-trace.log"))
        .env("TERM", "xterm-256color")
        .stdin(unsafe_stdio(slave))
        .stdout(unsafe_stdio(slave))
        .stderr(unsafe_stdio(slave))
        .spawn()
        .expect("spawn --tui");
    set_winsize(master, 30, 110);

    let wait_for = |needle: &str, master: i32, seen: &mut Vec<u8>| -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline {
            if let Ok(n) = read_nonblocking(master, &mut buf) {
                seen.extend_from_slice(&buf[..n]);
            }
            let text = strip_ansi(seen);
            if text.contains(needle) {
                return text;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        strip_ansi(seen)
    };

    let seen = &mut Vec::new();
    let text = wait_for("PtyMenuTitle", master, seen);
    assert!(text.contains("PtyMenuTitle"), "the list must be drawn:\n{text}");

    let mut m = unsafe_file(master);
    m.write_all(b"\r").unwrap(); // Enter opens the selected session's menu
    let text = wait_for("up/down move", master, seen);
    assert!(text.contains("Mark / unmark for team"), "the session menu must be drawn:\n{text}");

    m.write_all(b"jj\r").unwrap(); // down two rows to Mirror to harness, then run it
    let text = wait_for("space tick", master, seen);
    assert!(text.contains("[ ] π pi") && text.contains("[ ] ⋆ claude"),
        "a fresh picker starts with nothing ticked, each harness behind its icon:\n{text}");

    m.write_all(b"j \r").unwrap(); // tick claude only, confirm
    let text = wait_for("sync with: claude", master, seen);
    assert!(text.contains("sync with: claude"),
        "confirming must name the chosen harnesses:\n{text}");

    m.write_all(b"q").unwrap();
    let status = wait_with_timeout_reading(&mut child, master, seen, Duration::from_secs(10));
    let trace = fs::read_to_string(sb.join("tui-trace.log")).unwrap_or_default();
    assert!(status.is_some(),
        "the TUI must exit on q; the last frames were:\n{}\nthe child's own trace:\n{}",
        strip_ansi(&seen), trace);
    assert!(status.unwrap().success(), "a clean quit must exit 0");

    // The choice must have been saved, and the next pass must write only the chosen harness.
    let pass = std::process::Command::new(BIN)
        .arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("SESYNC_CONFIG_DIR", sb.join("config"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", sb.join("no-opencode-bin"))
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_QUIET", "0")
        .output()
        .expect("run --once after the TUI");
    assert!(pass.status.success(), "{}", String::from_utf8_lossy(&pass.stderr));

    fn jsonl(root: &std::path::Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() { out.extend(jsonl(&p)); }
                else if p.extension().map_or(false, |x| x == "jsonl") { out.push(p); }
            }
        }
        out
    }
    let claude = jsonl(&sb.join("claude").join("projects"));
    assert_eq!(claude.len(), 1, "the chosen claude mirror must be written");
    let body = fs::read_to_string(&claude[0]).unwrap();
    assert!(body.contains("hello from the tui mirror test"), "the conversation must be carried");
    assert!(jsonl(&sb.join("pi").join("sessions")).is_empty(), "the unticked pi target must not be written");
}

// Minimal pty helpers over libc, kept local so no extra crate is needed for tests.
mod libc_fd {
    extern "C" {
        fn openpty(master: *mut i32, slave: *mut i32, name: *mut u8, t: *const u8, w: *const u8) -> i32;
    }
    pub struct Pair { pub master: i32, pub slave: i32 }
    pub fn open_pty() -> Pair {
        let mut m = 0;
        let mut s = 0;
        let r = unsafe { openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) };
        assert_eq!(r, 0, "openpty failed");
        Pair { master: m, slave: s }
    }
}

fn unsafe_stdio(fd: i32) -> std::process::Stdio {
    use std::os::unix::io::FromRawFd;
    unsafe { std::process::Stdio::from_raw_fd(libc_dup(fd)) }
}

fn libc_dup(fd: i32) -> i32 {
    extern "C" { fn dup(fd: i32) -> i32; }
    let d = unsafe { dup(fd) };
    assert!(d >= 0);
    d
}

fn unsafe_file(fd: i32) -> std::fs::File {
    use std::os::unix::io::FromRawFd;
    unsafe { std::fs::File::from_raw_fd(fd) }
}

/// Read whatever is ready without blocking, so the deadline in the caller is honoured.
fn read_nonblocking(fd: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    extern "C" {
        fn read(fd: i32, b: *mut u8, n: usize) -> isize;
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;
    const O_NONBLOCK: i32 = 0o4000;
    unsafe {
        let flags = fcntl(fd, F_GETFL);
        fcntl(fd, F_SETFL, flags | O_NONBLOCK);
        let n = read(fd, buf.as_mut_ptr(), buf.len());
        if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
    }
}

fn wait_with_timeout(child: &mut std::process::Child, d: Duration) -> Option<std::process::ExitStatus> {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if let Ok(Some(s)) = child.try_wait() {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    None
}

/// The same wait, but the master keeps being drained: the TUI paints the theme background on
/// every cell, so one frame is tens of kilobytes; a test that stops reading leaves most of it
/// in the pty buffer, and the child's next write then blocks forever — it would never poll
/// again and never see the `q`. macOS's pty buffer is much smaller than Linux's, where this
/// used to fit.
fn wait_with_timeout_reading(
    child: &mut std::process::Child, fd: i32, seen: &mut Vec<u8>, d: Duration,
) -> Option<std::process::ExitStatus> {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if let Ok(Some(s)) = child.try_wait() {
            return Some(s);
        }
        let mut buf = [0u8; 4096];
        if let Ok(n) = read_nonblocking(fd, &mut buf) {
            seen.extend_from_slice(&buf[..n]);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    None
}

/// Drop CSI and OSC escape sequences so the check reads the text the screen shows.
fn strip_ansi(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('@'..='~').contains(&n) { break; }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
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

/// Give the pty a real window size. Without it ratatui sees a 0x0 frame and draws nothing.
fn set_winsize(fd: i32, rows: u16, cols: u16) {
    #[repr(C)]
    struct Winsize { row: u16, col: u16, xpixel: u16, ypixel: u16 }
    extern "C" { fn ioctl(fd: i32, req: u64, ...) -> i32; }
    // The request number differs by platform: Linux 0x5414, macOS/BSD 0x80087467.
    #[cfg(target_os = "linux")]
    const TIOCSWINSZ: u64 = 0x5414;
    #[cfg(not(target_os = "linux"))]
    const TIOCSWINSZ: u64 = 0x8008_7467;
    let ws = Winsize { row: rows, col: cols, xpixel: 0, ypixel: 0 };
    let r = unsafe { ioctl(fd, TIOCSWINSZ, &ws as *const Winsize) };
    assert_eq!(r, 0, "TIOCSWINSZ failed");
}
