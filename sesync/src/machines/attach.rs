//! `sesync machine attach`: another machine's conversation pane, live in this terminal. The bridge streams the
//! pane's screen as full repaints (JSON `terminal.subscribe`); each is drawn cut to this window, bottom rows
//! kept, with autowrap off so a wider remote pane is cut at the right edge instead of wrapping. Keys go to the
//! pane as typing (`terminal.send` text, as the phone's terminal keyboard sends them), so Enter there is typing,
//! not a prompt turn. PageUp or the mouse wheel shows the pane's scrollback (`terminal.read`, as text) until End,
//! Esc, `q`, scrolling past the bottom, or typing. Ctrl-] detaches. Run inside a herdr tab, this is a remote
//! terminal tab.

use super::client::{self, Reader, Writer};
use super::store::Machine;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::time::Duration;

/// The key that leaves an attached pane, as telnet's escape: Ctrl-].
pub const DETACH: u8 = 0x1d;
/// What every bridge repaint starts with: cursor home, then clear the screen.
const REPAINT: &str = "\x1b[H\x1b[2J";
/// How often the window size is checked, so a resized window is drawn again.
const SIZE_EVERY: Duration = Duration::from_millis(400);
/// The scrollback rows asked of the bridge: all herdr keeps. A bridge older than `terminal.read`'s `lines` sends 200.
const SCROLLBACK: u32 = 1000;
/// Rows one wheel notch scrolls.
const WHEEL: usize = 3;

/// The conversation `query` names among `worktree.ps` rows: its worktree id; else its title, ignoring case;
/// else the one title that contains it. Two that fit equally is an error that lists their ids.
pub fn pick<'a>(rows: &'a [Value], query: &str) -> Result<&'a Value, String> {
    let title = |r: &Value| r["displayName"].as_str().unwrap_or("").to_lowercase();
    let wanted = query.to_lowercase();
    let exact: Vec<&Value> = rows.iter().filter(|r| r["worktreeId"] == query).collect();
    let named: Vec<&Value> = rows.iter().filter(|r| title(r) == wanted).collect();
    let partial: Vec<&Value> = rows.iter().filter(|r| title(r).contains(&wanted)).collect();
    for fits in [exact, named, partial] {
        match fits.as_slice() {
            [] => continue,
            [only] => return Ok(only),
            several => {
                let list: Vec<String> = several
                    .iter()
                    .map(|r| format!("  {}  {}  {}", r["displayName"].as_str().unwrap_or(""), r["path"].as_str().unwrap_or(""), r["worktreeId"].as_str().unwrap_or("")))
                    .collect();
                return Err(format!("{query:?} fits {} conversations; name one by its id:\n{}", several.len(), list.join("\n")));
            }
        }
    }
    Err(format!("no conversation named {query:?} (see `sesync machine list`)"))
}

/// One screen to draw: the remote `frame` (a repaint) cut to `rows` lines of this window, the bottom ones kept,
/// with `status` on the last line. Every line ends its colours, so a cut line cannot tint the next.
pub fn render(frame: &str, rows: u16, status: &str) -> String {
    let body = frame.strip_prefix(REPAINT).unwrap_or(frame);
    let lines: Vec<&str> = body.split("\r\n").collect();
    let room = usize::from(rows.max(2)) - 1;
    let shown = &lines[lines.len().saturating_sub(room)..];
    let mut out = String::from(REPAINT);
    for (i, line) in shown.iter().enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        out.push_str(line);
        out.push_str("\x1b[0m");
    }
    out.push_str(&format!("\x1b[{rows};1H\x1b[0;7m {status} \x1b[0m"));
    out
}

/// What attach does with a key or mouse report read from this terminal. Everything it does not act on is `Text`,
/// typing for the remote pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Text,
    Detach,
    PageUp,
    PageDown,
    WheelUp,
    WheelDown,
    Up,
    Down,
    Home,
    End,
    Esc,
    /// Any other mouse report: clicks and moves, which the remote pane is never sent.
    Mouse,
}

