//! QR rendering for the one-time authenticator secret. A terminal QR is built from half
//! blocks: '▀' paints the top half of a cell dark, '▄' the bottom half. Rounds the code in a
//! quiet zone so phone scanners find it; the colours come from the ui side.

use qrcodegen::QrCode;

/// The quiet zone in modules — helpful when the code is drawn small.
pub const QUIET: i32 = 4;

/// How many modules wide and tall the drawing will be, quiet zone included.
pub fn padded_size(code: &QrCode) -> i32 {
    code.size() + QUIET * 2
}

/// One string per cell row: i.e. two adjacent rows.
/// Every cell is painted with a light background and a top/bottom dark painting via '▀'/'█'/''.
/// Two spaces per module horizontally, so the code stays square on screen: legacy terminals
/// render half blocks 1x2, so the x axis doubles.
pub fn rows(code: &QrCode) -> Vec<String> {
    let n = padded_size(code);
    let mut out = Vec::new();
    let mut y = 0;
    while y < n {
        let mut row = String::new();
        for x in 0..n {
            let top = module(code, x, y - QUIET);
            let bottom = module(code, x, y + 1 - QUIET);
            match (top, bottom) {
                (true, true) => row.push_str("██"),
                (true, false) => row.push_str("▀▀"),
                (false, true) => row.push_str("▄▄"),
                (false, false) => row.push_str("  "),
            }
        }
        out.push(row);
        y += 2;
    }
    out
}

/// Outside the matrix = background (false).
fn module(code: &QrCode, x: i32, in_matrix_y: i32) -> bool {
    if x < QUIET || x >= code.size() + QUIET { return false; }
    if in_matrix_y < 0 || in_matrix_y >= code.size() { return false; }
    let mx = x - QUIET;
    let my = in_matrix_y;
    code.get_module(mx, my)
}

/// The number of dark modules drawn, for tests and spacing checks.
pub fn dark_count(code: &QrCode) -> usize {
    let mut n = 0;
    for row in rows(code) {
        let mut chars = row.chars();
        while let Some(a) = chars.next() {
            match (a, chars.next()) {
                ('█', Some('█')) => n += 2,
                ('▀' | '▄', Some('▀' | '▄')) => n += 1,
                _ => {}
            }
        }
    }
    n
}
