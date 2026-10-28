//! Drawing for the manage TUI, with ratatui. Reads the App and writes a frame.
//! No state is kept here: the App owns everything the screen shows. Dialogs float centered
//! over the lists, in the style of the old Textual palette.

use crate::app::{App, Kind, Mode, View};
use crate::theme::Theme;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
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
        Mode::SharePick => draw_share_pick(f, app, th, area),
        Mode::LeavePick => draw_leave_pick(f, app, th, area),
        Mode::Members => draw_members(f, app, th, area),
        _ => {}
    }
    // Session text reaches every view (titles, transcripts, team rows, messages). A glyph whose
    // width terminals disagree on shifts the rest of its line on screen and leaves stale
    // characters behind, so none is sent: each is blanked in the finished frame, in place.
    scrub_unsafe_glyphs(f.buffer_mut());
}

/// Replace every glyph `session_cols::is_unsafe_glyph` refuses with what is left of its cell
/// when that is one narrow character, else a space. Cells keep their positions, so each line
/// keeps the layout ratatui computed.
fn scrub_unsafe_glyphs(buf: &mut ratatui::buffer::Buffer) {
    use crate::session_cols::{is_unsafe_glyph, text_width};
    for cell in buf.content.iter_mut() {
        if !cell.symbol().chars().any(is_unsafe_glyph) {
            continue;
        }
        let kept: String = cell.symbol().chars().filter(|c| !is_unsafe_glyph(*c)).collect();
        if text_width(&kept) == 1 {
            cell.set_symbol(&kept);
        } else {
            cell.set_symbol(" ");
        }
    }
}

fn draw_tab_bar(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let selected = Style::default().fg(th.text).bg(th.accent);
    let unselected = Style::default().fg(th.dim);
    let (s_style, t_style) = match app.view() {
        View::Sessions => (selected, unselected),
        View::Team => (unselected, selected),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    // The daemon saves the state file at the end of every pass (every SESYNC_POLL, 5 s by
    // default), so the file's age is the time since its last pass.
    let (daemon, daemon_color) = match app.state_written_at() {
        Some(t) if now - t < 60.0 => (format!("daemon: last pass {}", crate::session_cols::relative(now, t)), th.dim),
        Some(t) => (format!("daemon: no pass for {}", crate::session_cols::short_span((now - t).max(0.0) as u64)), th.warn),
        None => ("daemon: never ran".to_string(), th.warn),
    };
    let (ok, failing) = app.sync_counts();
    let bar = Line::from(vec![
        Span::styled(" Sessions ", s_style),
        Span::styled("│", Style::default().fg(th.dim)),
        Span::styled(format!(" Team ({}) ", app.team_rows().len()), t_style),
        Span::styled(format!("   {daemon}"), Style::default().fg(daemon_color)),
        Span::styled(format!(" · {ok} copies in place, {failing} failing"),
            Style::default().fg(if failing > 0 { th.error } else { th.dim })),
    ]);
    f.render_widget(Paragraph::new(bar), area);
}

/// Widths of the sessions list's right-hand columns, in cells.
const MIRRORS_W: usize = 16;
const SYNCED_W: usize = 10;
const TEAM_W: usize = 7;
const UPDATED_W: usize = 10;
/// The harness label is always this wide, so titles line up across harnesses.
const HARNESS_W: usize = 8;
/// From this screen width the details pane sits beside the list.
pub const DETAILS_MIN_WIDTH: u16 = 120;

/// The list pane, and the details pane beside it when the screen is wide enough.
fn sessions_panes(area: Rect) -> (Rect, Option<Rect>) {
    if area.width >= DETAILS_MIN_WIDTH {
        let parts = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).split(area);
        (parts[0], Some(parts[1]))
    } else {
        (area, None)
    }
}

