//! The TUI colour schemes. Kept small and dark-first, with a light option;
//! matching the spirit of the old Textual themes, not byte for byte.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    /// The page background, painted over the whole frame like a text-editor scheme.
    pub bg: Color,
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
    /// The strip behind the status and footer rows, distinct from the page background
    /// (like a statusline), so typed input is obvious.
    pub bar: Color,
}

pub const DEFAULT: &str = "textual (dark)";

pub const THEMES: &[Theme] = &[
    Theme {
        name: "textual (dark)",
        bg: Color::Rgb(0x10, 0x10, 0x10),
        text: Color::Rgb(0xE7, 0xE7, 0xE7),
        dim: Color::Rgb(0x76, 0x7C, 0x86),
        accent: Color::Rgb(0x51, 0xA2, 0xEE),
        error: Color::Rgb(0xE1, 0x57, 0x5A),
        warn: Color::Rgb(0xEC, 0xBE, 0x77),
        bar: Color::Rgb(0x1E, 0x26, 0x33),
    },
    Theme {
        name: "textual (light)",
        bg: Color::Rgb(0xFF, 0xFF, 0xFF),
        text: Color::Rgb(0x2A, 0x2C, 0x2E),
        dim: Color::Rgb(0x7D, 0x82, 0x89),
        accent: Color::Rgb(0x21, 0x6B, 0xC9),
        error: Color::Rgb(0xB8, 0x2C, 0x30),
        warn: Color::Rgb(0x9C, 0x6C, 0x00),
        bar: Color::Rgb(0xE4, 0xE6, 0xE9),
    },
    Theme {
        name: "tokyo-night",
        bg: Color::Rgb(0x1A, 0x1B, 0x26),
        text: Color::Rgb(0xC0, 0xCA, 0xF5),
        dim: Color::Rgb(0x56, 0x5F, 0x8F),
        accent: Color::Rgb(0x7A, 0xA2, 0xF7),
        error: Color::Rgb(0xF7, 0x76, 0x8E),
        warn: Color::Rgb(0xE0, 0xAF, 0x68),
        bar: Color::Rgb(0x24, 0x28, 0x3B),
    },
    Theme {
        name: "solarized (light)",
        bg: Color::Rgb(0xFD, 0xF6, 0xE3),
        text: Color::Rgb(0x65, 0x7B, 0x83),
        dim: Color::Rgb(0x93, 0xA1, 0xA1),
        accent: Color::Rgb(0x26, 0x8B, 0xD2),
        error: Color::Rgb(0xDC, 0x32, 0x2F),
        warn: Color::Rgb(0xB5, 0x86, 0x00),
        bar: Color::Rgb(0xEE, 0xE8, 0xD5),
    },
    Theme {
        name: "solarized (dark)",
        bg: Color::Rgb(0x00, 0x2B, 0x36),
        text: Color::Rgb(0x93, 0xA1, 0xA1),
        dim: Color::Rgb(0x65, 0x7B, 0x83),
        accent: Color::Rgb(0x26, 0x8B, 0xD2),
        error: Color::Rgb(0xDC, 0x32, 0x2F),
        warn: Color::Rgb(0xB5, 0x89, 0x00),
        bar: Color::Rgb(0x07, 0x36, 0x42),
    },
    Theme {
        name: "gruvbox (dark)",
        bg: Color::Rgb(0x28, 0x28, 0x28),
        text: Color::Rgb(0xEB, 0xDB, 0xB2),
        dim: Color::Rgb(0x92, 0x83, 0x74),
        accent: Color::Rgb(0x83, 0xA5, 0x98),
        error: Color::Rgb(0xFB, 0x49, 0x34),
        warn: Color::Rgb(0xFA, 0xBD, 0x2F),
        bar: Color::Rgb(0x3C, 0x38, 0x36),
    },
    Theme {
        name: "gruvbox (light)",
        bg: Color::Rgb(0xFB, 0xF1, 0xC7),
        text: Color::Rgb(0x3C, 0x38, 0x36),
        dim: Color::Rgb(0x92, 0x83, 0x74),
        accent: Color::Rgb(0x07, 0x66, 0x78),
        error: Color::Rgb(0x9D, 0x00, 0x06),
        warn: Color::Rgb(0xB5, 0x76, 0x14),
        bar: Color::Rgb(0xEB, 0xDB, 0xB2),
    },
    Theme {
        name: "nord",
        bg: Color::Rgb(0x2E, 0x34, 0x40),
        text: Color::Rgb(0xD8, 0xDE, 0xE9),
        dim: Color::Rgb(0x7B, 0x88, 0xA1),
        accent: Color::Rgb(0x88, 0xC0, 0xD0),
        error: Color::Rgb(0xBF, 0x61, 0x6A),
        warn: Color::Rgb(0xEB, 0xCB, 0x8B),
        bar: Color::Rgb(0x3B, 0x42, 0x52),
    },
    Theme {
        name: "dracula",
        bg: Color::Rgb(0x28, 0x2A, 0x36),
        text: Color::Rgb(0xF8, 0xF8, 0xF2),
        dim: Color::Rgb(0x62, 0x72, 0xA4),
        accent: Color::Rgb(0xBD, 0x93, 0xF9),
        error: Color::Rgb(0xFF, 0x55, 0x55),
        warn: Color::Rgb(0xF1, 0xFA, 0x8C),
        bar: Color::Rgb(0x44, 0x47, 0x5A),
    },
    Theme {
        name: "catppuccin mocha",
        bg: Color::Rgb(0x1E, 0x1E, 0x2E),
        text: Color::Rgb(0xCD, 0xD6, 0xF4),
        dim: Color::Rgb(0x6C, 0x70, 0x86),
        accent: Color::Rgb(0x89, 0xB4, 0xFA),
        error: Color::Rgb(0xF3, 0x8B, 0xA8),
        warn: Color::Rgb(0xFA, 0xB3, 0x87),
        bar: Color::Rgb(0x31, 0x32, 0x44),
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
