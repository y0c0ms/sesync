//! Copying text into the desktop clipboard from the TUI, with the OSC52 escape sequence.
//! Terminals that understand it (wezterm, kitty, foot, tmux with set-clipboard on, ...) put the
//! text on the clipboard of the machine where the terminal runs — which is what a user wants in
//! an ssh session, and needs no extra crate. A terminal that refuses the sequence ignores it
//! silently; the status line then still says "copied", which is the known limit of the approach.

use std::io::Write;

const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, so any tool can decode the payload.
pub fn b64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[(n >> 18) as usize & 63] as char);
        out.push(ALPHA[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHA[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHA[n as usize & 63] as char } else { '=' });
    }
    out
}

/// The escape sequence that asks the terminal to put `text` on the clipboard selection.
pub fn osc52_payload(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", b64(text.as_bytes()))
}

/// Write the copy sequence to the terminal. Nothing terminal-visible is printed: OSC commands
/// are consumed by the terminal, not drawn.
pub fn copy(text: &str) -> Result<(), String> {
    let payload = osc52_payload(text);
    let mut out = std::io::stdout();
    out.write_all(payload.as_bytes()).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}
