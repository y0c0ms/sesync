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

/// How long an informational status message stays before the view fades it. Failures stay.
pub const STATUS_TTL: std::time::Duration = std::time::Duration::from_secs(25);

/// TUI preferences that survive runs: the colour scheme and the last tab. They live in their
/// own small file under the configuration directory, never in the daemon's state file: the
/// daemon and the app both write that one, and a taste must not cost a merge.
pub mod prefs {
    use super::{View};
    use serde_json::json;
    use std::path::Path;

    /// What was remembered: the theme name used last (validated on load) and the last tab.
    #[derive(Debug, Default, Clone, PartialEq, Eq)]
    pub struct Prefs {
        pub theme: Option<String>,
        pub view: Option<View>,
    }

    pub fn load(config_dir: &Path) -> Prefs {
        let file = config_dir.join("tui.json");
        let Ok(text) = std::fs::read_to_string(&file) else { return Prefs::default() };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Prefs::default() };
        Prefs {
            theme: v.get("theme").and_then(|t| t.as_str()).map(String::from),
            view: match v.get("view").and_then(|t| t.as_str()) {
                Some("team") => Some(View::Team),
                Some("sessions") => Some(View::Sessions),
                _ => None,
            },
        }
    }

    pub fn save(config_dir: &Path, theme_name: &str, view: View) -> std::io::Result<()> {
        let file = config_dir.join("tui.json");
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let v = json!({
            "theme": theme_name,
            "view": match view { View::Sessions => "sessions", View::Team => "team" },
        });
        std::fs::write(&file, serde_json::to_vec_pretty(&v)?)?;
        Ok(())
    }
}

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
    /// Typing a session-list filter at the status line. Enter keeps it, Esc clears it.
    Filter,
    /// The `?` overlay naming the keys for the current tab.
    Help,
    /// Reading the selected session's file inside the TUI, scrollable.
    Transcript,
    /// The delete confirmation: names what goes and what stays, confirmed by ctrl+d/enter/y.
    DeleteConfirm,
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

/// The `f` cycle on the Sessions tab: which subset of sessions is listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterScope {
    All,
    Marked,
    Mirrored,
    Failing,
}

impl FilterScope {
    fn next(self) -> FilterScope {
        match self {
            FilterScope::All => FilterScope::Marked,
            FilterScope::Marked => FilterScope::Mirrored,
            FilterScope::Mirrored => FilterScope::Failing,
            FilterScope::Failing => FilterScope::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FilterScope::All => "all",
            FilterScope::Marked => "marked",
            FilterScope::Mirrored => "mirrored",
            FilterScope::Failing => "failing",
        }
    }
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
    /// Titles already read, keyed by file size and mtime, so a re-list does not read
    /// unchanged files again. See [`crate::manage::TitleCache`].
    titles: crate::manage::TitleCache,
    /// Set when a key changed a field the app owns (a mark or a mirror choice); the
    /// caller saves state only then, not on every keypress.
    user_dirty: bool,
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
    /// The message at the status line, its severity, and when it was set. All writes go through
    /// `say`/`fail`, so the view can fade old news and keep failures visible.
    status_msg: String,
    status_error: bool,
    status_at: Instant,
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
    /// The last team pull this run: when it finished, how many rows it verified and refused.
    last_pull: Option<(Instant, usize, usize)>,
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
    /// Indices into `groups` passing the filter (scope and typed text), in list order.
    /// Everything the list pane shows goes through this.
    visible: Vec<usize>,
    /// The typed filter text from `/`, matched the way the command list matches.
    filter_text: String,
    /// The `f` cycle: which subset of sessions is listed.
    filter_scope: FilterScope,
    /// The open transcript view: its path, the lines shown and the scroll offset.
    /// Only the selected session's file is ever opened, and it is read alone.
    transcript: Option<TranscriptView>,
    /// Set when the taste changed (theme, tab) so the caller can persist `prefs`.
    prefs_dirty: bool,
    /// Whether a team pull is running in the background. Only one at a time.
    pulling: bool,
    /// The session key behind the open delete confirmation.
    armed_delete: Option<String>,
}

/// What the transcript view holds: the file it came from, its lines and the scroll.
/// Lines are capped: the tail is kept, the head is replaced by a marker.
pub struct TranscriptView {
    pub path: String,
    pub lines: Vec<String>,
    pub scroll: usize,
}

/// How many lines a transcript shows before the head is cut.
pub const TRANSCRIPT_MAX_LINES: usize = 4000;