fn draw_sessions(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    use crate::session_cols::{display_safe, fit};
    let (list_pane, details) = sessions_panes(area);
    if let Some(d) = details {
        draw_details(f, app, th, d);
    }
    // Every row starts with the list's 2-cell cursor gutter, the harness label and the 2-cell
    // team mark. TITLE takes what is left after the columns that fit; columns go from the least
    // useful (TEAM, then UPDATED, MIRRORS, SYNCED) until the title keeps MIN_TITLE cells.
    const LEAD: usize = 2 + HARNESS_W + 2;
    const MIN_TITLE: usize = 16;
    let pane_w = list_pane.width as usize;
    let mut cols: Vec<(&str, usize)> =
        vec![("MIRRORS", MIRRORS_W), ("SYNCED", SYNCED_W), ("TEAM", TEAM_W), ("UPDATED", UPDATED_W)];
    for drop in ["TEAM", "UPDATED", "MIRRORS", "SYNCED"] {
        let block: usize = cols.iter().map(|(_, w)| w + 1).sum();
        if pane_w >= LEAD + MIN_TITLE + block {
            break;
        }
        cols.retain(|(name, _)| *name != drop);
    }
    let block: usize = cols.iter().map(|(_, w)| w + 1).sum();
    let title_w = pane_w.saturating_sub(LEAD + block);

    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(list_pane);
    let mut head = format!("{}{}", " ".repeat(LEAD), fit("TITLE", title_w));
    for (name, w) in &cols {
        head.push(' ');
        head.push_str(&fit(name, *w));
    }
    f.render_widget(Paragraph::new(head).style(Style::default().fg(th.dim)), parts[0]);

    let dim = Style::default().fg(th.dim);
    let items: Vec<ListItem> = app
        .visible_rows()
        .iter()
        .map(|g| {
            // A session recorded in state but gone from disk is drawn dimmed, marked "(missing)".
            let title = if g.orphan {
                format!("{} (missing)", display_safe(&g.title))
            } else {
                display_safe(&g.title)
            };
            let mut spans = vec![
                Span::styled(fit(crate::harness::label(&g.fmt), HARNESS_W), dim),
                // A star marks a session the user chose for the team; unmarked rows show a space.
                Span::raw(if app.is_marked(g) { "* " } else { "  " }),
                Span::styled(fit(&title, title_w), if g.orphan { dim } else { Style::default() }),
            ];
            if !cols.is_empty() {
                let c = app.session_cols(g);
                for (name, w) in &cols {
                    spans.push(Span::raw(" "));
                    match *name {
                        "MIRRORS" => spans.extend(mirror_spans(&c.mirror_parts, *w, th)),
                        "SYNCED" => {
                            let color = match c.synced.as_str() {
                                "failing" => th.error,
                                "waiting" => th.warn,
                                _ => th.dim,
                            };
                            spans.push(Span::styled(fit(&c.synced, *w), Style::default().fg(color)));
                        }
                        "TEAM" => spans.push(Span::styled(fit(&c.team, *w), dim)),
                        _ => spans.push(Span::styled(fit(&c.updated, *w), dim)),
                    }
                }
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

/// The details pane: the highlighted session in full, and each mirror target with its own sync
/// state, so a failing target shows its error and when it happened.
fn draw_details(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    use crate::session_cols::{display_safe, relative, MirrorState};
    let block = Block::default().borders(Borders::LEFT).border_style(Style::default().fg(th.dim));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let dim = Style::default().fg(th.dim);
    let mut lines: Vec<Line> = Vec::new();
    let Some(g) = app.selected_row() else {
        lines.push(Line::from(Span::styled("No session selected.", dim)));
        f.render_widget(Paragraph::new(lines), inner);
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let c = app.session_cols(g);
    let title = if g.orphan {
        format!("{} (missing)", display_safe(&g.title))
    } else {
        display_safe(&g.title)
    };
    lines.push(Line::from(Span::styled(title, Style::default().add_modifier(Modifier::BOLD))));
    lines.push(Line::from(Span::styled(format!("Harness: {}", g.fmt), dim)));
    lines.push(Line::from(Span::styled(format!("File: {}", display_safe(&g.key)), dim)));
    lines.push(Line::from(Span::styled(format!("Team: {}    Updated: {}", c.team, c.updated), dim)));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Mirrors", Style::default().add_modifier(Modifier::BOLD))));
    if c.mirror_parts.is_empty() {
        lines.push(Line::from(Span::styled("  Not mirrored to any harness yet.", dim)));
    }
    let sync = app.state().sources.get(&g.key).map(|s| s.sync.clone()).unwrap_or_default();
    for (target, state) in &c.mirror_parts {
        let rec = sync.get(target);
        let problem = |e: &crate::state::TargetSync| {
            let mut s = format!("{}: {}", relative(now, e.problem_at), display_safe(&e.problem));
            if e.ok_at > 0.0 {
                s.push_str(&format!("; last good write {}", relative(now, e.ok_at)));
            }
            s
        };
        let (label, color, detail) = match state {
            MirrorState::Failing => ("failed", th.error, rec.map(problem).unwrap_or_default()),
            MirrorState::Busy => ("waiting", th.warn, rec.map(problem).unwrap_or_default()),
            MirrorState::Pending => ("waiting", th.warn, "not written yet".to_string()),
            MirrorState::Mirrored => match rec.filter(|e| e.ok_at > 0.0) {
                Some(e) => ("synced", th.text, relative(now, e.ok_at)),
                None => ("synced", th.text, "write time not recorded yet".to_string()),
            },
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<9}", crate::harness::label(target)), Style::default().fg(th.text)),
            Span::styled(format!("{label:<8}"), Style::default().fg(color)),
            Span::styled(detail, dim),
        ]));
    }
    // The details take the rows they need (a line that wraps gets a spare row); the last turns
    // fill the rest of the pane.
    let width = inner.width.max(1) as usize;
    let top: usize = lines.iter().map(|l| match l.width() {
        w if w <= width => 1,
        w => w.div_ceil(width) + 1,
    }).sum();
    let parts = Layout::vertical([Constraint::Length((top as u16).min(inner.height)), Constraint::Min(0)]).split(inner);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), parts[0]);
    draw_preview(f, app, th, parts[1]);
}