/// `bytes` read from this terminal, split into keys, each with its bytes. Runs of plain text stay together.
fn keys(bytes: &[u8]) -> Vec<(Key, &[u8])> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        let (key, len) = match rest[0] {
            DETACH => (Key::Detach, 1),
            0x1b => escape(rest),
            _ => (Key::Text, rest.iter().position(|&b| b == 0x1b || b == DETACH).unwrap_or(rest.len())),
        };
        out.push((key, &rest[..len]));
        at += len;
    }
    out
}

/// The key the escape sequence starting `rest` is, and its length. A lone Esc ends the read; Alt and a key is
/// text; a CSI sequence runs to its final byte, and an SGR mouse report (`ESC [ < button ; x ; y M`) is a wheel
/// notch for buttons 64 and 65.
fn escape(rest: &[u8]) -> (Key, usize) {
    match rest.get(1) {
        None => (Key::Esc, 1),
        Some(b'[') => {
            let Some(end) = rest[2..].iter().position(|b| (0x40..=0x7e).contains(b)).map(|p| p + 3) else {
                return (Key::Text, rest.len());
            };
            let key = match &rest[2..end] {
                b"5~" => Key::PageUp,
                b"6~" => Key::PageDown,
                b"A" => Key::Up,
                b"B" => Key::Down,
                b"H" | b"1~" => Key::Home,
                b"F" | b"4~" => Key::End,
                [b'<', report @ ..] => match report.split(|&b| b == b';').next() {
                    Some(b"64") => Key::WheelUp,
                    Some(b"65") => Key::WheelDown,
                    _ => Key::Mouse,
                },
                _ => Key::Text,
            };
            (key, end)
        }
        Some(_) => (Key::Text, 2),
    }
}

/// What a key does while the scrollback is shown.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Show the view ending this many rows above the bottom.
    Move(usize),
    /// Back to the live screen.
    Live,
    /// Back to the live screen, and the key is typing for the pane.
    Type,
    Ignore,
}

/// `key` while the view ends `offset` rows above the bottom, `most` being the furthest it can go and `page` the rows
/// a page moves. Scrolling down past the bottom is the live screen again.
fn scroll_step(key: Key, raw: &[u8], offset: usize, most: usize, page: usize) -> Step {
    let down = |by: usize| if by >= offset { Step::Live } else { Step::Move(offset - by) };
    let up = |by: usize| Step::Move(offset.saturating_add(by).min(most));
    match key {
        Key::PageUp => up(page),
        Key::WheelUp => up(WHEEL),
        Key::Up => up(1),
        Key::Home => Step::Move(most),
        Key::PageDown => down(page),
        Key::WheelDown => down(WHEEL),
        Key::Down => down(1),
        Key::End | Key::Esc => Step::Live,
        Key::Text if raw == b"q" => Step::Live,
        Key::Text => Step::Type,
        Key::Mouse | Key::Detach => Step::Ignore,
    }
}

/// The scrollback on show: the pane's last rows as the bridge answered the `read` request (None until it does),
/// and how many rows above the bottom the view ends.
struct Scroll {
    read: String,
    rows: Option<Vec<String>>,
    offset: usize,
}

/// The furthest a view of `rows` rows can end above the bottom in a window `height` rows high.
fn most(rows: usize, height: u16) -> usize {
    rows.saturating_sub(usize::from(height.max(2)) - 1)
}

/// What the window shows: the live `frame`, or the scrollback once its rows have arrived.
fn screen(frame: &str, scroll: Option<&Scroll>, height: u16, name: &str) -> String {
    match scroll.and_then(|s| s.rows.as_ref().map(|rows| (rows, s.offset))) {
        Some((rows, offset)) => {
            let end = rows.len() - offset.min(rows.len());
            let status = format!("{name} — scrollback, {offset} of {} rows up · PgUp PgDn ↑ ↓ wheel · End or q: live", most(rows.len(), height));
            render(&rows[..end].join("\r\n"), height, &status)
        }
        None => render(frame, height, &format!("{name} — PgUp or wheel: scrollback · Ctrl-] detaches")),
    }
}

/// Restores the terminal however attach ends.
struct Screen;

