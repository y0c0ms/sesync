//! Drawing for the manage TUI, with ratatui. Reads the App and writes a frame.
//! No state is kept here: the App owns everything the screen shows. Dialogs float centered
//! over the lists, in the style of the old Textual palette.

use crate::app::{App, Kind, Mode, View};
use crate::theme::Theme;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};
use std::time::Instant;

const QR_FG: Color = Color::Rgb(0x10, 0x10, 0x10);
const QR_BG: Color = Color::Rgb(0xFF, 0xFF, 0xFF);

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let th = app.theme();
    // The theme owns the page background, like a text-editor colour scheme: paint it over the
    // whole frame first; later widgets only set foregrounds, so the fill shows through.
    f.render_widget(Block::default().style(Style::default().bg(th.bg)), area);
    let chunks = Layout::vertical([
        Constraint::Length(1), // tab bar
        Constraint::Min(1),    // list, team list
        Constraint::Length(1), // status or rename prompt
        Constraint::Length(1), // key help
    ])
    .split(area);

    draw_tab_bar(f, app, th, chunks[0]);
    match app.view() {
        View::Sessions => draw_sessions(f, app, th, chunks[1]),
        View::Team => draw_team(f, app, th, chunks[1]),
    }

    let status_line = match app.mode() {
        // The status goes beside the input, so a refused rename is visible while the user edits.
        // A rename refusal is an error and does not fade, so the raw status is honest here too.
        Mode::Rename => Some((format!("Rename: {}_  {}", app.input(), app.status()), app.status_is_error())),
        _ => app.status_line(Instant::now()).map(|(t, e)| (t.to_string(), e)),
    };
    // Both bottom rows sit on the theme's bar colour, like a statusline, so typed input and
    // messages are easy to find. While the filter is being typed it gets the foreground colour
    // and bold: that is the text the user is editing.
    let (status_col, bold) = match (&status_line, app.mode()) {
        (Some((_, true)), _) => (th.error, false),
        (_, Mode::Filter) => (th.text, true),
        _ => (th.text, false),
    };
    let mut status_style = Style::default().fg(status_col).bg(th.bar);
    if bold {
        status_style = status_style.add_modifier(Modifier::BOLD);
    }
    let status_text = status_line.as_ref().map(|(t, _)| t.as_str()).unwrap_or("");
    f.render_widget(Paragraph::new(status_text).style(status_style), chunks[2]);
    f.render_widget(Paragraph::new(app.footer()).style(Style::default().fg(th.dim).bg(th.bar)), chunks[3]);

    // Dialogs float over everything.
    match app.mode() {
        Mode::Palette => draw_palette(f, app, th, area),
        Mode::SessionMenu => draw_session_menu(f, app, th, area),
        Mode::MirrorPick => draw_mirror_pick(f, app, th, area),
        Mode::Theme => draw_theme(f, app, th, area),
        Mode::Form => draw_form(f, app, th, area),
        Mode::Qr => draw_qr(f, app, th, area),
        Mode::Help => draw_help(f, app, th, area),
        Mode::Transcript => draw_transcript(f, app, th, area),
        Mode::DeleteConfirm => draw_delete_confirm(f, app, th, area),
        _ => {}
    }
}

fn draw_tab_bar(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let selected = Style::default().fg(th.text).bg(th.accent);
    let unselected = Style::default().fg(th.dim);
    let (s_style, t_style) = match app.view() {
        View::Sessions => (selected, unselected),
        View::Team => (unselected, selected),
    };
    let bar = Line::from(vec![
        Span::styled(" Sessions ", s_style),
        Span::styled("│", Style::default().fg(th.dim)),
        Span::styled(format!(" Team ({}) ", app.team_rows().len()), t_style),
    ]);
    f.render_widget(Paragraph::new(bar), area);
}