/// The highlighted session's last turns, newest at the bottom, below its details. The caller's
/// worker reads them and reads them again when the session changes, so a new message shows by
/// itself; shift+up/down or the wheel over the pane scroll back.
fn draw_preview(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    use crate::harness::Role;
    use crate::session_cols::{display_safe, relative};
    if area.height < 4 {
        return;
    }
    let p = app.preview();
    let dim = Style::default().fg(th.dim);
    // A blank row, the heading, then the turns.
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).split(area);
    f.render_widget(Paragraph::new(Line::from(vec![
        Span::styled("Last messages", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled("  shift+↑↓ scroll", dim),
    ])), rows[1]);
    let width = rows[2].width as usize;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let mut body: Vec<Line<'static>> = Vec::new();
    if p.messages.is_empty() {
        let note = match (&p.error, p.loading) {
            (Some(e), _) => display_safe(e),
            (None, true) => "reading…".to_string(),
            (None, false) => "No messages yet.".to_string(),
        };
        body.push(Line::from(Span::styled(format!("  {note}"), dim)));
    }
    for m in &p.messages {
        if !body.is_empty() {
            body.push(Line::from(""));
        }
        let (who, color) = match m.role {
            Role::User => ("user", th.accent),
            Role::Assistant => ("assistant", th.text),
        };
        let when = if m.at > 0.0 { format!(" · {}", relative(now, m.at)) } else { String::new() };
        body.push(Line::from(vec![
            Span::styled(who, Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(when, dim),
        ]));
        for raw in m.text.lines() {
            let safe = display_safe(raw);
            for piece in wrap_words(&safe, width.saturating_sub(2)) {
                body.push(Line::from(Span::styled(format!("  {piece}"), Style::default().fg(th.text))));
            }
        }
    }
    let height = rows[2].height as usize;
    let max = body.len().saturating_sub(height);
    p.max_scroll.set(max);
    let end = body.len() - p.scroll.min(max);
    let start = end.saturating_sub(height);
    f.render_widget(Paragraph::new(body[start..end].to_vec()), rows[2]);
}

/// Text broken into lines of at most `width` cells, at spaces where it can be; a word wider than
/// a line is cut.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in text.split(' ') {
        let w = crate::session_cols::text_width(word);
        if used > 0 && used + 1 + w <= width {
            line.push(' ');
            line.push_str(word);
            used += 1 + w;
            continue;
        }
        if used > 0 {
            out.push(std::mem::take(&mut line));
            used = 0;
        }
        for c in word.chars() {
            let cw = c.width().unwrap_or(0);
            if used + cw > width {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(c);
            used += cw;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// The area between the tab bar and the status rows: the same split as `draw`.
fn main_area(screen: Rect) -> Rect {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(screen)[1]
}

/// Whether a screen cell is on the details pane, so the wheel there scrolls the preview.
pub fn details_pane_at(app: &App, screen: Rect, col: u16, row: u16) -> bool {
    if !matches!(app.view(), View::Sessions) {
        return false;
    }
    let (_, details) = sessions_panes(main_area(screen));
    details.is_some_and(|d| col >= d.x && col < d.x + d.width && row >= d.y && row < d.y + d.height)
}

/// The index of the session row under a screen cell, for mouse clicks. None when the cell is on
/// the tab bar, the status rows, the details pane, the column header, or past the last row.
pub fn session_row_at(app: &App, screen: Rect, col: u16, row: u16) -> Option<usize> {
    if !matches!(app.view(), View::Sessions) {
        return None;
    }
    let (list_pane, _) = sessions_panes(main_area(screen));
    let in_cols = col >= list_pane.x && col < list_pane.x + list_pane.width;
    let first = list_pane.y as usize + 1; // row 0 of the pane is the column header
    let height = (list_pane.height as usize).saturating_sub(1);
    let r = row as usize;
    if !in_cols || r < first || r >= first + height {
        return None;
    }
    let offset = if height > 0 { app.selected_index().saturating_sub(height - 1) } else { 0 };
    let idx = offset + (r - first);
    (idx < app.visible_rows().len()).then_some(idx)
}

/// The MIRRORS cell, one span per target: dim when mirrored, warn while a chosen write is
/// still pending, error when it failed. The plain text is used when the cell must be cut.
fn mirror_spans(parts: &[(String, crate::session_cols::MirrorState)], width: usize, th: &Theme) -> Vec<Span<'static>> {
    use crate::session_cols::{fit, mirror_mark, text_width, MirrorState};
    if parts.is_empty() {
        return vec![Span::styled(fit("-", width), Style::default().fg(th.dim))];
    }
    let label = |t: &str, s: MirrorState| format!("{}{}", crate::harness::label(t), mirror_mark(s));
    let plain: String = parts.iter().map(|(t, s)| label(t, *s)).collect::<Vec<_>>().join(",");
    let used = text_width(&plain);
    if used > width {
        return vec![Span::styled(fit(&plain, width), Style::default().fg(th.dim))];
    }
    let mut spans = Vec::new();
    for (i, (t, s)) in parts.iter().enumerate() {
        let color = match s {
            MirrorState::Mirrored => th.dim,
            MirrorState::Pending | MirrorState::Busy => th.warn,
            MirrorState::Failing => th.error,
        };
        let sep = if i > 0 { "," } else { "" };
        spans.push(Span::styled(format!("{sep}{}", label(t, *s)), Style::default().fg(color)));
    }
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    spans
}

/// The Team pane: one line per joined team saying where this machine stands (with a dim line
/// under it when the user has something to do), then what a verified pull actually brought over.
fn draw_team(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    use crate::team_join::Standing;
    const LEAVE: &str = "ctrl+p, Leave a team";
    let cards = app.team_cards();
    // Teams this machine started but has not joined: it can manage them, but syncs nothing there
    // until it joins like any other device.
    let unjoined: Vec<&crate::team_cli::AdminTeam> = app.admin_teams().iter()
        .filter(|a| !cards.iter().any(|c| c.space == a.team))
        .collect();
    let mut head: Vec<Line> = Vec::new();
    for c in cards {
        let who = if c.member.is_empty() { String::new() } else { format!("  {}@{}", c.member, c.machine) };
        let server = if c.server.is_empty() { String::new() } else { format!("  {}", c.server) };
        let (standing, color, hint) = match &c.standing {
            // A listed device the server now refuses (revoked, or its member removed): say so,
            // and how to clear it, instead of reading as a healthy team with nothing in it.
            Standing::Listed { .. } if app.team_problem(&c.space).is_some() => (
                app.team_problem(&c.space).unwrap_or_default().to_string(), th.error,
                Some(format!("this machine is no longer in the team; to clear it: {LEAVE}"))),
            Standing::Listed { version: Some(v) } => (format!("listed (list v{v})"), th.text, None),
            Standing::Listed { version: None } => ("joined".to_string(), th.text, None),
            // The server answers a declined key as it answers a pending one, so the hint says
            // how to drop a join that will never be approved.
            Standing::Waiting { fingerprint } => (
                format!("waiting for the admin; tell them this fingerprint: {fingerprint}"), th.warn,
                Some(format!("if the admin declined it, it waits forever; to drop it: {LEAVE}"))),
            Standing::Unreadable(e) => (format!("credential unreadable: {e}"), th.error,
                Some(format!("to remove it: {LEAVE}"))),
        };
        head.push(Line::from(vec![
            Span::styled(format!("{}{who}{server}  ", crate::session_cols::display_safe(&c.space)), Style::default().fg(th.text)),
            Span::styled(standing, Style::default().fg(color)),
        ]));
        if let Some(hint) = hint {
            head.push(Line::from(Span::styled(format!("  {hint}"), Style::default().fg(th.dim))));
        }
    }
    for a in &unjoined {
        head.push(Line::from(vec![
            Span::styled(format!("{}  {}  ", crate::session_cols::display_safe(&a.team), a.server), Style::default().fg(th.text)),
            Span::styled("managed here; join it to sync this machine", Style::default().fg(th.dim)),
        ]));
    }
    // This machine's shared sessions first: what it is sending, and whether it has gone out yet.
    let shared = app.shared_from_here();
    const SHARED_SHOWN: usize = 5;
    if !shared.is_empty() {
        head.push(Line::from(Span::styled(format!("Shared from this machine ({})", shared.len()), Style::default().fg(th.text).add_modifier(Modifier::BOLD))));
        for s in shared.iter().take(SHARED_SHOWN) {
            let color = if s.state.contains("waiting") || s.state.contains("changed") { th.warn } else { th.dim };
            head.push(Line::from(vec![
                Span::styled(format!("  {}  ", crate::session_cols::fit(&s.harness, 8)), Style::default().fg(th.dim)),
                Span::styled(format!("{}  ", crate::session_cols::fit(&crate::session_cols::display_safe(&s.title), 40)), Style::default().fg(th.text)),
                Span::styled(s.state.clone(), Style::default().fg(color)),
            ]));
        }
        if shared.len() > SHARED_SHOWN {
            head.push(Line::from(Span::styled(format!("  and {} more (Sessions tab, f view:marked)", shared.len() - SHARED_SHOWN), Style::default().fg(th.dim))));
        }
    }
    let rows = app.team_rows();
    head.push(Line::from(vec![
        Span::styled("On the team  ", Style::default().fg(th.text).add_modifier(Modifier::BOLD)),
        Span::styled(app.team_header(std::time::Instant::now()), Style::default().fg(th.dim)),
    ]));
    if !rows.is_empty() {
        head.push(Line::from(Span::styled(
            format!("  {}  {}  {}  {}", crate::session_cols::fit("TITLE", TEAM_TITLE_W), crate::session_cols::fit("FROM", 12),
                crate::session_cols::fit("UPDATED", 9), "HERE"),
            Style::default().fg(th.dim),
        )));
    }
    let parts = Layout::vertical([Constraint::Length(head.len() as u16), Constraint::Min(1)]).split(area);
    f.render_widget(Paragraph::new(head), parts[0]);
    let area = parts[1];
    if rows.is_empty() {
        let msg = if app.pulling() {
            "Pulling the team…"
        } else if !cards.is_empty() || app.team_configured() {
            "No verified team sessions yet. Press ctrl+r to pull."
        } else {
            "Not in a team yet. Press ctrl+p and pick \"Join a team\", or \"Start a team\" on the admin machine."
        };
        f.render_widget(Paragraph::new(msg).style(Style::default().fg(th.dim)).wrap(Wrap { trim: true }), area);
        return;
    }
    let height = area.height as usize;
    let selected = Some(app.team_index());
    let offset = selected.unwrap_or(0).saturating_sub(height.saturating_sub(1));
    let mut list_state = ListState::default().with_offset(offset).with_selected(selected);
    let items: Vec<ListItem> = rows.iter().map(|r| {
        let title = format!("{} · {}…", crate::session_cols::display_safe(&r.title), r.short_id);
        ListItem::new(Line::from(vec![
            Span::styled(format!("{}  ", crate::session_cols::fit(&title, TEAM_TITLE_W)), Style::default().fg(th.text)),
            Span::styled(format!("{}  ", crate::session_cols::fit(r.from, 12)), Style::default().fg(if r.from == "this machine" { th.dim } else { th.text })),
            Span::styled(format!("{}  ", crate::session_cols::fit(&r.updated, 9)), Style::default().fg(th.dim)),
            Span::styled(if r.here { "yes" } else { "no (i imports it)" }, Style::default().fg(th.dim)),
        ]))
    }).collect();
    let list = List::new(items)
        .highlight_style(Style::default().bg(th.accent))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut list_state);
}

/// The width of the Team list's title column (title and short id).
const TEAM_TITLE_W: usize = 50;

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
            let mut spans = vec![Span::styled(format!(" {mark} {} {t}", crate::harness::icon(t)), style)];
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

/// The join/start dialog: one input row per field, the current one highlighted. An empty field
/// shows an example, dimmed; it is never a value, so Enter on an untouched dialog is refused.
fn draw_form(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let Some(view) = app.form_view() else { return };
    let title = match view.kind {
        Kind::Enroll => "Join a team",
        Kind::Register => "Start a team",
        Kind::Invite => "Invite a member",
    };
    let dim = Style::default().fg(th.dim);
    let warning = match view.kind {
        Kind::Register => "next: a QR code for your authenticator app, shown only once",
        Kind::Enroll | Kind::Invite => "",
    };
    // Below the fields: what the current field is for, the warning, then the keys.
    let mut bottom = vec![Line::from(Span::styled(format!(" {}", view.help), Style::default().fg(th.text)))];
    if !warning.is_empty() {
        bottom.push(Line::from(Span::styled(format!(" {warning}"), dim)));
    }
    bottom.push(Line::from(Span::styled(" tab next field · enter submit · esc cancel", dim)));
    let bottom_n = bottom.len() as u16;
    let outer = centered(76, (view.fields.len() as u16) + 5 + bottom_n, area);
    let inner = dialog(f, title, th, outer);
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(bottom_n)]).split(inner);
    // The label column fits the longest label plus a gap; a value longer than what is left shows
    // its end, so a long path being typed keeps its newest characters in view.
    let label_w = view.fields.iter().map(|(n, _)| n.chars().count()).max().unwrap_or(0) + 2;
    let value_w = (parts[0].width as usize).saturating_sub(1 + label_w);
    let example_style = dim.add_modifier(Modifier::ITALIC);
    let rows: Vec<Line<'static>> = view.fields.iter().enumerate().map(|(i, (name, value))| {
        let here = view.cursor_here(name);
        let label = format!("{name:<label_w$}");
        let label = Span::styled(format!(" {label}"), if here { on_accent(th, true) } else { dim });
        let value_style = if here { on_accent(th, false) } else { Style::default().fg(th.text) };
        let example = view.examples.get(i).copied().unwrap_or("");
        if value.is_empty() && !example.is_empty() {
            let cursor = if here { "_" } else { "" };
            let shown = crate::session_cols::fit(&format!("e.g. {example}"), value_w.saturating_sub(cursor.len()));
            Line::from(vec![label, Span::styled(cursor, value_style), Span::styled(shown, example_style)])
        } else {
            let rest = tail_fit(&if here { format!("{value}_") } else { value.clone() }, value_w);
            Line::from(vec![label, Span::styled(rest, value_style)])
        }
    }).collect();
    let cursor = view.cursor;
    let shown: Vec<ListItem> = show_window(&rows, parts[0].height as usize, cursor);
    f.render_widget(List::new(shown), parts[0]);
    f.render_widget(Paragraph::new(bottom), parts[1]);
}

