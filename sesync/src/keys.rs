//! Terminal key events mapped to app events. Kept in the library, not the binary, so the map is
//! tested. Raw mode turns Ctrl+C into an ordinary key, so it has to be mapped here for the
//! clipboard copy, and quit is plain q.

use crate::app::Ev;
use crossterm::event::{KeyCode, KeyModifiers};

/// The app event for a key press, or None for keys the TUI does not use. A letter with Ctrl or Alt
/// held has only the combinations listed here; any other one does nothing, so it can never be
/// mistaken for the plain letter (Ctrl+Q used to quit).
pub fn key_to_ev(code: KeyCode, mods: KeyModifiers) -> Option<Ev> {
    match code {
        KeyCode::Char('d') if mods == KeyModifiers::CONTROL => Some(Ev::CtrlD),
        KeyCode::Char('r') if mods == KeyModifiers::CONTROL => Some(Ev::CtrlR),
        KeyCode::Char('c') if mods == KeyModifiers::CONTROL => Some(Ev::CtrlC),
        KeyCode::Char('p') if mods == KeyModifiers::CONTROL => Some(Ev::CtrlP),
        KeyCode::Char('t') if mods == KeyModifiers::CONTROL => Some(Ev::Theme),
        KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => Some(Ev::Char(c)),
        KeyCode::Char(_) => None,
        KeyCode::Backspace => Some(Ev::Backspace),
        KeyCode::Enter => Some(Ev::Enter),
        KeyCode::Esc => Some(Ev::Esc),
        KeyCode::Tab => Some(Ev::Tab),
        // Shift with an arrow scrolls the preview of the highlighted session, not the list.
        KeyCode::Up if mods.contains(KeyModifiers::SHIFT) => Some(Ev::PreviewUp),
        KeyCode::Down if mods.contains(KeyModifiers::SHIFT) => Some(Ev::PreviewDown),
        KeyCode::Up => Some(Ev::Up),
        KeyCode::Down => Some(Ev::Down),
        KeyCode::PageUp => Some(Ev::PageUp),
        KeyCode::PageDown => Some(Ev::PageDown),
        KeyCode::Home => Some(Ev::Home),
        KeyCode::End => Some(Ev::End),
        _ => None,
    }
}