fn draw_sessions(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    use crate::session_cols::fit;
    // Right-hand columns: team state, last sync, next sync, mirrors, updated. Hidden on narrow panes
    // so the session title keeps its room.
    const COLS: [(&str, usize); 5] = [("TEAM", 7), ("LAST SYNC", 10), ("NEXT", 10), ("MIRRORS", 12), ("UPDATED", 10)];
    const BLOCK: usize = 7 + 10 + 10 + 12 + 10 + 5; // widths plus a space between each column
    let show_cols = area.width as usize >= 8 + 2 + BLOCK + 16;
    let title_w = (area.width as usize).saturating_sub(8 + 2 + if show_cols { BLOCK } else { 0 });

    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    if show_cols {
        let mut head = format!("{}  {}", " ".repeat(8), fit("TITLE", title_w));
        for (name, w) in COLS {
            head.push(' ');
            head.push_str(&fit(name, w));
        }
        f.render_widget(Paragraph::new(head).style(Style::default().fg(th.dim)), parts[0]);
    }

    let items: Vec<ListItem> = app
        .visible_rows()
        .iter()
        .map(|g| {
            let mut spans = vec![
                Span::styled(format!("{:<6}", g.fmt), Style::default().fg(th.dim)),
                // A star marks a session the user chose for the team; unmarked rows show a space.
                Span::raw(if app.is_marked(g) { "* " } else { "  " }),
                Span::raw(fit(&g.title, title_w)),
            ];
            if show_cols {
                let c = app.session_cols(g);
                let values = [c.team, c.last_sync, c.next_sync];
                for i in 0..3 {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(fit(&values[i], COLS[i].1), Style::default().fg(th.dim)));
                }
                spans.push(Span::raw(" "));
                spans.extend(mirror_spans(&c.mirror_parts, COLS[3].1, th));
                spans.push(Span::raw(" "));
                spans.push(Span::styled(fit(&c.updated, COLS[4].1), Style::default().fg(th.dim)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    // The highlighted row is the cursor; the offset keeps it on screen when the list is longer than the pane.
    let list_area = parts[1];
    let height = list_area.height as usize;
    let selected = if app.visible_rows().is_empty() { None } else { Some(app.selected_index()) };
    let offset = match selected {
        Some(sel) if height > 0 => sel.saturating_sub(height - 1),
        _ => 0,
    };
    let mut list_state = ListState::default().with_offset(offset).with_selected(selected);
    // Only the background marks the cursor row; the foreground colours (marker states,
    // dimmed columns) stay readable on the selected row.
    let list = List::new(items)
        .highlight_style(Style::default().bg(th.accent))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, list_area, &mut list_state);
}

/// The MIRRORS cell, one span per target: dim when mirrored, warn while a chosen write is
/// still pending, error when it failed. The plain text is used when the cell must be cut.
fn mirror_spans(parts: &[(String, crate::session_cols::MirrorState)], width: usize, th: &Theme) -> Vec<Span<'static>> {
    use crate::session_cols::{fit, mirror_mark, MirrorState};
    let plain: String = parts.iter().map(|(t, s)| format!("{t}{}", mirror_mark(*s))).collect::<Vec<_>>().join(",");
    if plain.chars().count() > width {
        return vec![Span::styled(fit(&plain, width), Style::default().fg(th.dim))];
    }
    let mut spans = Vec::new();
    for (i, (t, s)) in parts.iter().enumerate() {
        let color = match s {
            MirrorState::Mirrored => th.dim,
            MirrorState::Pending => th.warn,
            MirrorState::Failing => th.error,
        };
        let sep = if i > 0 { "," } else { "" };
        spans.push(Span::styled(format!("{sep}{t}{}", mirror_mark(*s)), Style::default().fg(color)));
    }
    let used = plain.chars().count();
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    spans
}

/// The Team pane: what a verified pull actually brought over. Without a server configured the
/// pane says so instead of showing nothing.
fn draw_team(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);
    let header = Paragraph::new(app.team_header(std::time::Instant::now()))
        .style(Style::default().fg(th.dim));
    f.render_widget(header, parts[0]);
    let area = parts[1];
    let rows = app.team_rows();
    if rows.is_empty() {
        let msg = if app.team_configured() {
            "No verified team sessions yet. Press ctrl+r to pull."
        } else {
            "No team server configured. Set SESYNC_TEAM_URL, join from the command list (ctrl+p, Join a team), then press ctrl+r."
        };
        f.render_widget(Paragraph::new(msg).style(Style::default().fg(th.dim)), area);
        return;
    }
    let height = area.height as usize;
    let selected = Some(app.team_index());
    let offset = selected.unwrap_or(0).saturating_sub(height.saturating_sub(1));
    let mut list_state = ListState::default().with_offset(offset).with_selected(selected);
    let items: Vec<ListItem> = rows.iter().map(|r| ListItem::new(r.clone())).collect();
    let list = List::new(items)
        .highlight_style(Style::default().bg(th.accent))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut list_state);
}

