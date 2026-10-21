//! Tests for the QR drawing: the quiet zone, square shape, and dark-cell accounting
//! that matches the encoded matrix.

use qrcodegen::QrCodeEcc;
use sesync::qr::{dark_count, padded_size, rows};

const URI: &str = "otpauth://totp/sesync:main?secret=JBSWY3DPEHPK3PXP&issuer=sesync";

fn code() -> qrcodegen::QrCode {
    qrcodegen::QrCode::encode_text(URI, QrCodeEcc::Medium).unwrap()
}

#[test]
fn every_row_has_the_same_padded_width() {
    let c = code();
    let rows = rows(&c);
    let n = padded_size(&c);
    let width = n as usize; // one column per module: a half-block cell is square
    assert_eq!(rows.len(), ((n + 1) / 2) as usize, "two cell rows per line");
    for row in &rows {
        assert_eq!(row.chars().count(), width, "square: |{row}|");
    }
}

#[test]
fn the_quiet_zone_is_blank_at_the_edges() {
    let rows = rows(&code());
    for row in rows.iter().take(1).chain(rows.iter().skip(rows.len() - 1)) {
        let blank = row.chars().all(|c| c == ' ');
        assert!(blank, "the outer margin is blank: |{row}|");
    }
    let first = rows.first().unwrap();
    assert!(first.starts_with("  "), "left margin inside: |{first}|");
    assert!(first.ends_with("  "), "right margin inside: |{first}|");
}

#[test]
fn dark_cells_match_the_encoded_matrix() {
    let c = code();
    let size = c.size() as i32;
    let mut modules = 0usize;
    for y in 0..size {
        for x in 0..size {
            if c.get_module(x, y) {
                modules += 1;
            }
        }
    }
    assert_eq!(dark_count(&c), modules);
}

#[test]
fn the_code_draws_somewhere_in_the_middle() {
    let rows = rows(&code());
    let mid = &rows[rows.len() / 2];
    assert!(mid.contains('█') || mid.contains('▀') || mid.contains('▄'),
        "the middle row holds the QR: |{mid}|");
}

/// Which halves of a drawn row are painted, top then bottom, per column.
fn painted(row: &str) -> Vec<(bool, bool)> {
    row.chars().map(|c| match c {
        '█' => (true, true),
        '▀' => (true, false),
        '▄' => (false, true),
        _ => (false, false),
    }).collect()
}

#[test]
fn light_ink_paints_exactly_the_modules_dark_ink_leaves_blank() {
    // Without colours the glyphs take the terminal's light foreground, so the light modules and
    // the quiet zone are what gets painted: the same code, the other way round.
    use sesync::qr::{rows_inked, Ink, QUIET_MIN};
    let c = code();
    let dark = rows_inked(&c, QUIET_MIN, Ink::Dark);
    let light = rows_inked(&c, QUIET_MIN, Ink::Light);
    let n = (c.size() + 2 * QUIET_MIN) as usize;
    assert_eq!(dark.len(), light.len());
    for (y, (d, l)) in dark.iter().zip(&light).enumerate() {
        for (x, (dp, lp)) in painted(d).into_iter().zip(painted(l)).enumerate() {
            assert_ne!(dp.0, lp.0, "top half of ({x}, {y}) is painted in exactly one ink");
            // The bottom half of the last row lies below the drawing when the size is odd.
            if 2 * y + 1 < n {
                assert_ne!(dp.1, lp.1, "bottom half of ({x}, {y}) is painted in exactly one ink");
            } else {
                assert!(!dp.1 && !lp.1, "nothing is painted below the drawing");
            }
        }
    }
}