impl App {
    pub fn new(roots: Roots, state: State, lock_dir: PathBuf, sesync_lock: PathBuf) -> App {
        let mut titles = crate::manage::TitleCache::default();
        let groups = manage::list_groups_cached(&roots, &state, &mut titles);
        let visible: Vec<usize> = (0..groups.len()).collect();
        let model = Model::new(groups.iter().map(|g| g.title.clone()).collect());
        App {
            model,
            roots, state, lock_dir, sesync_lock, groups, titles, user_dirty: false,
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
            status_msg: String::new(),
            status_error: false,
            status_at: Instant::now(),
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
            last_pull: None,
            visible,
            filter_text: String::new(),
            filter_scope: FilterScope::All,
            transcript: None,
            prefs_dirty: false,
            pulling: false,
            armed_delete: None,
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
        &self.status_msg
    }

    /// Whether the current status is a failure (drawn in the error colour, never faded).
    pub fn status_is_error(&self) -> bool {
        self.status_error
    }

    /// Say something informational at the status line.
    fn say(&mut self, msg: impl Into<String>) {
        self.status_msg = msg.into();
        self.status_error = false;
        self.status_at = Instant::now();
    }

    /// Report a failure at the status line; the view draws it in the error colour and it stays
    /// until it is replaced, where an informational message fades on its own.
    fn fail(&mut self, msg: impl Into<String>) {
        self.status_msg = msg.into();
        self.status_error = true;
        self.status_at = Instant::now();
    }

    /// The status line as the view should draw it: `(text, is_error)`, or None when there is
    /// nothing to say. Informational messages fade after `STATUS_TTL`; failures do not.
    pub fn status_line(&self, now: Instant) -> Option<(&str, bool)> {
        if self.status_msg.is_empty() {
            return None;
        }
        if !self.status_error && now.saturating_duration_since(self.status_at) > STATUS_TTL {
            return None;
        }
        Some((&self.status_msg, self.status_error))
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

    /// Mark that a pull started. False (and no change) when one is already running: pulls are
    /// looked at one at a time.
    pub fn begin_pull(&mut self) -> bool {
        if self.pulling {
            self.say("a team pull is already running");
            return false;
        }
        self.pulling = true;
        self.say("pulling the team…");
        true
    }

    /// Raise the app's copy of the conversation floors to what this pull accepted, so the next
    /// pull never accepts a record below what this one verified. In memory only: the daemon
    /// owns the persisted floors, and `save_user` does not write these.
    pub fn absorb_pull_floors(&mut self, accepted: &std::collections::BTreeMap<String, u64>) {
        for (conv, seq) in accepted {
            let cur = self.state.team_seen.entry(conv.clone()).or_insert(0);
            if *seq > *cur {
                *cur = *seq;
            }
        }
    }

    /// A pull ended without a result (its worker failed or was lost). Stop the wait, say so,
    /// and leave the list as it was.
    pub fn pull_failed(&mut self) {
        self.pulling = false;
        self.fail("the team pull did not finish");
    }

    /// Whether a pull is in flight.
    pub fn pulling(&self) -> bool {
        self.pulling
    }

    /// Record what a team pull returned: the verified sessions and how many the app refused.
    pub fn set_pull_result(&mut self, sessions: Vec<(String, Vec<u8>)>, refused: usize) {
        self.set_team_sessions(sessions);
        let shown = self.team.len();
        self.pulling = false;
        self.last_pull = Some((Instant::now(), shown, refused));
        self.say(if refused == 0 {
            format!("team pull: {shown} verified")
        } else {
            format!("team pull: {shown} verified, {refused} refused; the authors are outside the verified member list")
        });
    }

    /// One dim line above the team list: how many rows it shows, when this run last pulled, and
    /// how many records the pull refused. So an empty list is never mistaken for an empty team.
    pub fn team_header(&self, now: Instant) -> String {
        let mut s = format!("{} session(s)", self.team.len());
        match self.last_pull {
            Some((at, _, refused)) => {
                s.push_str(&format!(" · pulled {}s ago", now.saturating_duration_since(at).as_secs()));
                if refused > 0 {
                    s.push_str(&format!(" · {refused} refused"));
                }
            }
            None => s.push_str(" · not pulled yet this run"),
        }
        s
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
        self.say(msg);
    }

    /// A caller-reported problem (missing configuration, network failure): drawn in the error
    /// colour, and unlike an informational message it does not fade on its own.
    pub fn set_error(&mut self, msg: &str) {
        self.fail(msg);
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
            self.say("no space selected");
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
                self.say(format!("{} new session(s) in '{space}'", self.new_ids.len()));
                self.remote = sessions;
            }
            Err(_) => {
                self.fail(format!("server unreachable; showing last list for '{space}'"));
            }
        }
    }