// ----- floating dialogs -------------------------------------------------------------------

/// A centered rectangle of the given size, clamped to the screen.
fn centered(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width.saturating_sub(2)).max(3);
    let h = h.min(area.height.saturating_sub(2)).max(3);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Fill the area with the page background and draw a bordered box; returns the inside of the box.
fn dialog(f: &mut Frame, title: &str, th: &Theme, outer: Rect) -> Rect {
    // A dialog floats over rows that were already painted. `set_style` only restyles the cells
    // that are there - it does not replace their symbols - so restamping the background alone
    // leaves the list's characters (a stray `"`, the TEAM column) showing through under the box.
    // Clear the rectangle first (that is what removes the symbols), then paint the background.
    f.render_widget(Clear, outer);
    f.render_widget(Block::default().style(Style::default().bg(th.bg)), outer);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.accent))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(th.text).bg(th.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(outer);
    f.render_widget(block, outer);
    inner
}

/// Hint rows sit at the bottom of dialogs, dimmed.
fn hint(f: &mut Frame, text: &str, th: &Theme, area: Rect) {
    f.render_widget(Paragraph::new(text).style(Style::default().fg(th.dim)), area);
}

fn on_accent(th: &Theme, bold: bool) -> Style {
    let s = Style::default().fg(th.text).bg(th.accent);
    if bold { s.add_modifier(Modifier::BOLD) } else { s }
}

/// The command list as a floating dialog with its input line and hint.
fn draw_palette(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let rows = app.palette_rows();
    let desired_height = (rows.len() as u16) + 5; // border(2) + query + hint + slack
    let outer = centered(72, desired_height, area);
    let inner = dialog(f, "Command palette", th, outer);
    let parts = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ]).split(inner);
    f.render_widget(
        Paragraph::new(format!("command: {}_", app.palette_query())).style(Style::default().fg(th.text)),
        parts[0],
    );
    let cursor = rows.iter().position(|r| r.highlighted).unwrap_or(0);
    let list_rows: Vec<Line<'static>> = rows.iter()
        .map(|r| {
            let state = if r.why.is_empty() { "enabled".to_string() } else { format!("off: {}", r.why) };
            let state_style = if r.highlighted {
                on_accent(th, false)
            } else if r.why.is_empty() {
                Style::default().fg(th.dim)
            } else {
                Style::default().fg(th.warn)
            };
            let key_style = if r.highlighted { on_accent(th, false) } else { Style::default().fg(th.dim) };
            let title_style = if r.highlighted { on_accent(th, true) } else { Style::default().fg(th.text) };
            Line::from(vec![
                Span::styled(format!(" {:<22}", r.title), title_style),
                Span::styled(format!("{:<8}", r.key), key_style),
                Span::styled(state, state_style),
            ])
        })
        .collect();
    let shown: Vec<ListItem> = show_window(&list_rows, parts[1].height as usize, cursor);
    let list = List::new(shown);
    f.render_widget(list, parts[1]);
    hint(f, "up/down move · enter run · esc close", th, parts[2]);
}


/// Options for the selected session. Each row names the key that does the same thing.
fn draw_session_menu(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let rows = app.session_menu_rows();
    let outer = centered(56, rows.len() as u16 + 4, area);
    let inner = dialog(f, "Session", th, outer);
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let cursor = app.menu_cursor();
    let lines: Vec<Line<'static>> = rows.iter().enumerate().map(|(i, (title, key))| {
        let on = i == cursor;
        let style = if on { on_accent(th, true) } else { Style::default().fg(th.text) };
        let key_style = if on { on_accent(th, false) } else { Style::default().fg(th.dim) };
        Line::from(vec![
            Span::styled(format!(" {:<26}", title), style),
            Span::styled(key.to_string(), key_style),
        ])
    }).collect();
    f.render_widget(List::new(lines.into_iter().map(ListItem::new).collect::<Vec<_>>()), parts[0]);
    hint(f, "up/down move · enter run · esc close", th, parts[1]);
}

