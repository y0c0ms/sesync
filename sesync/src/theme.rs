//! The TUI colour schemes. Kept small and dark-first, with a light option;
//! matching the spirit of the old Textual themes, not byte for byte.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    /// Main text.
    pub text: Color,
    /// Hints, keys, quiet info.
    pub dim: Color,
    /// Borders, titles, the highlighted row.
    pub accent: Color,
    /// Broken things.
    pub error: Color,
    /// Warnings.
    pub warn: Color,
}

pub const DEFAULT: &str = "textual (dark)";

pub const THEMES: &[Theme] = &[
    Theme {
        name: "textual (dark)",
        text: Color::Rgb(0xE7, 0xE7, 0xE7),
        dim: Color::Rgb(0x76, 0x7C, 0x86),
        accent: Color::Rgb(0x51, 0xA2, 0xEE),
        error: Color::Rgb(0xE1, 0x57, 0x5A),
        warn: Color::Rgb(0xEC, 0xBE, 0x77),
    },
    Theme {
        name: "textual (light)",
        text: Color::Rgb(0x2A, 0x2C, 0x2E),
        dim: Color::Rgb(0x7D, 0x82, 0x89),
        accent: Color::Rgb(0x21, 0x6B, 0xC9),
        error: Color::Rgb(0xB8, 0x2C, 0x30),
        warn: Color::Rgb(0x9C, 0x6C, 0x00),
    },
    Theme {
        name: "tokyo-night",
        text: Color::Rgb(0xC0, 0xCA, 0xF5),
        dim: Color::Rgb(0x56, 0x5F, 0x8F),
        accent: Color::Rgb(0x7A, 0xA2, 0xF7),
        error: Color::Rgb(0xF7, 0x76, 0x8E),
        warn: Color::Rgb(0xE0, 0xAF, 0x68),
    },
    Theme {
        name: "solarized (light)",
        text: Color::Rgb(0x65, 0x7B, 0x83),
        dim: Color::Rgb(0x93, 0xA1, 0xA1),
        accent: Color::Rgb(0x26, 0x8B, 0xD2),
        error: Color::Rgb(0xDC, 0x32, 0x2F),
        warn: Color::Rgb(0xB5, 0x86, 0x00),
    },
];

/// The theme with this name, or the default one.
pub fn by(name: &str) -> &'static Theme {
    THEMES.iter().find(|t| t.name == name).unwrap_or(&THEMES[0])
}

/// Every theme name, sorted, as the picker lists them.
pub fn names_sorted() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = THEMES.iter().map(|t| t.name).collect();
    names.sort();
    names
}