/// The end of a text that fits `width` cells, behind "…" when the start had to go.
fn tail_fit(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if crate::session_cols::text_width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 1; // the "…"
    for c in text.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        kept.push(c);
        used += w;
    }
    kept.reverse();
    format!("…{}", kept.into_iter().collect::<String>())
}

/// The `?` overlay: the keys of the current tab, one line each.
fn draw_help(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let lines = app.help_lines();
    let height = lines.len() as u16 + 4;
    let inner = dialog(f, if app.view() == View::Team { "Keys: team" } else { "Keys: sessions" }, th,
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
    let inner = dialog(f, format!("Transcript: {name}").as_str(), th, centered(96, 24, area));
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

/// The share picker: one row per team this machine belongs to, plus any team the session is
/// still shared with that it no longer belongs to (so that share can be untied), space ticks.
/// A session can be shared into several teams; the ticks start from what it already is.
fn draw_share_pick(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let teams = app.share_picker_rows();
    let outer = centered(56, teams.len().max(1) as u16 + 4, area);
    let inner = dialog(f, "Share with teams", th, outer);
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    if teams.is_empty() {
        f.render_widget(Paragraph::new(" no team is configured here").style(Style::default().fg(th.dim)), parts[0]);
    } else {
        let cursor = app.share_cursor();
        let lines: Vec<ListItem> = teams.iter().enumerate().map(|(i, t)| {
            let mark = if app.share_is_picked(t) { "[x]" } else { "[ ]" };
            let on = i == cursor;
            let style = if on { on_accent(th, true) } else { Style::default().fg(th.text) };
            ListItem::new(Line::from(Span::styled(format!(" {mark} {t}"), style)))
        }).collect();
        f.render_widget(List::new(lines), parts[0]);
    }
    hint(f, "space tick · a all · n none · enter keep · esc cancel", th, parts[1]);
}

/// The leave picker: every team record on this machine (a membership, or a team managed here),
/// the cursor, and the row a first Enter armed.
fn draw_leave_pick(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let rows = app.leave_rows();
    // Wide enough for the longest row and the confirm marker; `centered` clamps it to the screen.
    let widest = rows.iter().map(|(label, _)| label.chars().count()).max().unwrap_or(0);
    let outer = centered((widest + 32).min(u16::MAX as usize) as u16, rows.len().max(1) as u16 + 4, area);
    let inner = dialog(f, "Leave a team", th, outer);
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let cursor = app.leave_cursor();
    let lines: Vec<ListItem> = rows.iter().enumerate().map(|(i, (label, _))| {
        let armed = app.leave_armed() == Some(i);
        let style = if i == cursor { on_accent(th, true) } else { Style::default().fg(th.text) };
        let text = format!(" {}{}", crate::session_cols::display_safe(label), if armed { "  · enter again to confirm" } else { "" });
        ListItem::new(Line::from(Span::styled(text, style)))
    }).collect();
    f.render_widget(List::new(lines), parts[0]);
    hint(f, "up/down move · enter twice leaves · esc cancel", th, parts[1]);
}

/// The members dialog: the team's list (listed devices, then requests waiting for approval),
/// the cursor on one row, and the prompt of a change waiting for its second press.
fn draw_members(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    let Some(m) = app.members() else { return };
    let outer = centered(80, m.rows.len().max(1) as u16 + 7, area);
    let inner = dialog(f, "Team members", th, outer);
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1),
                                  Constraint::Length(2), Constraint::Length(1)]).split(inner);
    let (listed, waiting) = m.counts();
    let version = m.version.map_or("?".to_string(), |v| v.to_string());
    let busy = if app.admin_busy() { " · working…" } else { "" };
    f.render_widget(Paragraph::new(format!(
        " team '{}' · list v{version} · {listed} listed, {waiting} waiting{busy}", m.team))
        .style(Style::default().fg(th.dim)), parts[0]);
    let lines: Vec<Line<'static>> = if m.rows.is_empty() {
        vec![Line::from(Span::styled(" nobody has joined this team yet", Style::default().fg(th.dim)))]
    } else {
        m.rows.iter().enumerate().map(|(i, row)| {
            let style = if i == m.cursor { on_accent(th, true) } else { Style::default().fg(th.text) };
            Line::from(Span::styled(member_row_text(row), style))
        }).collect()
    };
    f.render_widget(List::new(show_window(&lines, parts[1].height as usize, m.cursor)), parts[1]);
    let prompt = m.prompt().unwrap_or("");
    f.render_widget(Paragraph::new(format!(" {prompt}")).style(Style::default().fg(th.warn)).wrap(Wrap { trim: false }), parts[2]);
    hint(f, "a approve · d refuse · r revoke · m remove · t next team · esc close", th, parts[3]);
}

