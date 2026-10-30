//! `sesync machine attach`: another machine's conversation pane, live in this terminal. The bridge streams the
//! pane's screen as full repaints (JSON `terminal.subscribe`); each is drawn cut to this window, bottom rows
//! kept, with autowrap off so a wider remote pane is cut at the right edge instead of wrapping. Keys go to the
//! pane as typing (`terminal.send` text, as the phone's terminal keyboard sends them), so Enter there is typing,
//! not a prompt turn. Ctrl-] detaches. Run inside a herdr tab, this is a remote terminal tab.

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

/// Restores the terminal however attach ends.
struct Screen;

impl Screen {
    fn enter() -> Result<Screen, String> {
        crossterm::terminal::enable_raw_mode().map_err(|e| format!("cannot take over the terminal: {e}"))?;
        enable_vt_input();
        // Alternate screen, no autowrap, no cursor: the remote screen carries no cursor position.
        print!("\x1b[?1049h\x1b[?7l\x1b[?25l");
        let _ = std::io::stdout().flush();
        Ok(Screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        print!("\x1b[?25h\x1b[?7h\x1b[?1049l");
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
    let status = format!("{} · {title} — Ctrl-] detaches", machine.name);
    let screen = Screen::enter()?;
    let result = mirror(&mut writer, &mut reader, &stream, &terminal, &status).await;
    drop(screen);
    result.map(|()| format!("detached from {title} on {}", machine.name))
}

async fn mirror(writer: &mut Writer, reader: &mut Reader, stream: &str, terminal: &str, status: &str) -> Result<(), String> {
    let (keys_tx, mut keys) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
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
    let mut size = crossterm::terminal::size().map_err(|e| e.to_string())?;
    let mut tick = tokio::time::interval(SIZE_EVERY);
    let draw = |frame: &str, rows: u16| {
        print!("{}", render(frame, rows, status));
        let _ = std::io::stdout().flush();
    };
    loop {
        tokio::select! {
            message = reader.next() => {
                let Some(message) = message? else { return Err("the machine closed the connection".into()) };
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
                draw(&frame, size.1);
            }
            typed = keys.recv() => {
                let Some(bytes) = typed else { return Ok(()) };
                let (send, detach) = match bytes.iter().position(|&b| b == DETACH) {
                    Some(at) => (&bytes[..at], true),
                    None => (&bytes[..], false),
                };
                if !send.is_empty() {
                    let text = String::from_utf8_lossy(send).into_owned();
                    writer.request("terminal.send", json!({ "terminal": terminal, "text": text })).await?;
                }
                if detach {
                    return Ok(());
                }
            }
            _ = tick.tick() => {
                let now = crossterm::terminal::size().map_err(|e| e.to_string())?;
                if now != size {
                    size = now;
                    draw(&frame, size.1);
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
}