/// Pick the harnesses to mirror the selected session into. Installed targets only; the session's
/// own format is never offered. A ticked row is marked with [x], and each row leads with the
/// harness's glyph so the list reads at a glance.
fn draw_mirror_pick(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    const HINT: &str = "up/down · space tick · a all · n none · enter mirror · esc cancel";
    let targets = app.picker_targets();
    // Wide enough for the hint plus the border and padding: the fixed 56 used to cut "esc cancel" off.
    let width = (HINT.chars().count() as u16 + 4).max(56);
    let outer = centered(width, targets.len().max(1) as u16 + 4, area);
    let inner = dialog(f, "Mirror to harness", th, outer);
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    if targets.is_empty() {
        f.render_widget(Paragraph::new(" no other harness is installed here").style(Style::default().fg(th.dim)), parts[0]);
    } else {
        let cursor = app.pick_cursor();
        let lines: Vec<ListItem> = targets.iter().enumerate().map(|(i, t)| {
            let mark = if app.pick_is_chosen(t) { "[x]" } else { "[ ]" };
            let on = i == cursor;
            let style = if on { on_accent(th, true) } else { Style::default().fg(th.text) };
            let mut spans = vec![Span::styled(format!(" {mark} {} {t}", harness_icon(t)), style)];
            if let Some(note) = app.pick_row_note(t) {
                let note_style = if on { style } else { Style::default().fg(th.dim) };
                spans.push(Span::styled(format!("  {note}"), note_style));
            }
            ListItem::new(Line::from(spans))
        }).collect();
        f.render_widget(List::new(lines), parts[0]);
    }
    hint(f, HINT, th, parts[1]);
}

/// The glyph shown before a harness's name in the picker.
fn harness_icon(fmt: &str) -> &'static str {
    match fmt {
        "claude" => "✶",
        "gemini" => "✦",
        "omp" => "Ω",
        "opencode" => "◉",
        "pi" => "π",
        _ => "•",
    }
}

/// Colour scheme picker: the scheme at the cursor is applied live, so Enter keeps it and
/// Esc reverts to what was in use when the dialog opened. "(active)" marks the current one.
fn draw_theme(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let names = crate::theme::names_sorted();
    let outer = centered(46, (names.len() as u16) + 5, area);
    let inner = dialog(f, "Colour scheme", th, outer);
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let (highlighted, active) = app.theme_highlighted();
    let marker = if active { " (active)" } else { "" };
    f.render_widget(
        Paragraph::new(format!("highlighted: {highlighted}{marker}")).style(on_accent(th, true)),
        parts[0],
    );
    let cursor = app.theme_cursor();
    let list_rows: Vec<Line<'static>> = names.iter().enumerate().map(|(i, name)| {
        let on = i == cursor;
        let style = if on { on_accent(th, true) } else { Style::default().fg(th.text) };
        Line::from(Span::styled(format!(" {name}"), style))
    }).collect();
    let shown: Vec<ListItem> = show_window(&list_rows, parts[1].height as usize, cursor);
    f.render_widget(List::new(shown), parts[1]);
    hint(f, "up/down preview · enter apply · esc revert", th, parts[2]);
}

/// The join/start dialog: one input row per field, the current one highlighted.
fn draw_form(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let Some(view) = app.form_view() else { return };
    let title = match view.kind {
        Kind::Enroll => "Join a team",
        Kind::Register => "Start a team",
    };
    let outer = centered(64, (view.fields.len() as u16) + if view.kind == Kind::Register { 7 } else { 6 }, area);
    let inner = dialog(f, title, th, outer);
    let rows_n = if view.kind == Kind::Register { 2 } else { 1 };
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(rows_n)]).split(inner);
    let rows: Vec<Line<'static>> = view.fields.iter().map(|(name, value)| {
        let here = view.cursor_here(name);
        let label = format!("{:<16}", name);
        let rest = if here { format!("{value}_") } else { value.clone() };
        Line::from(vec![
            Span::styled(format!(" {label}"), if here { on_accent(th, true) } else { Style::default().fg(th.dim) }),
            Span::styled(rest, if here { on_accent(th, false) } else { Style::default().fg(th.text) }),
        ])
    }).collect();
    let cursor = view.cursor;
    let shown: Vec<ListItem> = show_window(&rows, parts[0].height as usize, cursor);
    f.render_widget(List::new(shown), parts[0]);
    let warning = match view.kind {
        Kind::Register => "the dialog's answer is shown once and cannot be read back",
        Kind::Enroll => "",
    };
    if warning.is_empty() {
        hint(f, "tab next field · enter submit · esc cancel", th, parts[1]);
    } else {
        let tw = Paragraph::new(vec![
            Line::from(Span::styled(format!(" {warning}"), Style::default().fg(th.dim))),
            Line::from(Span::styled(" tab next field · enter submit · esc cancel", Style::default().fg(th.dim))),
        ]);
        f.render_widget(tw, parts[1]);
    }
}

