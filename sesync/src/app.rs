//! TUI application state. Ported from daemon/hs_tui.py ManageApp. Key events go
//! in, state and file operations come out. No terminal code here, so it is tested
//! with real files; the ratatui front end only draws and forwards keys.

use crate::lock::ClaimOutcome;
use crate::manage::{self, Group, Roots};
use crate::palette::{self};
use crate::state::State;
use crate::tui::{Key, Model};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Browse,
    Rename,
    /// Waiting for y/n before importing the selected team session.
    Confirm,
    /// Typing into the command list. Enter runs the highlighted command.
    Palette,
    /// Options for the selected local session (Enter on a session). Enter runs the highlighted
    /// option through the same event its normal key sends.
    SessionMenu,
    /// Choosing which harnesses to mirror the selected session into (multi-select).
    MirrorPick,
    /// Browsing the colour schemes; previewing, Enter applies, Esc reverts.
    Theme,
    /// Filling the join/create team dialog.
    Form,
    /// Showing the one-time authenticator QR after a team was created.
    Qr,
}

/// Which dialog the form serves: joining a team or starting one (admin side).
pub use crate::form::Kind;

/// A completed team dialog handed to the caller (the bin), which owns the network step.
#[derive(Debug, Clone)]
pub struct SetupRequest {
    pub kind: Kind,
    pub values: Vec<(String, String)>,
}

impl SetupRequest {
    pub fn value_of(&self, name: &str) -> String {
        self.values.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
}

/// What the form dialog shows to the drawing code.
#[derive(Debug, Clone)]
pub struct FormView {
    pub kind: Kind,
    /// Index of the current field in `fields`.
    pub cursor: usize,
    /// One entry per field: (label, value).
    pub fields: Vec<(String, String)>,
}

impl FormView {
    pub fn cursor_here(&self, name: &str) -> bool {
        self.fields.get(self.cursor).map_or(false, |(n, _)| n == name)
    }
}

/// Which list the main pane shows. The two tabs have separate cursors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Sessions,
    Team,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ev {
    CtrlD,
    /// Refresh the selected space's sessions from the team server.
    CtrlR,
    /// Quit, from any mode. Raw mode makes Ctrl+C an ordinary key, so quitting is keyed on q
    /// only; copied paths go to the clipboard on Ctrl+C.
    CtrlC,
    /// Open (or close) the command list, in any mode.
    CtrlP,
    Char(char),
    Backspace,
    Enter,
    /// Switch between the Sessions and the Team list.
    Tab,
    /// Open the colour scheme picker (ctrl+t).
    Theme,
    /// Open the join-a-team dialog (from the command list).
    OpenEnroll,
    /// Open the start-a-team dialog (from the command list).
    OpenRegister,
    Esc,
    /// Move the selection in the session list.
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

/// One row of the command list as the view draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct PaletteRow {
    pub title: &'static str,
    pub key: &'static str,
    /// Why the command cannot run right now; empty when it can.
    pub why: String,
    pub ok: bool,
    pub highlighted: bool,
}

pub struct App {
    roots: Roots,
    state: State,
    lock_dir: PathBuf,
    sesync_lock: PathBuf,
    groups: Vec<Group>,
    /// Team sessions from the last pull, as (id, bytes). Shown for display; written only when
    /// the user confirms an import.
    team: Vec<(String, Vec<u8>)>,
    /// Cursor into `team`, kept separate from the session cursor.
    team_index: usize,
    /// Ids imported in this run, in order. Recorded so the result can be shown.
    imported: Vec<String>,
    /// The space currently shown, if any, and the sessions last fetched for it.
    space: Option<String>,
    remote: Vec<crate::remote::SpaceSession>,
    /// Session ids already seen in an earlier refresh, so only new ones are reported.
    seen: std::collections::HashSet<String>,
    new_ids: Vec<String>,
    model: Model,
    mode: Mode,
    view: View,
    input: String,
    status: String,
    quit: bool,
    /// The team session shown when `i` was pressed. Only this one can be imported by `y`, so a
    /// pull that changes the list between the question and the answer cannot change the target.
    pending_import: Option<(String, Vec<u8>)>,
    /// Set by ctrl+r in browse mode. The caller performs the refresh and clears the request.
    refresh_requested: bool,
    /// Whether the caller found team-server configuration, so "Refresh team" can be offered.
    team_configured: bool,
    /// Filter text typed into the command list.
    palette_query: String,
    palette_cursor: usize,
    /// Cursor and picks for the per-session menu and the mirror-target picker.
    menu_cursor: usize,
    installed_targets: Vec<String>,
    pick_cursor: usize,
    pick_chosen: std::collections::BTreeSet<String>,
    /// The colour scheme name in use. Previewing while browsing is done by writing here.
    theme_name: String,
    /// The scheme to fall back to when the picker is escaped.
    theme_original: Option<String>,
    theme_cursor: usize,
    /// The open team dialog, if any, with its kind and fields.
    form: Option<crate::form::Form>,
    /// A completed dialog waiting for the caller to perform the network step.
    setup: Option<SetupRequest>,
    /// The authenticator URI to draw as a QR, with the team it belongs to.
    qr: Option<(String, String)>,
}

impl App {
    pub fn new(roots: Roots, state: State, lock_dir: PathBuf, sesync_lock: PathBuf) -> App {
        let groups = manage::list_groups(&roots, &state);
        let titles = groups.iter().map(|g| g.title.clone()).collect();
        App {
            model: Model::new(titles),
            roots, state, lock_dir, sesync_lock, groups,
            team: Vec::new(),
            team_index: 0,
            imported: Vec::new(),
            space: None,
            remote: Vec::new(),
            seen: std::collections::HashSet::new(),
            new_ids: Vec::new(),
            mode: Mode::Browse,
            view: View::Sessions,
            input: String::new(),
            status: String::new(),
            quit: false,
            pending_import: None,
            refresh_requested: false,
            team_configured: false,
            palette_query: String::new(),
            palette_cursor: 0,
            menu_cursor: 0,
            installed_targets: Vec::new(),
            pick_cursor: 0,
            pick_chosen: Default::default(),
            theme_name: crate::theme::DEFAULT.to_string(),
            theme_original: None,
            theme_cursor: 0,
            form: None,
            setup: None,
            qr: None,
        }
    }