    /// The key hint for the footer, by mode and view, so every screen says what its keys do.
    /// Kept here (not in the drawing code) so the strings are testable.
    pub fn footer(&self) -> String {
        match self.mode {
            Mode::Rename => "enter save · esc cancel".to_string(),
            Mode::Confirm => "y import · any other key cancels".to_string(),
            Mode::Palette => "type to filter · up/down move · enter run · ctrl+p/esc close".to_string(),
            Mode::SessionMenu => "enter run · esc close".to_string(),
            Mode::MirrorPick => "space tick · a all · n none · enter mirror · esc cancel".to_string(),
            Mode::Theme => "up/down preview · enter apply · esc revert".to_string(),
            Mode::Form => "tab next field · enter submit · esc cancel".to_string(),
            Mode::Qr => "scan now · the secret is shown once · esc close".to_string(),
            Mode::Filter => "type to filter · enter keep · esc clear".to_string(),
            Mode::Help => "esc close".to_string(),
            Mode::Transcript => "j/k scroll · esc close".to_string(),
            Mode::DeleteConfirm => "ctrl+d delete · any other key cancels".to_string(),
            Mode::Browse => match self.view {
                View::Sessions => {
                    let view_key = match self.filter_scope {
                        FilterScope::All => "f view".to_string(),
                        s => format!("f view:{}", s.label()),
                    };
                    let mut f = format!("enter menu · m mark · / filter · {view_key} · ctrl+r relist · ? help · ctrl+p cmds · tab team · q quit");
                    if self.has_mirror_flags() {
                        f.push_str("  · ? pending  ! failing");
                    }
                    f
                }
                View::Team => "enter import · ctrl+r pull · ? help · ctrl+p cmds · tab sessions · q quit".to_string(),
            },
        }
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    fn selected(&self) -> Option<Group> {
        self.visible.get(self.model.selected()).and_then(|i| self.groups.get(*i)).cloned()
    }

    /// Whether a group passes the current scope and typed filter.
    fn passes(&self, g: &Group) -> bool {
        let scope_ok = match self.filter_scope {
            FilterScope::All => true,
            FilterScope::Marked => self.state.is_marked(&self.state.team_id_of(&g.key)),
            FilterScope::Mirrored => {
                !g.mirrors.is_empty()
                    || self.state.sources.get(&g.key).map_or(false, |s| !s.chosen_targets.is_empty())
            }
            // Failing means "asked to mirror and the write failed". A stale failure counter on
            // a session with nothing chosen and no mirrors cannot fail anything.
            FilterScope::Failing => self.state.sources.get(&g.key).map_or(false, |s| {
                s.failures > 0 && (!s.chosen_targets.is_empty() || !s.mirrors.is_empty())
            }),
        };
        scope_ok && palette::fuzzy_matches(&g.title, &self.filter_text)
    }

    /// Recompute which groups the list shows. Cheap (no file reads) and called after any
    /// filter change and after every re-list.
    fn recompute_visible(&mut self) {
        self.visible = (0..self.groups.len()).filter(|i| self.passes(&self.groups[*i])).collect();
    }

    /// Rebuild the cursor model from `visible`, trying to keep the row with this key selected.
    fn rebuild_model_keep(&mut self, keep: Option<String>) {
        let index = keep
            .and_then(|k| self.visible.iter().position(|i| self.groups[*i].key == k))
            .unwrap_or(0);
        self.model = Model::new(self.visible.iter().map(|i| self.groups[*i].title.clone()).collect());
        self.model.select(index.min(self.visible.len().saturating_sub(1)));
    }

    /// The groups the list pane draws, in list order, after the filter.
    pub fn visible_rows(&self) -> Vec<&Group> {
        self.visible.iter().filter_map(|i| self.groups.get(*i)).collect()
    }

    /// A status message about the filter: how many rows of how many are shown.
    fn filter_status(&mut self) {
        let (shown, total) = (self.visible.len(), self.groups.len());
        self.say(if self.filter_text.is_empty() {
            format!("showing {shown} of {total} session(s)")
        } else {
            format!("filter '{}' — {shown} of {total} session(s)", self.filter_text)
        });
    }

    /// Whether a listed group is marked for the team. The view uses this to show the mark.
    pub fn is_marked(&self, g: &Group) -> bool {
        self.state.is_marked(&self.state.team_id_of(&g.key))
    }

    /// True when any local session has a chosen mirror still pending or failing, so the
    /// footer carries the marker legend only when it means something.
    pub fn has_mirror_flags(&self) -> bool {
        self.groups.iter().any(|g| {
            self.state.sources.get(&g.key).map_or(false, |s| {
                s.chosen_targets.iter().any(|t| !s.mirrors.contains_key(t))
            })
        })
    }

    /// Whether the selected session is marked for the team.
    /// The sessions-list columns for one row, read from state at the current time.
    pub fn session_cols(&self, g: &Group) -> crate::session_cols::Cols {
        let id = self.state.team_id_of(&g.key);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let last_change = self.state.sources.get(&g.key).map_or(0.0, |s| s.last_seen_change);
        crate::session_cols::columns(&crate::session_cols::Input {
            marked: self.state.is_marked(&id),
            pushed_at: self.state.team.get(&id).map(|t| t.pushed_at),
            last_change,
            now,
            quiet_secs: crate::team_pass::team_quiet_secs(),
            // A chosen target with no mirror yet shows with a marker so the user can see the
            // write is still pending ("?") or has been failing ("!").
            mirror_targets: {
                use crate::session_cols::MirrorState;
                let src = self.state.sources.get(&g.key);
                let failures = src.map_or(0, |s| s.failures);
                let mut targets: Vec<(String, MirrorState)> =
                    g.mirrors.iter().map(|(t, _)| (t.clone(), MirrorState::Mirrored)).collect();
                if let Some(s) = src {
                    for t in &s.chosen_targets {
                        if !s.mirrors.contains_key(t) {
                            targets.push((t.clone(), if failures > 0 { MirrorState::Failing } else { MirrorState::Pending }));
                        }
                    }
                }
                targets
            },
        })
    }

    pub fn is_selected_marked(&self) -> bool {
        self.selected().map_or(false, |g| self.state.is_marked(&self.state.team_id_of(&g.key)))
    }

    /// Toggle the team mark on the selected session. Returns the new state, or None if nothing
    /// is selected. Only the session id is stored, never its contents.
    pub fn toggle_mark(&mut self) -> Option<bool> {
        let g = self.selected()?;
        let id = self.state.team_id_of(&g.key);
        let now_marked = if self.state.is_marked(&id) {
            self.state.unmark(&id);
            false
        } else {
            self.state.mark(&id);
            true
        };
        self.say(if now_marked {
            format!("marked '{}' for the team", g.title)
        } else {
            format!("unmarked '{}'", g.title)
        });
        // In the marked-only view, a row that no longer qualifies leaves the list at once.
        if self.filter_scope == FilterScope::Marked {
            self.recompute_visible();
            self.rebuild_model_keep(Some(g.key.clone()));
        }
        self.user_dirty = true;
        Some(now_marked)
    }

    /// True once the user changed a field the app owns (a mark or a mirror choice), and
    /// clears the flag. The caller saves state only when this returns true.
    pub fn take_user_dirty(&mut self) -> bool {
        std::mem::take(&mut self.user_dirty)
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

    /// The team list as the view shows it: one label per pulled session, titled and with a short
    /// conversation id. The full id is still what actions use; the row only has to be recognizable.
    pub fn team_rows(&self) -> Vec<String> {
        self.team.iter().map(|(id, bytes)| {
            let text = String::from_utf8_lossy(bytes);
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            let (title, found) = crate::titles::omp_title_from_lines(&lines);
            let title = if found && !title.trim().is_empty() { title } else { "untitled".to_string() };
            let short: String = id.chars().take(8).collect();
            format!("{title} · {short}…")
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
        let last = self.visible.len().saturating_sub(1) as isize;
        let next = (self.model.selected() as isize).saturating_add(delta).clamp(0, last);
        self.model.select(next as usize);
    }

    /// Move the team cursor by `delta`, clamped to the team list.
    fn move_team(&mut self, delta: isize) {
        let last = self.team.len().saturating_sub(1) as isize;
        let next = (self.team_index as isize).saturating_add(delta).clamp(0, last);
        self.team_index = next as usize;
    }

    /// Ctrl+R on the local tab: re-read every session file and keep the cursor on the same
    /// session, so a title changed in the harness itself (Claude's /rename, omp, Pi) is visible
    /// immediately without waiting for the daemon.
    pub fn refresh_local(&mut self) {
        self.refresh_rows();
        self.say(format!("{} session(s)", self.groups.len()));
    }

    /// Take the daemon-owned parts of a state file into this app: mirror records, scan stamps,
    /// team floors and subagents. The user-owned parts (marks, chosen targets) stay local, so
    /// a mirror the daemon just wrote becomes visible without a restart.
    pub fn absorb_daemon_state(&mut self, disk: &State) {
        self.state.mirror_keys = disk.mirror_keys.clone();
        self.state.subagents = disk.subagents.clone();
        self.state.team = disk.team.clone();
        self.state.team_seen = disk.team_seen.clone();
        for (key, dsrc) in &disk.sources {
            if let Some(s) = self.state.sources.get_mut(key) {
                s.last_seen_change = dsrc.last_seen_change;
                s.last_hash = dsrc.last_hash.clone();
                s.last_stamp = dsrc.last_stamp.clone();
                s.failures = dsrc.failures;
                s.mirrors = dsrc.mirrors.clone();
                if !dsrc.title.is_empty() { s.title = dsrc.title.clone(); }
            } else {
                self.state.sources.insert(key.clone(), dsrc.clone());
            }
        }
        // Adopt the daemon's deletions: an entry it removed stays removed even though this
        // app's memory still holds it. A stale copy here would otherwise be saved back by
        // this app's next save, resurrecting the session with fingerprints from an older
        // binary — which turn-taking would then read as a fresh edit. An entry the TUI itself
        // created (a choice not scanned yet) survives.
        self.state.sources.retain(|key, s| disk.sources.contains_key(key) || s.tui_created);
    }

    /// Re-list the local sessions and keep the cursor on the same session, without a status
    /// message. Used for the background refresh after absorbing the daemon's state.
    pub fn refresh_rows(&mut self) {
        let keep = self.selected().map(|g| g.key.clone());
        self.groups = manage::list_groups_cached(&self.roots, &self.state, &mut self.titles);
        self.recompute_visible();
        self.rebuild_model_keep(keep);
    }

    /// Rebuild the list after a change on disk. The cursor keeps its session when it is still
    /// listed, and otherwise keeps its row position (a delete does not jump back to the top).
    fn reload(&mut self) {
        let keep = self.selected().map(|g| g.key.clone());
        let index = self.model.selected();
        self.groups = manage::list_groups_cached(&self.roots, &self.state, &mut self.titles);
        self.recompute_visible();
        let found = keep.and_then(|k| self.visible.iter().position(|i| self.groups[*i].key == k));
        self.model = Model::new(self.visible.iter().map(|i| self.groups[*i].title.clone()).collect());
        self.model.select(found.unwrap_or(index).min(self.visible.len().saturating_sub(1)));
    }

    /// Cancel an armed delete. Any key other than ctrl+d does this, in every mode.
    fn disarm(&mut self, now: Instant) {
        if self.model.armed().is_some() {
            self.model.press(Key::Other, now);
            if self.status_msg.starts_with("Press ctrl+d again") {
                self.say("delete cancelled");
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
            Mode::Filter => self.handle_filter(ev),
            Mode::Help => self.handle_help(ev),
            Mode::Transcript => self.handle_transcript(ev),
            Mode::DeleteConfirm => self.handle_delete_confirm(ev),
            Mode::Browse => self.handle_browse(ev, now),
        }
    }

    /// Ctrl+C's job: put the selected session's file on the clipboard and remind the user that
    /// quitting is done with q, not ctrl+c.
    fn copy_selected_path(&mut self) {
        let Some(g) = self.selected() else {
            self.say("nothing selected; press q to quit");
            return;
        };
        let path = g.key.clone();
        match crate::clipboard::copy(&path) {
            Ok(()) => self.say(format!("copied {} to the clipboard; press q to quit", path)),
            Err(e) => self.fail(format!("could not copy to the clipboard: {e}; press q to quit")),
        }
    }

    fn handle_browse(&mut self, ev: Ev, now: Instant) {
        match ev {
            // Local refresh needs no network: re-list the files so a rename made in the harness
            // itself appears. The team refresh needs the server client, which the caller supplies.
            Ev::CtrlR => {
                if self.view == View::Sessions {
                    self.refresh_local();
                } else {
                    self.refresh_requested = true;
                }
            }
            Ev::OpenEnroll => self.open_form(Kind::Enroll, &[]),
            Ev::OpenRegister => self.open_form(Kind::Register, &[]),
            Ev::CtrlD => {
                if self.view != View::Sessions {
                    self.say("delete works on local sessions only; press tab");
                    return;
                }
                let Some(g) = self.selected() else {
                    self.say("nothing selected");
                    return;
                };
                if self.model.press(Key::CtrlD, now).is_some() {
                    // The arm fires into a confirmation, not straight into deletion: the dialog
                    // names the session and says what stays (mirrors, the team's sealed copies).
                    self.armed_delete = Some(g.key.clone());
                    self.mode = Mode::DeleteConfirm;
                    self.say("");
                } else {
                    self.say(format!("Press ctrl+d again to DELETE '{}'. Any other key cancels.", g.title));
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
                View::Sessions => self.say("team sessions are on the Team tab; press tab"),
                View::Team => self.ask_import(),
            },
            Ev::Char('m') => {
                if self.view != View::Sessions {
                    self.say("marking works on local sessions only; press tab");
                    return;
                }
                self.toggle_mark();
            }
            // `/` opens the filter at the status line; `f` cycles which subset is listed.
            Ev::Char('/') => {
                if self.view != View::Sessions {
                    self.say("the filter works on local sessions only; press tab");
                    return;
                }
                self.mode = Mode::Filter;
                self.filter_status();
            }
            Ev::Char('f') => {
                if self.view != View::Sessions {
                    self.say("the view cycle works on local sessions only; press tab");
                    return;
                }
                self.filter_scope = self.filter_scope.next();
                self.recompute_visible();
                self.rebuild_model_keep(None);
                self.say(format!(
                    "view: {} — {} of {} session(s)",
                    self.filter_scope.label(), self.visible.len(), self.groups.len()
                ));
            }
            Ev::Char('?') => self.open_help(),
            Ev::Char('r') => {
                if self.view != View::Sessions {
                    self.say("renaming works on local sessions only; press tab");
                    return;
                }
                let Some(g) = self.selected() else { return };
                self.input = g.title.clone();
                self.mode = Mode::Rename;
                self.say("Enter a new name; Esc cancels.");
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

    /// Typing into the session filter. The list narrows live; Enter keeps it, Esc clears it.
    fn handle_filter(&mut self, ev: Ev) {
        match ev {
            Ev::Char(c) => {
                self.filter_text.push(c);
                self.recompute_visible();
                self.rebuild_model_keep(None);
                self.filter_status();
            }
            Ev::Backspace => {
                self.filter_text.pop();
                self.recompute_visible();
                self.rebuild_model_keep(None);
                self.filter_status();
            }
            Ev::Enter => {
                self.mode = Mode::Browse;
                self.filter_status();
                let kept = format!("{}; press / then esc to clear", self.status_msg.clone());
                self.say(kept);
            }
            Ev::Esc => {
                self.filter_text.clear();
                self.recompute_visible();
                self.rebuild_model_keep(None);
                self.mode = Mode::Browse;
                self.say("filter cleared");
            }
            _ => {}
        }
    }

    /// Open the `?` overlay naming the keys of the current tab.
    fn open_help(&mut self) {
        self.mode = Mode::Help;
        self.say("");
    }
    fn handle_help(&mut self, ev: Ev) {
        match ev {
            Ev::Esc | Ev::Char('?') | Ev::Enter => self.mode = Mode::Browse,
            _ => {}
        }
    }

    /// Show the selected session's file inside the TUI. Only the one file is read, and only its
    /// tail if it is huge; this is for a quick look, not for paging a whole conversation.
    fn open_transcript(&mut self) {
        let Some(g) = self.selected() else { return };
        let bytes = match std::fs::read(&g.key) {
            Ok(b) => b,
            Err(e) => {
                self.fail(format!("could not read {}: {e}", g.key));
                return;
            }
        };
        let all: Vec<String> = String::from_utf8_lossy(&bytes).lines().map(|l| l.to_string()).collect();
        let cut = all.len().saturating_sub(TRANSCRIPT_MAX_LINES);
        let lines = if cut > 0 {
            let mut v = vec![format!("… {} earlier line(s) not shown …", cut)];
            v.extend(all[cut..].iter().cloned());
            v
        } else {
            all
        };
        self.transcript = Some(TranscriptView { path: g.key.clone(), lines, scroll: 0 });
        self.mode = Mode::Transcript;
        self.say("");
    }

    fn handle_transcript(&mut self, ev: Ev) {
        let Some(t) = self.transcript.as_mut() else { self.mode = Mode::Browse; return };
        let max = t.lines.len().saturating_sub(1);
        match ev {
            Ev::Esc | Ev::Enter | Ev::Char('q') => {
                self.transcript = None;
                self.mode = Mode::Browse;
            }
            Ev::Up | Ev::Char('k') => t.scroll = t.scroll.saturating_sub(1),
            Ev::Down | Ev::Char('j') => t.scroll = (t.scroll + 1).min(max),
            Ev::PageUp => t.scroll = t.scroll.saturating_sub(10),
            Ev::PageDown => t.scroll = (t.scroll + 10).min(max),
            Ev::Home => t.scroll = 0,
            Ev::End => t.scroll = max,
            _ => {}
        }
    }

    /// The delete confirmation: a deliberate key deletes what was named, anything else cancels.
    fn handle_delete_confirm(&mut self, ev: Ev) {
        let Some(key) = self.armed_delete.take() else { self.mode = Mode::Browse; return };
        match ev {
            Ev::CtrlD | Ev::Enter | Ev::Char('y') => {
                self.mode = Mode::Browse;
                if let Some(g) = self.groups.iter().find(|g| g.key == key).cloned() {
                    self.delete_selected(&g);
                } else {
                    self.say("that session is not listed any more");
                }
            }
            _ => {
                self.mode = Mode::Browse;
                self.say("delete cancelled");
            }
        }
    }

    /// What the delete dialog shows: the session and exactly what is and is not removed.
    pub fn delete_preview(&self) -> Option<(String, String)> {
        let key = self.armed_delete.as_ref()?;
        let g = self.groups.iter().find(|g| &g.key == key)?;
        Some((g.title.clone(), g.key.clone()))
    }

    /// The transcript being read, if any.
    pub fn transcript_view(&self) -> Option<&TranscriptView> {
        self.transcript.as_ref()
    }

    /// The `?` overlay's contents: one `(keys, what)` per line, for the tab being shown.
    pub fn help_lines(&self) -> Vec<(&'static str, &'static str)> {
        match self.view {
            View::Sessions => vec![
                ("enter", "open the selected session's menu (rename, mark, mirror, delete)"),
                ("m", "mark / unmark the session for the team push"),
                ("/", "filter the list by title; enter keeps it, esc clears it"),
                ("f", "cycle what is listed: all, marked, mirrored, failing"),
                ("ctrl+r", "re-list the files, so harness-side renames show"),
                ("ctrl+d", "delete the session's source file (press twice); mirrors stay"),
                ("ctrl+c", "copy the session's path; runs in every mode"),
                ("ctrl+t", "change the colour scheme"),
                ("tab", "switch to the team list"),
                ("ctrl+p", "open the command list"),
                ("q", "quit"),
            ],
            View::Team => vec![
                ("i", "import the selected team session into this harness"),
                ("ctrl+r", "pull the team's verified sessions from the server"),
                ("tab", "back to the local sessions"),
                ("ctrl+p", "open the command list"),
                ("q", "quit"),
            ],
        }
    }

    /// Switch between the local list and the team list. Each keeps its own cursor.
    fn switch_view(&mut self) {
        self.view = match self.view {
            View::Sessions => View::Team,
            View::Team => View::Sessions,
        };
        self.prefs_dirty = true;
    }

    /// Restore last-run taste before the first draw. An unknown theme name falls back to
    /// whatever this run already has; a view value always applies.
    pub fn restore_prefs(&mut self, theme: Option<String>, view: Option<View>) {
        if let Some(t) = theme {
            if crate::theme::names_sorted().iter().any(|n| n == &t) {
                self.theme_name = t;
            }
        }
        if let Some(v) = view {
            self.view = v;
        }
    }

    /// Whether the taste changed since the caller last persisted it, and clear that flag.
    pub fn take_prefs_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.prefs_dirty, false)
    }

    /// Ask before writing a team session into the harness. The question is about the row on
    /// screen right now; only that exact one survives a later refresh.
    fn ask_import(&mut self) {
        let Some(sel) = self.team.get(self.team_index).cloned() else {
            self.say("no team session selected");
            return;
        };
        self.say(format!("import team session {} into your harness? y/n", sel.0));
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
        self.say("Enter runs the highlighted command; Esc closes.");
    }


    /// The session menu options, in display order. Each runs the same event its normal key sends.
    pub fn session_menu_rows(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("Rename", "r"),
            ("Mark / unmark for team", "m"),
            ("Mirror to harness", ""),
            ("Show transcript", "t"),
            ("Copy its path", "ctrl+c"),
            ("Delete", "ctrl+d"),
        ]
    }

    fn open_session_menu(&mut self) {
        self.menu_cursor = 0;
        self.mode = Mode::SessionMenu;
        self.say("Enter runs the highlighted option; Esc closes.");
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
                    3 => self.open_transcript(),
                    4 => self.copy_selected_path(),
                    5 => self.handle(Ev::CtrlD, now),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Open the mirror-target picker for the selected session. Targets are the harnesses installed
    /// here (passed in by the caller), minus the session's own format. The ticks start from what
    /// the session already has — its saved choice plus its existing mirrors — never from
    /// everything: one stray Enter must not mirror a session everywhere.
    fn open_mirror_pick(&mut self) {
        let Some(g) = self.selected() else { return };
        let own = g.fmt.clone();
        let offered: Vec<String> = self.installed_targets.iter().filter(|t| **t != own).cloned().collect();
        let mut chosen = std::collections::BTreeSet::new();
        if let Some(src) = self.state.sources.get(&g.key) {
            chosen.extend(src.chosen_targets.iter().filter(|t| offered.contains(t)).cloned());
        }
        chosen.extend(g.mirrors.iter().map(|(t, _)| t).filter(|t| offered.contains(t)).cloned());
        self.pick_chosen = chosen;
        self.pick_cursor = 0;
        self.mode = Mode::MirrorPick;
        self.say("Space toggles a harness; Enter mirrors the session into the ticked ones; Esc cancels.");
    }

    fn handle_mirror_pick(&mut self, ev: Ev) {
        let targets = self.picker_targets();
        let rows = targets.len();
        match ev {
            Ev::Esc => {
                self.mode = Mode::Browse;
                self.say("mirror cancelled");
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
            Ev::Char('a') => self.pick_chosen = targets.iter().cloned().collect(),
            Ev::Char('n') => self.pick_chosen.clear(),
            Ev::Enter => {
                let chosen: Vec<String> = self.pick_chosen.iter().cloned().collect();
                self.mode = Mode::Browse;
                self.save_chosen_targets(chosen);
            }
            _ => {}
        }
    }

    /// What a picker row is right now, so the user sees what unticking stops: `mirrored` when a
    /// mirror file exists, `chosen` when only the saved intent is there. None when neither.
    pub fn pick_row_note(&self, t: &str) -> Option<&'static str> {
        let g = self.selected()?;
        if g.mirrors.iter().any(|(m, _)| m == t) {
            return Some("mirrored");
        }
        if self.state.sources.get(&g.key).map_or(false, |s| s.chosen_targets.contains(t)) {
            return Some("chosen");
        }
        None
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
            .or_insert_with(|| {
                let mut s = crate::state::Source::new(&fmt, &key);
                s.tui_created = true;
                s
            });
        src.chosen_targets = chosen.iter().cloned().collect();
        self.user_dirty = true;
        self.say(if chosen.is_empty() {
            format!("'{title}' will not be mirrored anywhere")
        } else {
            format!("'{title}' will be kept in sync with: {}", chosen.join(", "))
        });
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
            self.say(format!("{} not available here: {reason}", palette::title_of(action)));
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
        self.say("up/down preview · enter apply · esc revert");
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
                self.prefs_dirty = true;
                self.say(format!("colour scheme now '{}'", self.theme_name));
            }
            Ev::Esc => {
                if let Some(original) = self.theme_original.take() {
                    self.theme_name = original;
                }
                self.mode = Mode::Browse;
                self.say("");
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
        self.say("");
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
            Ok(msg) => self.say(msg),
            Err(e) => self.fail(e),
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
                self.say("");
            }
            _ => {}
        }
    }

    fn submit_form(&mut self) {
        let Some(f) = self.form.as_ref() else { return };
        if let Err(reason) = f.validate() {
            self.fail(reason);
            return;
        }
        let kind = f.kind;
        let values = f.collected();
        self.form = None;
        self.mode = Mode::Browse;
        self.say(match kind {
            Kind::Enroll => "joining the team…",
            Kind::Register => "starting the team…",
        });
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
                self.say("");
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
                        self.say(format!("not imported: team session id {id} is not a harness session id"));
                        return;
                    }
                    let target = self.roots.omp.join(name);
                    match crate::team_import::import_confirmed(&target, &bytes, true) {
                        Ok(_) => {
                            self.imported.push(id.clone());
                            self.say(format!("imported team session {id}"));
                        }
                        Err(e) => self.fail(format!("not imported: {e:?}")),
                    }
                }
            }
            _ => {
                self.pending_import = None;
                self.mode = Mode::Browse;
                self.say("import cancelled");
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
                self.say("rename cancelled");
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
                        self.say(format!("renamed to '{saved}'"));
                        self.reload();
                    }
                    Err(e) => {
                        // Invalid titles and refusals keep the input open so the user can fix it.
                        self.fail(format!("refused: {e}"));
                    }
                }
            }
            _ => {}
        }
    }

    fn delete_selected(&mut self, g: &Group) {
        match manage::delete_group(&self.roots, g, &self.lock_dir, &self.sesync_lock) {
            Ok(done) => {
                self.say(format!("deleted {} item(s) for '{}'", done.len(), g.title));
                self.reload();
            }
            Err(e) => self.fail(format!("refused: {e}")),
        }
    }
}

#[allow(dead_code)]
fn _unused(_: ClaimOutcome<()>) {}