/// The `?` overlay: the keys of the current tab, one line each.
fn draw_help(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let lines = app.help_lines();
    let height = lines.len() as u16 + 4;
    let inner = dialog(f, if app.view() == View::Team { "Keys — team" } else { "Keys — sessions" }, th,
        centered(74, height, area));
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let items: Vec<ListItem> = lines.iter().map(|(keys, what)| {
        ListItem::new(Line::from(vec![
            Span::styled(format!(" {:<8}", keys), Style::default().fg(th.accent).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {what}"), Style::default().fg(th.text)),
        ]))
    }).collect();
    f.render_widget(List::new(items), parts[0]);
    hint(f, "esc close", th, parts[1]);
}

/// The transcript view: the session's own file, scrollable.
fn draw_transcript(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let Some(t) = app.transcript_view() else { return };
    let name = t.path.rsplit('/').next().unwrap_or(&t.path).to_string();
    let inner = dialog(f, format!("Transcript — {name}").as_str(), th, centered(96, 24, area));
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let body = parts[0].height as usize;
    let start = t.scroll.min(t.lines.len().saturating_sub(1));
    let shown: Vec<ListItem> = t.lines[start..(start + body).min(t.lines.len())]
        .iter()
        .map(|l| ListItem::new(Line::from(Span::styled(l.clone(), Style::default().fg(th.text)))))
        .collect();
    f.render_widget(List::new(shown), parts[0]);
    hint(f, "j/k scroll · esc close", th, parts[1]);
}

/// The delete confirmation: what will go, and the copies that will not.
fn draw_delete_confirm(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let Some((title, path)) = app.delete_preview() else { return };
    let inner = dialog(f, "Delete session", th, centered(76, 7, area));
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let body = Paragraph::new(vec![
        Line::from(Span::styled(format!(" {title}"), Style::default().fg(th.text).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(format!(" {path}"), Style::default().fg(th.dim))),
        Line::from(""),
        Line::from(Span::styled(
            " Removes the source file only. Mirrors in other harnesses and the team's sealed copies stay.",
            Style::default().fg(th.warn),
        )),
    ]);
    f.render_widget(body, parts[0]);
    hint(f, "ctrl+d delete · any other key cancels", th, parts[1]);
}

/// The one-time authenticator QR, on a white block so phones scan it.
fn draw_qr(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let Some((team, uri)) = app.qr_data() else { return };
    let Ok(code) = qrcodegen::QrCode::encode_text(&uri, qrcodegen::QrCodeEcc::Medium) else { return };
    let rows = crate::qr::rows(&code);
    let rows_h = rows.len() as u16;
    let width_chars = rows.first().map(|r| r.chars().count() as u16).unwrap_or(20);
    let outer = centered(width_chars + 2, rows_h + 5, area);
    let inner = dialog(f, "Add team to authenticator", th, outer);
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).split(inner);
    f.render_widget(
        Paragraph::new(format!("team '{team}' was created.").repeat(1))
            .style(Style::default().fg(th.warn)),
        parts[0],
    );
    let qr_lines: Vec<Line<'static>> = rows.into_iter()
        .map(|row| Line::from(Span::styled(row, Style::default().fg(QR_FG).bg(QR_BG))))
        .collect();
    f.render_widget(Paragraph::new(qr_lines), parts[1]);
    hint(f, "scan now · the secret is shown once · esc close", th, parts[2]);
}

/// Which rows the list area shows, so the cursor stays visible when the list is longer.
fn show_window(rows: &[Line<'static>], height: usize, cursor: usize) -> Vec<ListItem<'static>> {
    let offset = cursor.saturating_sub(height.saturating_sub(1));
    rows.iter().skip(offset).take(height).cloned().map(ListItem::new).collect()
}