    /// Whether ctrl+r asked for a team refresh since the last call. The caller performs it.
    pub fn take_refresh_request(&mut self) -> bool {
        std::mem::take(&mut self.refresh_requested)
    }

    /// Read-only view of the state the app is working from (for tests and status).
    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Which list the main pane shows.
    pub fn view(&self) -> View {
        self.view
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    pub fn rows(&self) -> &[Group] {
        &self.groups
    }

    /// Set the team sessions a pull returned. Display only; nothing is written by this.
    pub fn set_team_sessions(&mut self, sessions: Vec<(String, Vec<u8>)>) {
        self.team = sessions;
        self.clamp_team_index();
    }

    /// Record what a team pull returned: the verified sessions and how many the app refused.
    pub fn set_pull_result(&mut self, sessions: Vec<(String, Vec<u8>)>, refused: usize) {
        self.set_team_sessions(sessions);
        let shown = self.team.len();
        self.status = if refused == 0 {
            format!("team pull: {shown} verified")
        } else {
            format!("team pull: {shown} verified, {refused} refused; the authors are outside the verified member list")
        };
    }

    /// The team session ids imported during this run.
    pub fn imported(&self) -> Vec<String> {
        self.imported.clone()
    }

    /// Choose the space whose sessions Ctrl+R refreshes.
    pub fn select_space(&mut self, space: &str) {
        self.space = Some(space.to_string());
    }

    pub fn set_status(&mut self, msg: &str) {
        self.status = msg.to_string();
    }

    /// Sessions fetched from the server for the selected space, in server order.
    pub fn remote_rows(&self) -> &[crate::remote::SpaceSession] {
        &self.remote
    }

    /// Session ids that appeared since the previous refresh.
    pub fn new_remote_ids(&self) -> Vec<String> {
        self.new_ids.clone()
    }

    /// Ask the server for the selected space's sessions. On success the list is replaced and
    /// the sessions not seen before are recorded. On failure the last good list is kept.
    pub fn refresh_space(&mut self, remote: &dyn crate::remote::RemoteSessions, _now: Instant) {
        let Some(space) = self.space.clone() else {
            self.status = "no space selected".into();
            return;
        };
        match remote.list(&space) {
            Ok(sessions) => {
                self.new_ids = sessions.iter()
                    .filter(|s| !self.seen.contains(&s.id))
                    .map(|s| s.id.clone())
                    .collect();
                for s in &sessions {
                    self.seen.insert(s.id.clone());
                }
                self.status = format!("{} new session(s) in '{space}'", self.new_ids.len());
                self.remote = sessions;
            }
            Err(_) => {
                self.status = format!("server unreachable; showing last list for '{space}'");
            }
        }
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    fn selected(&self) -> Option<Group> {
        self.groups.get(self.model.selected()).cloned()
    }

    /// Whether a listed group is marked for the team. The view uses this to show the mark.
    pub fn is_marked(&self, g: &Group) -> bool {
        self.state.is_marked(&crate::manage::session_id_of(&g.key))
    }

    /// Whether the selected session is marked for the team.
    /// The sessions-list columns for one row, read from state at the current time.
    pub fn session_cols(&self, g: &Group) -> crate::session_cols::Cols {
        let id = manage::session_id_of(&g.key);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let last_change = self.state.sources.get(&g.key).map_or(0.0, |s| s.last_seen_change);
        crate::session_cols::columns(&crate::session_cols::Input {
            marked: self.state.is_marked(&id),
            pushed_at: self.state.team.get(&id).map(|t| t.pushed_at),
            last_change,
            now,
            quiet_secs: crate::team_pass::team_quiet_secs(),
            mirror_targets: g.mirrors.iter().map(|(t, _)| t.clone()).collect(),
        })
    }

    pub fn is_selected_marked(&self) -> bool {
        self.selected().map_or(false, |g| self.state.is_marked(&crate::manage::session_id_of(&g.key)))
    }

    /// Toggle the team mark on the selected session. Returns the new state, or None if nothing
    /// is selected. Only the session id is stored, never its contents.
    pub fn toggle_mark(&mut self) -> Option<bool> {
        let g = self.selected()?;
        let id = crate::manage::session_id_of(&g.key);
        let now_marked = if self.state.is_marked(&id) {
            self.state.unmark(&id);
            false
        } else {
            self.state.mark(&id);
            true
        };
        self.status = if now_marked {
            format!("marked '{}' for the team", g.title)
        } else {
            format!("unmarked '{}'", g.title)
        };
        Some(now_marked)
    }

    /// Index of the highlighted session row. The view uses it to draw the cursor.
    pub fn selected_index(&self) -> usize {
        self.model.selected()
    }

    /// Index of the highlighted team row, in `team_rows`.
    pub fn team_index(&self) -> usize {
        self.team_index
    }

    /// Whether the caller found team-server configuration for this run.
    pub fn team_configured(&self) -> bool {
        self.team_configured
    }

    pub fn set_team_configured(&mut self, yes: bool) {
        self.team_configured = yes;
    }

    /// The team list as the view shows it: one label per pulled session.
    pub fn team_rows(&self) -> Vec<String> {
        self.team.iter().map(|(id, bytes)| {
            let text = String::from_utf8_lossy(bytes);
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            let (title, found) = crate::titles::omp_title_from_lines(&lines);
            let title = if found && !title.trim().is_empty() { title } else { "untitled".to_string() };
            format!("{title} [{id}]")
        }).collect()
    }

    /// The command list as the view draws it, filtered by what has been typed.
    pub fn palette_rows(&self) -> Vec<PaletteRow> {
        let ctx = palette::Ctx {
            view: self.view,
            session_selected: self.selected().is_some(),
            team_selected: self.team.get(self.team_index).is_some(),
            team_configured: self.team_configured,
        };
        palette::filter(&self.palette_query).into_iter().enumerate().map(|(i, action)| {
            let (why, ok) = match palette::enabled(action, &ctx) {
                Ok(()) => (String::new(), true),
                Err(why) => (why.to_string(), false),
            };
            PaletteRow {
                title: palette::title_of(action),
                key: palette::key_of(action),
                why,
                ok,
                highlighted: i == self.palette_cursor,
            }
        }).collect()
    }

    /// Titles of the filtered commands, in shown order.
    pub fn palette_titles(&self) -> Vec<&'static str> {
        palette::filter(&self.palette_query).into_iter().map(|a| palette::title_of(a)).collect()
    }

    /// What has been typed into the command list so far.
    pub fn palette_query(&self) -> &str {
        &self.palette_query
    }

    fn clamp_team_index(&mut self) {
        self.team_index = self.team_index.min(self.team.len().saturating_sub(1));
    }

    /// Move the cursor by `delta` rows, clamped to the list. The caller has already cancelled any
    /// pending delete, so a moved cursor never deletes the row it was armed on.
    fn move_selection(&mut self, delta: isize) {
        let last = self.groups.len().saturating_sub(1) as isize;
        let next = (self.model.selected() as isize).saturating_add(delta).clamp(0, last);
        self.model.select(next as usize);
    }

    /// Move the team cursor by `delta`, clamped to the team list.
    fn move_team(&mut self, delta: isize) {
        let last = self.team.len().saturating_sub(1) as isize;
        let next = (self.team_index as isize).saturating_add(delta).clamp(0, last);
        self.team_index = next as usize;
    }

    /// Rebuild the list after a change on disk and keep the cursor on the same row index, so a
    /// delete or rename does not jump the cursor back to the top.
    fn reload(&mut self) {
        let keep = self.model.selected();
        self.groups = manage::list_groups(&self.roots, &self.state);
        self.model = Model::new(self.groups.iter().map(|g| g.title.clone()).collect());
        self.model.select(keep);
    }

    /// Cancel an armed delete. Any key other than ctrl+d does this, in every mode.
    fn disarm(&mut self, now: Instant) {
        if self.model.armed().is_some() {
            self.model.press(Key::Other, now);
            if self.status.starts_with("Press ctrl+d again") {
                self.status = "delete cancelled".into();
            }
        }
    }

    pub fn handle(&mut self, ev: Ev, now: Instant) {
        // ctrl+c is copy here, since raw mode lets us intercept it cleanly: it copies the
        // selected session's path and says how to quit; it does not quit.
        if ev == Ev::CtrlC && self.mode != Mode::Confirm {
            self.disarm(now);
            self.copy_selected_path();
            return;
        }
        // ctrl+t opens the colour scheme picker from anywhere except the questions and dialogs
        // that own the screen; inside them it stays out of the way.
        if ev == Ev::Theme && !matches!(self.mode, Mode::Confirm | Mode::Form | Mode::Qr) {
            self.disarm(now);
            self.open_theme();
            return;
        }
        // The first ctrl+d arms and only an immediate second one deletes. Anything typed in
        // between, in any mode, cancels the arm, so a stale arm cannot fire later.
        if ev != Ev::CtrlD {
            self.disarm(now);
        }
        match self.mode {
            Mode::Confirm => self.handle_confirm(ev),
            Mode::Rename => self.handle_rename(ev),
            Mode::Palette => self.handle_palette(ev, now),
            Mode::SessionMenu => self.handle_session_menu(ev, now),
            Mode::MirrorPick => self.handle_mirror_pick(ev),
            Mode::Theme => self.handle_theme(ev),
            Mode::Form => self.handle_form(ev),
            Mode::Qr => self.handle_qr(ev),
            Mode::Browse => self.handle_browse(ev, now),
        }
    }

    /// Ctrl+C's job: put the selected session's file on the clipboard and remind the user that
    /// quitting is done with q, not ctrl+c.
    fn copy_selected_path(&mut self) {
        let Some(g) = self.selected() else {
            self.status = "nothing selected; press q to quit".into();
            return;
        };
        let path = g.key.clone();
        match crate::clipboard::copy(&path) {
            Ok(()) => self.status = format!("copied {} to the clipboard; press q to quit", path),
            Err(e) => self.status = format!("could not copy to the clipboard: {e}; press q to quit"),
        }
    }

    fn handle_browse(&mut self, ev: Ev, now: Instant) {
        match ev {
            // The refresh needs the server client, which the caller supplies. This only asks for it.
            Ev::CtrlR => self.refresh_requested = true,
            Ev::OpenEnroll => self.open_form(Kind::Enroll, &[]),
            Ev::OpenRegister => self.open_form(Kind::Register, &[]),
            Ev::CtrlD => {
                if self.view != View::Sessions {
                    self.status = "delete works on local sessions only; press tab".into();
                    return;
                }
                let Some(g) = self.selected() else {
                    self.status = "nothing selected".into();
                    return;
                };
                if let Some(row) = self.model.press(Key::CtrlD, now) {
                    let _ = row;
                    self.delete_selected(&g);
                } else {
                    self.status = format!("Press ctrl+d again to DELETE '{}'. Any other key cancels.", g.title);
                }
            }
            Ev::Up | Ev::Char('k') => self.move_cursor(-1),
            Ev::Down | Ev::Char('j') => self.move_cursor(1),
            Ev::PageUp => self.move_cursor(-10),
            Ev::PageDown => self.move_cursor(10),
            Ev::Home => self.move_cursor(isize::MIN / 2),
            Ev::End => self.move_cursor(isize::MAX / 2),
            // Enter on a local session opens its options; ctrl+p always opens the command list.
            // Tab switches the pane.
            Ev::Enter if self.view == View::Sessions && self.selected().is_some() => self.open_session_menu(),
            Ev::Enter | Ev::CtrlP => self.open_palette(),
            Ev::Tab => self.switch_view(),
            Ev::Char('i') => match self.view {
                View::Sessions => self.status = "team sessions are on the Team tab; press tab".into(),
                View::Team => self.ask_import(),
            },
            Ev::Char('m') => {
                if self.view != View::Sessions {
                    self.status = "marking works on local sessions only; press tab".into();
                    return;
                }
                self.toggle_mark();
            }
            Ev::Char('r') => {
                if self.view != View::Sessions {
                    self.status = "renaming works on local sessions only; press tab".into();
                    return;
                }
                let Some(g) = self.selected() else { return };
                self.input = g.title.clone();
                self.mode = Mode::Rename;
                self.status = "Enter a new name; Esc cancels.".into();
            }
            Ev::Char('q') => {
                self.quit = true;
            }
            _ => {}
        }
    }

    /// Move whichever cursor belongs to the current list.
    fn move_cursor(&mut self, delta: isize) {
        match self.view {
            View::Sessions => self.move_selection(delta),
            View::Team => self.move_team(delta),
        }
    }

    /// Switch between the local list and the team list. Each keeps its own cursor.
    fn switch_view(&mut self) {
        self.view = match self.view {
            View::Sessions => View::Team,
            View::Team => View::Sessions,
        };
    }

    /// Ask before writing a team session into the harness. The question is about the row on
    /// screen right now; only that exact one survives a later refresh.
    fn ask_import(&mut self) {
        let Some(sel) = self.team.get(self.team_index).cloned() else {
            self.status = "no team session selected".into();
            return;
        };
        self.status = format!("import team session {} into your harness? y/n", sel.0);
        self.pending_import = Some(sel);
        self.mode = Mode::Confirm;
    }

    // ----- the command list ---------------------------------------------------------------

    /// Open the command list over the current view.
    fn open_palette(&mut self) {
        if self.mode == Mode::Palette {
            return;
        }
        self.palette_query.clear();
        self.palette_cursor = 0;
        self.mode = Mode::Palette;
        self.status = "Enter runs the highlighted command; Esc closes.".into();
    }


    /// The session menu options, in display order. Each runs the same event its normal key sends.
    pub fn session_menu_rows(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("Rename", "r"),
            ("Mark / unmark for team", "m"),
            ("Mirror to harness", ""),
            ("Delete", "ctrl+d"),
        ]
    }

