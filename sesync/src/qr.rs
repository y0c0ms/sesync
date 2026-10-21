//! QR rendering for the one-time authenticator secret. A terminal QR is built from half
//! blocks: '▀' paints the top half of a cell dark, '▄' the bottom half, '█' both. Rounds the code
//! in a quiet zone so phone scanners find it; the colours come from the ui side.

use qrcodegen::{QrCode, QrCodeEcc};

/// The quiet zone in modules, as the QR specification asks. `draw_qr` falls back to
/// `QUIET_MIN` only when that is what makes the code fit the terminal.
pub const QUIET: i32 = 4;
/// The narrowest quiet zone drawn. Phone scanners read a 2-module light border reliably.
pub const QUIET_MIN: i32 = 2;

/// The error correction level the QR screen draws with. Low keeps the code small (version 5 for a
/// typical team name, see `compact_uri`); tests encode with this same level.
pub const ECC: QrCodeEcc = QrCodeEcc::Low;

/// The text to encode: the server's otpauth URI without the parameters that only restate the
/// Key URI defaults (SHA1, 6 digits, 30 seconds), which every authenticator applies anyway. The
/// shorter text makes a smaller code. Any other parameter, or another value, is kept.
pub fn compact_uri(uri: &str) -> String {
    let Some((head, query)) = uri.split_once('?') else { return uri.to_string() };
    let kept: Vec<&str> = query.split('&')
        .filter(|p| !matches!(*p, "algorithm=SHA1" | "digits=6" | "period=30"))
        .collect();
    format!("{head}?{}", kept.join("&"))
}

/// How many modules wide and tall the drawing will be, quiet zone included.
pub fn padded_size(code: &QrCode) -> i32 {
    code.size() + QUIET * 2
}

/// One string per terminal row, each covering two module rows (top and bottom half of a cell),
/// with the standard quiet zone. See `rows_with_quiet`.
pub fn rows(code: &QrCode) -> Vec<String> {
    rows_with_quiet(code, QUIET)
}

/// Which modules the block glyphs paint. With colours the screen paints dark modules black on a
/// white cell. Without colours (NO_COLOR) a glyph takes the terminal's own foreground, which is
/// light on the usual dark theme: painting the light modules and the quiet zone then still shows
/// dark modules on a light field, the way scanners read best.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Dark,
    Light,
}

/// One string per terminal row, glyphs on the dark modules. See `rows_inked`.
pub fn rows_with_quiet(code: &QrCode, quiet: i32) -> Vec<String> {
    rows_inked(code, quiet, Ink::Dark)
}

/// One string per terminal row, each covering two module rows (top and bottom half of a cell).
/// One column per module: a terminal cell is about twice as tall as it is wide, so a half-block
/// cell is square and so is every module. Two columns per module would stretch the code to twice
/// its width, which phone scanners read badly and which no longer fits an 80-column terminal.
pub fn rows_inked(code: &QrCode, quiet: i32, ink: Ink) -> Vec<String> {
    let n = code.size() + quiet * 2;
    let painted = |x: i32, y: i32| module(code, quiet, x, y) == (ink == Ink::Dark);
    let mut out = Vec::new();
    let mut y = 0;
    while y < n {
        let mut row = String::new();
        for x in 0..n {
            // The row below the last one (odd sizes) is outside the code: never painted.
            let bottom = y + 1 < n && painted(x, y + 1 - quiet);
            row.push(match (painted(x, y - quiet), bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push(row);
        y += 2;
    }
    out
}

/// Outside the matrix = background (false).
fn module(code: &QrCode, quiet: i32, x: i32, in_matrix_y: i32) -> bool {
    if x < quiet || x >= code.size() + quiet { return false; }
    if in_matrix_y < 0 || in_matrix_y >= code.size() { return false; }
    code.get_module(x - quiet, in_matrix_y)
}

/// The number of dark modules drawn, for tests and spacing checks.
pub fn dark_count(code: &QrCode) -> usize {
    rows(code).iter().flat_map(|r| r.chars()).map(|c| match c {
        '█' => 2,
        '▀' | '▄' => 1,
        _ => 0,
    }).sum()
}

/// The base32 secret an otpauth URI carries, for typing into an authenticator by hand when the
/// code cannot be scanned.
pub fn secret_of(uri: &str) -> Option<&str> {
    let query = uri.split_once('?')?.1;
    query.split('&').find_map(|p| p.strip_prefix("secret="))
}

/// A base32 key in groups of four, as authenticator apps show it ("ABCD EFGH ...").
pub fn grouped(key: &str) -> String {
    key.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c)).collect::<Vec<_>>().join(" ")
}
