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
    let width = (n * 2) as usize; // two chars per module horizontally
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