    fn open_session_menu(&mut self) {
        self.menu_cursor = 0;
        self.mode = Mode::SessionMenu;
        self.status = "Enter runs the highlighted option; Esc closes.".into();
    }

    fn close_session_menu(&mut self) {
        self.menu_cursor = 0;
        self.mode = Mode::Browse;
    }

    fn handle_session_menu(&mut self, ev: Ev, now: Instant) {
        let rows = self.session_menu_rows().len();
        match ev {
            Ev::Esc => self.close_session_menu(),
            Ev::Up | Ev::Char('k') => self.menu_cursor = self.menu_cursor.saturating_sub(1),
            Ev::Down | Ev::Char('j') => self.menu_cursor = (self.menu_cursor + 1).min(rows - 1),
            Ev::Enter => {
                let choice = self.menu_cursor;
                self.close_session_menu();
                match choice {
                    0 => self.handle(Ev::Char('r'), now),
                    1 => self.handle(Ev::Char('m'), now),
                    2 => self.open_mirror_pick(),
                    3 => self.handle(Ev::CtrlD, now),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Open the mirror-target picker for the selected session. Targets are the harnesses installed
    /// here (passed in by the caller), minus the session's own format. Default: all selected.
    fn open_mirror_pick(&mut self) {
        let Some(g) = self.selected() else { return };
        let own = g.fmt.clone();
        self.pick_chosen = self.installed_targets.iter().filter(|t| **t != own).cloned().collect();
        self.pick_cursor = 0;
        self.mode = Mode::MirrorPick;
        self.status = "Space toggles a harness; Enter mirrors the session into the ticked ones; Esc cancels.".into();
    }

    fn handle_mirror_pick(&mut self, ev: Ev) {
        let targets = self.picker_targets();
        let rows = targets.len();
        match ev {
            Ev::Esc => {
                self.mode = Mode::Browse;
                self.status = "mirror cancelled".into();
            }
            Ev::Up | Ev::Char('k') => self.pick_cursor = self.pick_cursor.saturating_sub(1),
            Ev::Down | Ev::Char('j') => self.pick_cursor = (self.pick_cursor + 1).min(rows.saturating_sub(1)),
            Ev::Char(' ') => {
                if let Some(t) = targets.get(self.pick_cursor) {
                    if !self.pick_chosen.remove(t) {
                        self.pick_chosen.insert(t.clone());
                    }
                }
            }
            Ev::Enter => {
                let chosen: Vec<String> = self.pick_chosen.iter().cloned().collect();
                self.mode = Mode::Browse;
                self.save_chosen_targets(chosen);
            }
            _ => {}
        }
    }

    /// Harness targets offered by the picker: the installed harnesses except the session's own.
    pub fn picker_targets(&self) -> Vec<String> {
        let own = self.selected().map(|g| g.fmt.clone()).unwrap_or_default();
        self.installed_targets.iter().filter(|t| **t != own).cloned().collect()
    }

    /// The harnesses installed on this machine, as format names. Set by the caller at start-up.
    pub fn set_installed_targets(&mut self, targets: Vec<String>) {
        self.installed_targets = targets;
    }

    pub fn menu_cursor(&self) -> usize { self.menu_cursor }
    pub fn pick_cursor(&self) -> usize { self.pick_cursor }
    pub fn pick_is_chosen(&self, t: &str) -> bool { self.pick_chosen.contains(t) }

    /// Record the harnesses the user ticked for the selected session. The daemon reads these on its
    /// next pass and keeps those mirrors in sync; an empty choice removes the session's mirror
    /// intent, so nothing is written for it. The state is saved by the caller's normal save.
    fn save_chosen_targets(&mut self, chosen: Vec<String>) {
        let Some(g) = self.selected() else { return };
        let key = g.key.clone();
        let title = g.title.clone();
        // The session may not be in state yet (the daemon records sources on its first pass).
        // Create the entry now so the choice is never lost.
        let fmt = g.fmt.clone();
        let src = self.state.sources.entry(key.clone())
            .or_insert_with(|| crate::state::Source::new(&fmt, &key));
        src.chosen_targets = chosen.iter().cloned().collect();
        self.status = if chosen.is_empty() {
            format!("'{title}' will not be mirrored anywhere")
        } else {
            format!("'{title}' will be kept in sync with: {}", chosen.join(", "))
        };
    }

    /// Leave the command list without running anything.
    fn close_palette(&mut self) {
        self.palette_query.clear();
        self.palette_cursor = 0;
        self.mode = Mode::Browse;
    }

    /// Run the highlighted command. A command that cannot run here keeps the list open and
    /// the reason is shown; a usable one closes it and acts like the usual key was pressed.
    fn run_palette_choice(&mut self, now: Instant) {
        let filtered = palette::filter(&self.palette_query);
        let Some(&action) = filtered.get(self.palette_cursor) else { return };
        let ctx = palette::Ctx {
            view: self.view,
            session_selected: self.selected().is_some(),
            team_selected: self.team.get(self.team_index).is_some(),
            team_configured: self.team_configured,
        };
        if let Err(reason) = palette::enabled(action, &ctx) {
            self.status = format!("{} not available here: {reason}", palette::title_of(action));
            return;
        }
        let ev = palette::event_of(action);
        self.close_palette();
        self.handle(ev, now);
    }

    fn handle_palette(&mut self, ev: Ev, now: Instant) {
        match ev {
            Ev::Enter => self.run_palette_choice(now),
            Ev::CtrlP | Ev::Esc => self.close_palette(),
            Ev::Char(c) => {
                self.palette_query.push(c);
                self.palette_cursor = 0;
            }
            Ev::Backspace => {
                self.palette_query.pop();
                self.palette_cursor = 0;
            }
            Ev::Up | Ev::PageUp => self.move_palette(-1),
            Ev::Down | Ev::PageDown => self.move_palette(1),
            Ev::Home => self.move_palette(isize::MIN / 2),
            Ev::End => self.move_palette(isize::MAX / 2),
            _ => {}
        }
    }

    fn move_palette(&mut self, delta: isize) {
        let last = palette::filter(&self.palette_query).len().saturating_sub(1) as isize;
        self.palette_cursor = ((self.palette_cursor as isize).saturating_add(delta).clamp(0, last)) as usize;
    }

    // ----- the colour scheme picker --------------------------------------------------------

    /// The name of the colour scheme in use.
    pub fn theme_name(&self) -> &str {
        &self.theme_name
    }

    /// The scheme object the view uses.
    pub fn theme(&self) -> &'static crate::theme::Theme {
        crate::theme::by(&self.theme_name)
    }

    /// The scheme the cursor sits on and whether it is the one in use.
    pub fn theme_highlighted(&self) -> (String, bool) {
        let names = crate::theme::names_sorted();
        let name = names.get(self.theme_cursor).unwrap_or(&crate::theme::DEFAULT).to_string();
        let is_current = name == self.theme_name;
        (name, is_current)
    }

    /// Index of the scheme the cursor sits on in the picker.
    pub fn theme_cursor(&self) -> usize {
        self.theme_cursor
    }

    /// What the open form dialog shows, or None when it is closed.
    pub fn form_view(&self) -> Option<FormView> {
        let f = self.form.as_ref()?;
        Some(FormView {
            kind: f.kind,
            cursor: f.cursor,
            fields: f.entries().into_iter().map(|(n, v)| (n.to_string(), v.to_string())).collect(),
        })
    }

    /// The (team, uri) pair the QR dialog draws, or None.
    pub fn qr_data(&self) -> Option<(String, String)> {
        self.qr.clone()
    }

    /// Open the picker over whatever is on screen. The scheme in use is remembered until the
    /// dialog closes, so Esc can restore it.
    fn open_theme(&mut self) {
        if self.mode == Mode::Theme { return; }
        if self.theme_original.is_none() {
            self.theme_original = Some(self.theme_name.clone());
        }
        let names = crate::theme::names_sorted();
        self.theme_cursor = names.iter().position(|n| *n == self.theme_name).unwrap_or(0);
        self.mode = Mode::Theme;
        self.status = "up/down preview · enter apply · esc revert".into();
    }

    fn handle_theme(&mut self, ev: Ev) {
        let names = crate::theme::names_sorted();
        let last = names.len().saturating_sub(1);
        match ev {
            Ev::Enter => {
                let (name, _) = self.theme_highlighted();
                self.theme_name = name;
                self.theme_original = None;
                self.mode = Mode::Browse;
                self.status = format!("colour scheme now '{}'", self.theme_name);
            }
            Ev::Esc => {
                if let Some(original) = self.theme_original.take() {
                    self.theme_name = original;
                }
                self.mode = Mode::Browse;
                self.status = String::new();
            }
            Ev::Up | Ev::PageUp | Ev::Char('k') => {
                self.theme_cursor = self.theme_cursor.saturating_sub(1);
                self.theme_name = self.theme_highlighted().0;
            }
            Ev::Down | Ev::PageDown | Ev::Char('j') => {
                self.theme_cursor = (self.theme_cursor + 1).min(last);
                self.theme_name = self.theme_highlighted().0;
            }
            Ev::Home => {
                self.theme_cursor = 0;
                self.theme_name = self.theme_highlighted().0;
            }
            Ev::End => {
                self.theme_cursor = last;
                self.theme_name = self.theme_highlighted().0;
            }
            _ => {}
        }
    }

    // ----- the team dialogs ----------------------------------------------------------------

    /// Open the join/create dialog, optionally with values prefilled by label.
    pub fn open_form(&mut self, kind: Kind, prefill: &[(&str, &str)]) {
        if self.form.is_some() { return; }
        self.form = Some(crate::form::Form::new(kind, prefill));
        self.mode = Mode::Form;
        self.status = String::new();
    }

    /// A field's current value, or an empty string when the dialog is closed.
    pub fn form_value(&self, name: &str) -> String {
        self.form.as_ref().map_or(String::new(), |f| f.value(name).to_string())
    }

    /// The field the cursor sits on, by index into the dialog's field order.
    pub fn form_cursor(&self) -> usize {
        self.form.as_ref().map_or(0, |f| f.cursor)
    }

    pub fn set_form_value(&mut self, name: &str, value: &str) {
        if let Some(f) = self.form.as_mut() {
            f.set_value(name, value);
        }
    }

    /// Take the request a completed dialog produced, once.
    pub fn take_setup_request(&mut self) -> Option<SetupRequest> {
        std::mem::take(&mut self.setup)
    }

    /// What the caller reports after performing a request: the message the status shows.
    pub fn finish_setup(&mut self, result: Result<String, String>) {
        match result {
            Ok(msg) => self.status = msg,
            Err(e) => self.status = e,
        }
    }

    fn handle_form(&mut self, ev: Ev) {
        match ev {
            Ev::Tab => {
                if let Some(f) = self.form.as_mut() {
                    f.next();
                }
            }
            Ev::Down => {
                if let Some(f) = self.form.as_mut() {
                    f.next();
                }
            }
            Ev::Up => {
                if let Some(f) = self.form.as_mut() {
                    f.cursor = f.cursor.saturating_sub(1);
                }
            }
            Ev::Char(c) => {
                if let Some(f) = self.form.as_mut() {
                    f.push_char(c);
                }
            }
            Ev::Backspace => {
                if let Some(f) = self.form.as_mut() {
                    f.pop_char();
                }
            }
            Ev::Enter => self.submit_form(),
            Ev::Esc => {
                self.form = None;
                self.mode = Mode::Browse;
                self.status = String::new();
            }
            _ => {}
        }
    }

    fn submit_form(&mut self) {
        let Some(f) = self.form.as_ref() else { return };
        if let Err(reason) = f.validate() {
            self.status = reason;
            return;
        }
        let kind = f.kind;
        let values = f.collected();
        self.form = None;
        self.mode = Mode::Browse;
        self.status = match kind {
            Kind::Enroll => "joining the team…".into(),
            Kind::Register => "starting the team…".into(),
        };
        self.setup = Some(SetupRequest { kind, values });
    }

    // ----- the authenticator QR ------------------------------------------------------------

    /// Show the one-time authenticator QR for a freshly created team.
    pub fn show_qr(&mut self, team: &str, uri: &str) {
        self.qr = Some((team.to_string(), uri.to_string()));
        self.mode = Mode::Qr;
    }

    fn handle_qr(&mut self, ev: Ev) {
        match ev {
            Ev::Esc | Ev::Enter | Ev::Char(_) | Ev::Tab => {
                self.qr = None;
                self.mode = Mode::Browse;
                self.status = String::new();
            }
            _ => {}
        }
    }

    // ----- confirm and rename -------------------------------------------------------------

    /// Confirm or cancel the pending import. Only `y` writes; everything else cancels.
    fn handle_confirm(&mut self, ev: Ev) {
        match ev {
            Ev::Char('y') => {
                let pending = self.pending_import.take();
                self.mode = Mode::Browse;
                if let Some((id, bytes)) = pending {
                    // The harness lists only `<timestamp>_<uuid>.jsonl`. A team id that cannot
                    // be written in that form is refused, so no file is written that nothing shows.
                    let name = format!("{}_{}.jsonl", "2026-01-01T00-00-00-000Z", id);
                    if !crate::discovery::is_listed_session_name(&name) {
                        self.mode = Mode::Browse;
                        self.status = format!("not imported: team session id {id} is not a harness session id");
                        return;
                    }
                    let target = self.roots.omp.join(name);
                    match crate::team_import::import_confirmed(&target, &bytes, true) {
                        Ok(_) => {
                            self.imported.push(id.clone());
                            self.status = format!("imported team session {id}");
                        }
                        Err(e) => self.status = format!("not imported: {e:?}"),
                    }
                }
            }
            _ => {
                self.pending_import = None;
                self.mode = Mode::Browse;
                self.status = "import cancelled".into();
            }
        }
    }

    fn handle_rename(&mut self, ev: Ev) {
        match ev {
            Ev::Char(c) => self.input.push(c),
            Ev::Backspace => {
                self.input.pop();
            }
            Ev::Esc => {
                self.mode = Mode::Browse;
                self.input.clear();
                self.status = "rename cancelled".into();
            }
            Ev::Enter => {
                let Some(g) = self.selected() else {
                    self.mode = Mode::Browse;
                    return;
                };
                let title = self.input.clone();
                match manage::rename_group(&self.roots, &g, &title, &self.lock_dir, &self.sesync_lock) {
                    Ok(saved) => {
                        self.mode = Mode::Browse;
                        self.input.clear();
                        self.status = format!("renamed to '{saved}'");
                        self.reload();
                    }
                    Err(e) => {
                        // Invalid titles and refusals keep the input open so the user can fix it.
                        self.status = format!("refused: {e}");
                    }
                }
            }
            _ => {}
        }
    }

    fn delete_selected(&mut self, g: &Group) {
        match manage::delete_group(&self.roots, g, &self.lock_dir, &self.sesync_lock) {
            Ok(done) => {
                self.status = format!("deleted {} item(s) for '{}'", done.len(), g.title);
                self.reload();
            }
            Err(e) => self.status = format!("refused: {e}"),
        }
    }
}

#[allow(dead_code)]
fn _unused(_: ClaimOutcome<()>) {}