impl Screen {
    fn enter() -> Result<Screen, String> {
        crossterm::terminal::enable_raw_mode().map_err(|e| format!("cannot take over the terminal: {e}"))?;
        enable_vt_input();
        // Alternate screen, no autowrap, no cursor (the remote screen carries no cursor position), mouse reports in
        // SGR form for the wheel.
        print!("\x1b[?1049h\x1b[?7l\x1b[?25l\x1b[?1000h\x1b[?1006h");
        let _ = std::io::stdout().flush();
        Ok(Screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        print!("\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?7h\x1b[?1049l");
        let _ = std::io::stdout().flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Windows consoles hand keys to a program as escape sequences only in virtual-terminal input mode.
#[cfg(windows)]
fn enable_vt_input() {
    use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_INPUT, STD_INPUT_HANDLE};
    unsafe {
        let input = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(input, &mut mode) != 0 {
            SetConsoleMode(input, mode | ENABLE_VIRTUAL_TERMINAL_INPUT);
        }
    }
}

#[cfg(not(windows))]
fn enable_vt_input() {}

/// Attach to the conversation `query` names on `machine` until Ctrl-], or until the pane or the connection ends.
/// A conversation not open in the machine's herdr is opened there first only with `open`, as the phone's Resume
/// does: a bridge that misses an open pane (an older one, or a pane it cannot place) would otherwise start a
/// second agent on a running conversation.
pub async fn run(machine: &Machine, query: &str, open: bool) -> Result<String, String> {
    let (mut writer, mut reader) = client::connect(machine).await?;
    let list = client::call(&mut writer, &mut reader, "worktree.ps", json!({})).await?;
    let rows = list["worktrees"].as_array().cloned().unwrap_or_default();
    let row = pick(&rows, query)?;
    let title = row["displayName"].as_str().unwrap_or(query).to_string();
    let worktree = row["worktreeId"].as_str().unwrap_or("").to_string();
    let terminal = match row["agents"][0]["paneKey"].as_str() {
        Some(handle) => handle.to_string(),
        None if !open => {
            return Err(format!(
                "{title} is not open on {}; add --open to resume it in that machine's herdr (only if it is not running there)",
                machine.name
            ))
        }
        None => {
            let opened = client::call(
                &mut writer,
                &mut reader,
                "session.tabs.createTerminal",
                json!({ "worktree": format!("id:{worktree}") }),
            )
            .await
            .map_err(|e| format!("{title} is not open on {} and could not be opened: {e}", machine.name))?;
            opened["tab"]["terminal"].as_str().ok_or("the machine opened no terminal")?.to_string()
        }
    };
    let host = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).unwrap_or_default();
    let stream = writer
        .request("terminal.subscribe", json!({ "terminal": terminal, "client": { "id": format!("sesync-machine-{host}") } }))
        .await?;
    let name = format!("{} · {title}", machine.name);
    let screen = Screen::enter()?;
    let result = mirror(&mut writer, &mut reader, &stream, &terminal, &name).await;
    drop(screen);
    result.map(|()| format!("detached from {title} on {}", machine.name))
}

async fn mirror(writer: &mut Writer, reader: &mut Reader, stream: &str, terminal: &str, name: &str) -> Result<(), String> {
    let (keys_tx, mut typed_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    // Blocking reads of stdin, on a thread of their own; the thread ends with the process.
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        let mut stdin = std::io::stdin();
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || keys_tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut frame = String::new();
    let mut scroll: Option<Scroll> = None;
    let mut size = crossterm::terminal::size().map_err(|e| e.to_string())?;
    let mut tick = tokio::time::interval(SIZE_EVERY);
    let draw = |frame: &str, scroll: Option<&Scroll>, height: u16| {
        print!("{}", screen(frame, scroll, height, name));
        let _ = std::io::stdout().flush();
    };
    loop {
        tokio::select! {
            message = reader.next() => {
                let Some(message) = message? else { return Err("the machine closed the connection".into()) };
                if let Some(s) = scroll.as_mut().filter(|s| message["id"].as_str() == Some(s.read.as_str())) {
                    let rows = client::outcome(&message).map_err(|e| format!("cannot read the pane's scrollback: {e}"))?;
                    let rows: Vec<String> = rows["terminal"]["tail"]
                        .as_array()
                        .map(|tail| tail.iter().filter_map(|r| r.as_str().map(str::to_string)).collect())
                        .unwrap_or_default();
                    s.offset = s.offset.min(most(rows.len(), size.1));
                    s.rows = Some(rows);
                    draw(&frame, scroll.as_ref(), size.1);
                    continue;
                }
                if message["id"] != stream {
                    continue; // replies to typing
                }
                client::outcome(&message).map_err(|e| format!("cannot open the pane: {e}"))?;
                let result = &message["result"];
                match result["type"].as_str() {
                    Some("scrollback" | "resized") => frame = result["serialized"].as_str().unwrap_or("").to_string(),
                    Some("data") => {
                        let chunk = result["chunk"].as_str().unwrap_or("");
                        if chunk.starts_with(REPAINT) {
                            frame.clear();
                        }
                        frame.push_str(chunk);
                    }
                    Some("end") => return Err("the pane closed on that machine".into()),
                    _ => continue,
                }
                if scroll.is_none() {
                    draw(&frame, None, size.1);
                }
            }
            typed = typed_rx.recv() => {
                let Some(bytes) = typed else { return Ok(()) };
                let page = usize::from(size.1.max(3)) - 2;
                let mut send: Vec<u8> = Vec::new();
                let scrolled = scroll.is_some();
                let mut detach = false;
                for (key, raw) in keys(&bytes) {
                    if key == Key::Detach {
                        detach = true;
                        break;
                    }
                    let Some(s) = scroll.as_mut() else {
                        match key {
                            Key::PageUp | Key::WheelUp => {
                                let read = writer.request("terminal.read", json!({ "terminal": terminal, "lines": SCROLLBACK })).await?;
                                let offset = if key == Key::PageUp { page } else { WHEEL };
                                scroll = Some(Scroll { read, rows: None, offset });
                            }
                            Key::WheelDown | Key::Mouse => {}
                            _ => send.extend_from_slice(raw),
                        }
                        continue;
                    };
                    let furthest = s.rows.as_ref().map_or(usize::MAX, |rows| most(rows.len(), size.1));
                    match scroll_step(key, raw, s.offset, furthest, page) {
                        Step::Move(offset) => s.offset = offset,
                        Step::Live => scroll = None,
                        Step::Type => {
                            scroll = None;
                            send.extend_from_slice(raw);
                        }
                        Step::Ignore => {}
                    }
                }
                if !send.is_empty() {
                    let text = String::from_utf8_lossy(&send).into_owned();
                    writer.request("terminal.send", json!({ "terminal": terminal, "text": text })).await?;
                }
                if detach {
                    return Ok(());
                }
                if scrolled || scroll.is_some() {
                    draw(&frame, scroll.as_ref(), size.1);
                }
            }
            _ = tick.tick() => {
                let now = crossterm::terminal::size().map_err(|e| e.to_string())?;
                if now != size {
                    size = now;
                    draw(&frame, scroll.as_ref(), size.1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, title: &str) -> Value {
        json!({ "worktreeId": id, "displayName": title, "path": "/p" })
    }

    #[test]
    fn a_conversation_is_named_by_id_then_title_then_a_unique_part_of_its_title() {
        let rows = vec![row("r::a", "OTmain"), row("r::b", "OTmain"), row("r::c", "Fix login flow"), row("r::d", "main agent")];
        assert_eq!(pick(&rows, "r::b").unwrap()["worktreeId"], "r::b", "an id wins over equal titles");
        assert_eq!(pick(&rows, "fix LOGIN flow").unwrap()["worktreeId"], "r::c", "case is ignored");
        assert_eq!(pick(&rows, "login").unwrap()["worktreeId"], "r::c");
        let twins = pick(&rows, "OTmain").unwrap_err();
        assert!(twins.contains("r::a") && twins.contains("r::b"), "two equal titles list their ids: {twins}");
        assert!(pick(&rows, "main").is_err(), "a part that fits several is not guessed");
        assert!(pick(&rows, "nothing like it").is_err());
    }

    #[test]
    fn a_frame_taller_than_the_window_keeps_its_bottom_rows_above_the_status_line() {
        let frame = format!("{REPAINT}one\r\n\x1b[31mtwo\r\nthree\r\nfour");
        let out = render(&frame, 3, "work-pc · OTmain");
        assert!(out.starts_with(REPAINT));
        assert!(!out.contains("one") && !out.contains("two"), "the top rows are cut: {out:?}");
        assert!(out.contains("three\x1b[0m\r\nfour\x1b[0m"), "every shown line ends its colours: {out:?}");
        assert!(out.ends_with("\x1b[3;1H\x1b[0;7m work-pc · OTmain \x1b[0m"), "status on the last row: {out:?}");
        let short = render(&format!("{REPAINT}only"), 10, "s");
        assert!(short.contains("only\x1b[0m"), "a short frame is drawn whole: {short:?}");
    }

    #[test]
    fn keys_split_into_scroll_keys_wheel_notches_and_typing() {
        let read = b"hi\x1b[5~\x1b[<64;10;5M\x1b[<65;1;1M\x1b[<0;3;4M\x1b[A\x1b[6~\x1bxok\x1d\x1b";
        let got: Vec<(Key, &[u8])> = keys(read);
        assert_eq!(
            got,
            vec![
                (Key::Text, &b"hi"[..]),
                (Key::PageUp, b"\x1b[5~"),
                (Key::WheelUp, b"\x1b[<64;10;5M"),
                (Key::WheelDown, b"\x1b[<65;1;1M"),
                (Key::Mouse, b"\x1b[<0;3;4M"),
                (Key::Up, b"\x1b[A"),
                (Key::PageDown, b"\x1b[6~"),
                (Key::Text, b"\x1bx"),
                (Key::Text, b"ok"),
                (Key::Detach, b"\x1d"),
                (Key::Esc, b"\x1b"),
            ]
        );
        assert_eq!(keys("é\x1b[1;5C".as_bytes()), vec![(Key::Text, "é".as_bytes()), (Key::Text, &b"\x1b[1;5C"[..])], "Ctrl-Right stays typing");
    }

    #[test]
    fn the_scrollback_view_stops_at_the_top_and_goes_live_past_the_bottom() {
        assert_eq!(scroll_step(Key::PageUp, b"", 10, 100, 20), Step::Move(30));
        assert_eq!(scroll_step(Key::PageUp, b"", 90, 100, 20), Step::Move(100), "the top stops it");
        assert_eq!(scroll_step(Key::Home, b"", 3, 100, 20), Step::Move(100));
        assert_eq!(scroll_step(Key::WheelDown, b"", 5, 100, 20), Step::Move(2));
        assert_eq!(scroll_step(Key::WheelDown, b"", 3, 100, 20), Step::Live, "past the bottom is live");
        assert_eq!(scroll_step(Key::Down, b"", 1, 100, 20), Step::Live);
        assert_eq!(scroll_step(Key::Text, b"q", 9, 100, 20), Step::Live);
        assert_eq!(scroll_step(Key::End, b"", 9, 100, 20), Step::Live);
        assert_eq!(scroll_step(Key::Text, b"hello", 9, 100, 20), Step::Type, "typing goes back and reaches the pane");
        assert_eq!(scroll_step(Key::Mouse, b"", 9, 100, 20), Step::Ignore);
    }

    #[test]
    fn the_scrollback_shows_the_rows_above_the_bottom_with_its_place_in_the_status() {
        let rows: Vec<String> = (1..=10).map(|n| format!("row {n}")).collect();
        let s = Scroll { read: "m3".into(), rows: Some(rows), offset: 4 };
        let out = screen("LIVE SCREEN", Some(&s), 4, "work-pc · OTmain");
        assert!(out.contains("row 4\x1b[0m\r\nrow 5\x1b[0m\r\nrow 6\x1b[0m"), "three rows ending 4 above the bottom: {out:?}");
        assert!(!out.contains("row 7") && !out.contains("LIVE SCREEN"), "{out:?}");
        assert!(out.contains("scrollback, 4 of 7 rows up"), "{out:?}");
        let loading = Scroll { read: "m3".into(), rows: None, offset: 4 };
        assert!(screen("LIVE SCREEN", Some(&loading), 4, "w").contains("LIVE SCREEN"), "the live screen stays until the rows arrive");
    }
}