/// One members-dialog row: a listed device, or a request waiting for approval with the
/// fingerprint the admin compares with the member's screen. Names are cut to their columns so a
/// row fits an 80-column terminal.
fn member_row_text(row: &crate::members::Row) -> String {
    use crate::members::Row;
    use crate::session_cols::{display_safe, fit};
    let cols = |who: &str, machine: &str| (fit(&display_safe(who), 12), fit(&display_safe(machine), 14));
    match row {
        Row::Listed(d) => {
            let (who, machine) = cols(d.member.as_deref().unwrap_or("-"), d.machine.as_deref().unwrap_or("-"));
            let admin = if d.admin { "  admin key" } else { "" };
            format!(" listed   {who} {machine} {}{admin}", d.fingerprint)
        }
        Row::Waiting(p) => {
            let (who, machine) = cols(&p.member, &p.machine);
            format!(" waiting  {who} {machine} {}  asked {} ago", p.fingerprint, crate::team_cli::age(p.age_secs))
        }
    }
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

/// The one-time authenticator code on a screen of its own, so it fits small terminals: the code
/// black on white whatever the theme (scanners need dark on light), or, on a terminal without
/// colours, its light modules painted in the terminal's own foreground (see `qr::Ink`); with the
/// standard 4-module quiet zone when there is room and 2 when that is what makes it fit; then the
/// setup key in groups of four for an app that is typed into. A terminal too small for either
/// gets no code (a clipped QR does not scan): it says the size needed and still shows the key.
fn draw_qr(f: &mut Frame, app: &App, th: &Theme, area: Rect) {
    use crate::qr::{compact_uri, grouped, rows_inked, secret_of, Ink, ECC, QUIET, QUIET_MIN};
    let Some((team, uri)) = app.qr_data() else { return };
    let uri = compact_uri(&uri);
    let Ok(code) = qrcodegen::QrCode::encode_text(&uri, ECC) else { return };
    f.render_widget(Clear, area);
    f.render_widget(Block::default().style(Style::default().bg(th.bg)), area);
    // The title above the code, the key and the hint below it.
    const TEXT_ROWS: u16 = 3;
    let size = |rows: &[String]| (rows.first().map_or(0, |r| r.chars().count()) as u16, rows.len() as u16 + TEXT_ROWS);
    let (ink, qr_style) = if app.monochrome() {
        (Ink::Light, Style::default())
    } else {
        (Ink::Dark, Style::default().fg(QR_FG).bg(QR_BG))
    };
    let fitting = [QUIET, QUIET_MIN].into_iter()
        .map(|q| rows_inked(&code, q, ink))
        .find(|rows| {
            let (w, h) = size(rows);
            w <= area.width && h <= area.height
        });
    let center = ratatui::layout::Alignment::Center;
    let title = Line::from(Span::styled(
        format!("Add team '{}' to your authenticator app", crate::session_cols::display_safe(&team)),
        Style::default().fg(th.warn).add_modifier(Modifier::BOLD)));
    let key = Line::from(vec![
        Span::styled("setup key  ", Style::default().fg(th.dim)),
        Span::styled(grouped(secret_of(&uri).unwrap_or("")), Style::default().fg(th.text).add_modifier(Modifier::BOLD)),
    ]);
    let Some(rows) = fitting else {
        let (w, h) = size(&rows_inked(&code, QUIET_MIN, ink));
        let lines = vec![
            title,
            Line::from(""),
            Line::from(Span::styled(format!(
                "This terminal is {}x{}; the code needs {w}x{h}. Enlarge the window to scan it, or type the key.",
                area.width, area.height), Style::default().fg(th.dim))),
            Line::from(""),
            key,
            Line::from(""),
            Line::from(Span::styled("the key is shown once · esc closes", Style::default().fg(th.dim))),
        ];
        let y = area.y + area.height.saturating_sub(lines.len() as u16) / 2;
        f.render_widget(Paragraph::new(lines).alignment(center).wrap(Wrap { trim: true }),
            Rect { y, height: area.height - (y - area.y), ..area });
        return;
    };
    let (w, h) = size(&rows);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    let qr_h = rows.len() as u16;
    f.render_widget(Paragraph::new(title).alignment(center), Rect { y, height: 1, ..area });
    let qr: Vec<Line<'static>> = rows.into_iter()
        .map(|row| Line::from(Span::styled(row, qr_style)))
        .collect();
    f.render_widget(Paragraph::new(qr), Rect { x, y: y + 1, width: w, height: qr_h });
    f.render_widget(Paragraph::new(key).alignment(center), Rect { y: y + 1 + qr_h, height: 1, ..area });
    f.render_widget(Paragraph::new(Line::from(Span::styled(
        "scan it now: the key is shown once · esc closes", Style::default().fg(th.dim)))).alignment(center),
        Rect { y: y + 2 + qr_h, height: 1, ..area });
}

/// Which rows the list area shows, so the cursor stays visible when the list is longer.
fn show_window(rows: &[Line<'static>], height: usize, cursor: usize) -> Vec<ListItem<'static>> {
    let offset = cursor.saturating_sub(height.saturating_sub(1));
    rows.iter().skip(offset).take(height).cloned().map(ListItem::new).collect()
}
