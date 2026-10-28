//! The Team tab: one list of the conversations this machine shares and the team holds, the state of each
//! read from its completed turns (`team_compare`, never file times: invariant 27), and the divergence
//! dialog that decides a behind or diverged one. Every file or state change a choice makes is made here;
//! the bin only sequences the state save and the replacement (`run_pending_replace`).

use super::{App, Ev, Mode, View};
use crate::harness::{self, Kind};
use crate::lock::{with_claim, ClaimOutcome};
use crate::manage::Group;
use crate::session_cols;
use crate::team_compare::{Compared, Job, Outcome, Read};
use crate::team_import;
use crate::team_watch::TeamCopy;
use crate::turns::{Relation, Turn};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The state of one row, drawn as one character. Five states, five markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    /// Both copies hold the same completed turns (`=`).
    InStep,
    /// This machine has completed turns the team copy lacks, so they go out on the next push (`↑`).
    Waiting,
    /// The team copy has completed turns this machine lacks, or the team holds it and this machine
    /// does not (`↓`).
    Behind,
    /// Both copies have completed turns and neither is a prefix of the other (`≠`).
    Diverged,
    /// Shared by this machine with nothing to compare yet, or still being compared (`•`).
    Shared,
}

impl RowState {
    /// The character the list shows for the state.
    pub fn marker(self) -> char {
        match self {
            RowState::InStep => '=',
            RowState::Waiting => '↑',
            RowState::Behind => '↓',
            RowState::Diverged => '≠',
            RowState::Shared => '•',
        }
    }
}

/// One row of the Team list: a conversation this machine shares, or the team holds, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRow {
    pub conversation: String,
    pub title: String,
    /// The conversation id's first 8 characters (the list shows it only when two titles are equal).
    pub short_id: String,
    pub state: RowState,
    /// The state in words, with the reason; the dialog and the status line carry the long form.
    pub note: String,
    /// LATEST BY: "this machine" for this device's copy, the teammate's device fingerprint (the admin
    /// sees member@machine when the roster maps it), or "-" when the team holds no copy.
    pub from: String,
    /// The harness this machine's copy is in ("omp", "claude", ...), or "-" when it holds none.
    pub harness: String,
    /// The last folder of the project directory the session records, or "-".
    pub project: String,
    /// When the newest completed turn happened ("5m ago"), or "-".
    pub updated: String,
    /// A session of this conversation is on this machine (written here, or imported).
    pub here: bool,
    /// This machine shares the conversation with a team.
    pub shared: bool,
}

impl TeamRow {
    /// The state column: the marker and a short word, prefixed with `•` when this machine shares it
    /// (`= in step`, `• ↑ waiting`, `↓ not here`, `≠ diverged`, `• nothing to send`).
    pub fn state_text(&self) -> String {
        let word = match self.state {
            RowState::InStep => "in step",
            RowState::Waiting => "waiting",
            RowState::Behind if self.here => "behind",
            RowState::Behind => "not here",
            RowState::Diverged => "diverged",
            RowState::Shared if self.note.starts_with("checking") => "checking",
            RowState::Shared => "nothing to send",
        };
        match (self.shared, self.state) {
            (true, RowState::Shared) | (false, _) => format!("{} {word}", self.state.marker()),
            (true, _) => format!("• {} {word}", self.state.marker()),
        }
    }
}

/// What the comparison was asked for: this machine's copy as it was listed, and the team sequence
/// it was compared against. A later listing or pull asks again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub stamp: String,
    pub team_seq: u64,
}

/// One choice in the divergence dialog. Every choice except "decide later" keeps both histories:
/// the continued side keeps the conversation id, the other side becomes a fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Team copy replaces this session; this machine's version is a local fork.
    TakeTheirsForkMine,
    /// As above, and the fork is shared with the team as its own conversation.
    TakeTheirsShareMine,
    /// This machine's copy is pushed over the team copy; the team's version is a local fork.
    KeepMineForkTheirs,
    /// As above, and the team's fork is shared with the team as its own conversation.
    KeepMineShareTheirs,
    /// Nothing moves; pushes stay paused.
    Later,
}

impl Choice {
    pub const ALL: [Choice; 5] = [
        Choice::TakeTheirsForkMine,
        Choice::TakeTheirsShareMine,
        Choice::KeepMineForkTheirs,
        Choice::KeepMineShareTheirs,
        Choice::Later,
    ];

    pub fn from_digit(c: char) -> Option<Choice> {
        match c {
            '1' => Some(Choice::TakeTheirsForkMine),
            '2' => Some(Choice::TakeTheirsShareMine),
            '3' => Some(Choice::KeepMineForkTheirs),
            '4' => Some(Choice::KeepMineShareTheirs),
            '5' => Some(Choice::Later),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Choice::TakeTheirsForkMine => "Take theirs, keep mine here",
            Choice::TakeTheirsShareMine => "Take theirs, keep mine on team",
            Choice::KeepMineForkTheirs => "Keep mine, keep theirs here",
            Choice::KeepMineShareTheirs => "Keep mine, keep theirs on team",
            Choice::Later => "Decide later",
        }
    }

    /// What happens to each copy, in one line. Forks install into omp, the first native journal harness.
    pub fn describe(self) -> &'static str {
        match self {
            Choice::TakeTheirsForkMine => "the team copy replaces this session here (the old copy is archived); this machine's version becomes a local fork, not shared. Forks install into omp",
            Choice::TakeTheirsShareMine => "as above, and this machine's fork is shared with the team as its own conversation; the team copy stays",
            Choice::KeepMineForkTheirs => "this machine's copy is pushed over the team copy; the team's version becomes a local fork, not shared. Forks install into omp",
            Choice::KeepMineShareTheirs => "as above, and the team's fork is shared with the team as its own conversation; the team copy is replaced",
            Choice::Later => "nothing moves; pushes stay paused until the turns change or you decide",
        }
    }
}

/// Which side of the divergence dialog has the scroll and the focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Team,
}

impl Side {
    fn other(self) -> Side {
        match self {
            Side::Local => Side::Team,
            Side::Team => Side::Local,
        }
    }
}

/// The divergence dialog's content: both sides' new turns after their common history, and the choice.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolve {
    pub conversation: String,
    /// This machine's title.
    pub title: String,
    pub space: String,
    /// Completed turns both copies share, from the start.
    pub common: usize,
    /// This machine's completed turns past the common history.
    pub local_new: Vec<Turn>,
    /// The team copy's completed turns past the common history.
    pub team_new: Vec<Turn>,
    /// True when this machine's copy is a prefix of the team copy (behind), not diverged.
    pub behind: bool,
    /// The turn fingerprint of this machine's copy, as the "keep mine" request names it.
    pub local_fp: String,
    /// The team record sequence the comparison read.
    pub team_seq: u64,
    /// The team copy's journal bytes.
    pub team_bytes: Vec<u8>,
    /// Who wrote the team copy, as LATEST BY shows it (names the fork of theirs).
    pub team_who: String,
    /// This machine's copy as an omp journal; None when unreadable.
    pub local_journal: Option<Vec<u8>>,
    /// This machine's copy: (harness, source key) when it is a file.
    pub local_source: Option<(String, String)>,
    pub focus: Side,
    /// Scroll offset of each side, in turns: [local, team].
    pub scroll: [usize; 2],
    /// The highlighted option; Enter or y runs it.
    pub choice: Option<Choice>,
}

impl Resolve {
    /// The turns of one side.
    pub fn side_turns(&self, side: Side) -> &[Turn] {
        match side {
            Side::Local => &self.local_new,
            Side::Team => &self.team_new,
        }
    }

    fn scroll_focus(&mut self, by: isize) {
        let i = match self.focus {
            Side::Local => 0,
            Side::Team => 1,
        };
        let last = self.side_turns(self.focus).len().saturating_sub(1) as isize;
        self.scroll[i] = (self.scroll[i] as isize).saturating_add(by).clamp(0, last) as usize;
    }
}

/// A "take theirs" the user confirmed. The state records the replacement first; the bin then
/// performs it with `App::run_pending_replace`, so the daemon never sees the new copy without it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReplace {
    pub conversation: String,
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

/// The stamp of a listed local copy: changes when the listing sees it change.
fn stamp_of(g: &Group) -> String {
    format!("{}|{}", g.key, g.updated)
}

/// The title a journal carries, or "untitled".
fn title_of(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let (title, found) = crate::harness::journal::omp_title_from_lines(&lines);
    if found && !title.trim().is_empty() { title } else { "untitled".to_string() }
}

/// The time of the newest stamped record in a journal, in seconds.
fn newest_stamp(bytes: &[u8]) -> Option<f64> {
    let text = String::from_utf8_lossy(bytes);
    text.lines().rev()
        .filter(|l| l.contains("\"timestamp\""))
        .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()
            .and_then(|v| v["timestamp"].as_str().and_then(crate::prompts::parse_rfc3339)))
}

/// The last folder of a project directory, or "-".
fn project_of(cwd: Option<String>) -> String {
    cwd.map(|c| c.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "-".to_string())
}

/// The state and its words, from a comparison that was read.
fn classify(read: &Read) -> (RowState, String) {
    match &read.relation {
        Relation::Identical if read.team_turns.is_empty() => (RowState::Shared, "shared by this machine; no completed turn yet".to_string()),
        Relation::Identical => (RowState::InStep, "in step".to_string()),
        Relation::Ahead(n) => (RowState::Waiting, format!("waiting to push: {n} completed turn(s) the team lacks")),
        Relation::Behind(n) => (RowState::Behind, format!("behind: the team has {n} more completed turn(s)")),
        Relation::Diverged { common, .. } => (RowState::Diverged, format!("diverged after {common} common turn(s)")),
    }
}

/// A fresh id for a fork: derived from the conversation, the purpose and the time, so each fork is its
/// own session.
fn fork_id(conversation: &str, which: &str) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, format!("sesync-fork:{which}:{conversation}:{nanos}").as_bytes()).to_string()
}

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

impl App {
    /// Give the archive folder that "take theirs" keeps replaced copies in (the daemon's `archive`).
    pub fn set_archive_dir(&mut self, dir: PathBuf) {
        self.archive_dir = Some(dir);
    }

    /// The Team list: one row per conversation this machine shares or the team holds, newest first.
    pub fn team_rows(&self) -> Vec<TeamRow> {
        let now = super::unix_now();
        let locals = self.locals_by_conversation();
        let copies = self.newest_copies();
        let mut convs: BTreeSet<String> = locals.keys().filter(|c| self.state.is_shared(c)).cloned().collect();
        convs.extend(copies.keys().cloned());
        let mut rows: Vec<(f64, TeamRow)> = convs.iter()
            .map(|c| self.row_for(c, locals.get(c).copied(), copies.get(c).copied(), now))
            .collect();
        rows.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.title.cmp(&b.1.title)));
        rows.into_iter().map(|(_, r)| r).collect()
    }

    fn row_for(&self, conv: &str, local: Option<&Group>, copy: Option<&TeamCopy>, now: f64) -> (f64, TeamRow) {
        let fresh = self.fresh_result(conv, local, copy).map(|c| &c.outcome);
        let (state, note) = match (local, copy, fresh) {
            (None, Some(_), _) => (RowState::Behind, "not here: the team holds it; i imports it".to_string()),
            (None, None, _) => (RowState::Shared, "shared by this machine; no session file here".to_string()),
            (Some(_), _, Some(Outcome::Unreadable(why))) => (RowState::Shared, format!("this copy cannot be read: {why}")),
            (Some(_), _, Some(Outcome::Read(read))) => classify(read),
            (Some(_), _, None) => (RowState::Shared, "checking…".to_string()),
        };
        let read = match fresh {
            Some(Outcome::Read(r)) => Some(r),
            _ => None,
        };
        let at = read
            .map(|r| r.local_turns.iter().chain(&r.team_turns).map(|t| t.at_ms as f64 / 1000.0).fold(0.0, f64::max))
            .filter(|t| *t > 0.0)
            .or_else(|| copy.and_then(|c| newest_stamp(&c.text)))
            .or_else(|| local.map(|g| g.updated).filter(|t| *t > 0.0));
        let title = match (local, copy) {
            (Some(g), _) => g.title.clone(),
            (None, Some(c)) => title_of(&c.text),
            (None, None) => conv.to_string(),
        };
        let cwd = read.and_then(|r| r.local_cwd.clone())
            .or_else(|| copy.and_then(|c| crate::harness::journal::session_head(&c.text).cwd));
        let row = TeamRow {
            conversation: conv.to_string(),
            title,
            short_id: conv.chars().take(8).collect(),
            state,
            note,
            from: self.latest_by(copy),
            harness: local.map_or_else(|| "-".to_string(), |g| g.fmt.clone()),
            project: project_of(cwd),
            updated: at.map_or_else(|| "-".to_string(), |t| session_cols::relative(now, t)),
            here: local.is_some(),
            shared: self.state.is_shared(conv),
        };
        (at.unwrap_or(0.0), row)
    }

    fn latest_by(&self, copy: Option<&TeamCopy>) -> String {
        match copy {
            None => "-".to_string(),
            Some(c) if c.mine => "this machine".to_string(),
            Some(c) if c.by.is_empty() => "teammate".to_string(),
            Some(c) if self.admin => self.roster_names.get(&c.by).cloned().unwrap_or_else(|| c.by.clone()),
            Some(c) => c.by.clone(),
        }
    }

    /// The comparison of this conversation, only while it still describes the listed copy and the
    /// team copy it was compared with.
    fn fresh_result(&self, conv: &str, local: Option<&Group>, copy: Option<&TeamCopy>) -> Option<&Compared> {
        let c = self.compared.get(conv)?;
        let stamp = local.map(stamp_of).unwrap_or_default();
        (c.stamp == stamp && c.team_seq == copy.map_or(0, |t| t.seq)).then_some(c)
    }

    /// This machine's copy by conversation id (the first listed copy of each).
    fn locals_by_conversation(&self) -> BTreeMap<String, &Group> {
        let mut out: BTreeMap<String, &Group> = BTreeMap::new();
        for g in &self.groups {
            out.entry(self.state.team_id_of(&g.key)).or_insert(g);
        }
        out
    }

    /// The newest verified copy of each conversation the last pull returned (ties keep the first).
    fn newest_copies(&self) -> BTreeMap<String, &TeamCopy> {
        let mut out: BTreeMap<String, &TeamCopy> = BTreeMap::new();
        for c in &self.team_copies {
            match out.get(&c.conversation) {
                Some(best) if best.seq >= c.seq => {}
                _ => {
                    out.insert(c.conversation.clone(), c);
                }
            }
        }
        out
    }

    /// The comparisons the list needs and has not asked for yet: one per shared local copy, asked
    /// again only when that copy or the team copy's sequence changes. The caller hands each to the
    /// comparison worker. A team-only row needs none.
    pub fn take_compare_jobs(&mut self) -> Vec<Job> {
        let planned: Vec<(String, Job, Asked)> = {
            let locals = self.locals_by_conversation();
            let copies = self.newest_copies();
            let convs: BTreeSet<String> = locals.keys().filter(|c| self.state.is_shared(c)).cloned()
                .chain(copies.keys().cloned())
                .collect();
            convs.into_iter().filter_map(|conv| {
                let g = locals.get(&conv).copied()?;
                let copy = copies.get(&conv).copied();
                let asked = Asked { stamp: stamp_of(g), team_seq: copy.map_or(0, |c| c.seq) };
                let job = Job {
                    conversation: conv.clone(),
                    local: Some((g.fmt.clone(), g.key.clone())),
                    stamp: asked.stamp.clone(),
                    team: copy.map(|c| (c.seq, c.text.clone())),
                };
                Some((conv, job, asked))
            }).collect()
        };
        let mut jobs = Vec::new();
        for (conv, job, asked) in planned {
            if self.compare_asked.get(&conv) == Some(&asked) {
                continue;
            }
            self.compare_asked.insert(conv, asked);
            jobs.push(job);
        }
        jobs
    }

    /// Take the comparisons the worker finished. Each is judged against the listed copy when the list
    /// is drawn, so a late answer for a copy that has since changed is never shown as current.
    pub fn apply_compared(&mut self, results: Vec<Compared>) {
        for r in results {
            self.compared.insert(r.conversation.clone(), r);
        }
    }

    /// Remember each listed device's name by fingerprint, so LATEST BY shows member@machine for the
    /// admin. Replaced whole by every roster read.
    pub(super) fn remember_roster(&mut self, view: &crate::team_cli::RosterView) {
        self.roster_names = view.devices.iter()
            .filter(|d| !d.fingerprint.is_empty())
            .map(|d| (d.fingerprint.clone(), match (&d.member, &d.machine) {
                (Some(m), Some(mc)) => format!("{m}@{mc}"),
                (Some(m), None) => m.clone(),
                _ => d.fingerprint.clone(),
            }))
            .collect();
    }

    /// The divergence dialog's content, while it is open.
    pub fn resolve(&self) -> Option<&Resolve> {
        self.resolve.as_ref()
    }

    /// The selected row of the Team list.
    pub fn team_selected(&self) -> Option<TeamRow> {
        self.team_rows().into_iter().nth(self.team_index)
    }

    /// Whether the selected row can be resolved now (behind or diverged, and held here).
    pub(super) fn resolve_ready(&self) -> Result<(), &'static str> {
        if self.view != View::Team {
            return Err("resolving works on the Team tab; press tab");
        }
        match self.team_selected() {
            Some(row) if row.here && matches!(row.state, RowState::Diverged | RowState::Behind) => Ok(()),
            Some(_) => Err("the selected conversation is not behind or diverged"),
            None => Err("no team session selected"),
        }
    }

    /// Enter on the Team tab: resolve a behind or diverged row, else open the command list.
    pub(super) fn enter_team(&mut self) {
        if self.resolve_ready().is_ok() {
            self.open_resolve();
        } else {
            self.open_palette();
        }
    }

    /// Open the divergence dialog for the selected row, or say why it cannot be opened.
    pub(super) fn open_resolve(&mut self) {
        match self.resolve_target() {
            Ok(r) => {
                self.resolve = Some(r);
                self.mode = Mode::Resolve;
            }
            Err(why) => self.say(why),
        }
    }

    /// The dialog's content for the selected row, from the comparison as it stands.
    fn resolve_target(&self) -> Result<Resolve, String> {
        self.resolve_ready().map_err(str::to_string)?;
        let row = self.team_selected().ok_or_else(|| "no team session selected".to_string())?;
        let locals = self.locals_by_conversation();
        let copies = self.newest_copies();
        let (Some(g), Some(copy)) = (locals.get(&row.conversation).copied(), copies.get(&row.conversation).copied()) else {
            return Err("no team copy to compare with".into());
        };
        let Some(Outcome::Read(read)) = self.fresh_result(&row.conversation, Some(g), Some(copy)).map(|c| &c.outcome) else {
            return Err("still comparing; try again in a moment".into());
        };
        let (common, local_new, team_new, behind) = match &read.relation {
            Relation::Diverged { common, local_new, team_new } => (*common, local_new.clone(), team_new.clone(), false),
            Relation::Behind(_) => {
                let n = read.local_turns.len();
                (n, Vec::new(), read.team_turns.get(n..).unwrap_or(&[]).to_vec(), true)
            }
            _ => return Err("nothing to resolve: the copies agree, or this machine is ahead and pushes".into()),
        };
        Ok(Resolve {
            conversation: row.conversation.clone(),
            title: g.title.clone(),
            space: copy.space.clone(),
            common,
            local_new,
            team_new,
            behind,
            local_fp: read.local_fp.clone(),
            team_seq: copy.seq,
            team_bytes: copy.text.clone(),
            team_who: self.latest_by(Some(copy)),
            local_journal: read.local_journal.clone(),
            local_source: Some((g.fmt.clone(), g.key.clone())),
            focus: Side::Local,
            scroll: [0, 0],
            choice: None,
        })
    }

    /// Keys in the divergence dialog: scroll, switch side, choose 1-5, y or enter confirms, esc decides later.
    pub(super) fn handle_resolve(&mut self, ev: Ev) {
        let Some(mut r) = self.resolve.take() else {
            self.mode = Mode::Browse;
            return;
        };
        let confirm = match ev {
            Ev::Esc => {
                self.mode = Mode::Browse;
                self.say("decided later: nothing changed");
                return;
            }
            Ev::Tab => {
                r.focus = r.focus.other();
                None
            }
            Ev::Up | Ev::Char('k') => {
                r.scroll_focus(-1);
                None
            }
            Ev::Down | Ev::Char('j') => {
                r.scroll_focus(1);
                None
            }
            Ev::PageUp => {
                r.scroll_focus(-10);
                None
            }
            Ev::PageDown => {
                r.scroll_focus(10);
                None
            }
            Ev::Char(c) if Choice::from_digit(c).is_some() => {
                r.choice = Choice::from_digit(c);
                None
            }
            Ev::Enter | Ev::Char('y') => match r.choice {
                Some(choice) => Some(choice),
                None => {
                    self.say("choose an option, 1 to 5, first");
                    None
                }
            },
            _ => None,
        };
        match confirm {
            Some(choice) => self.confirm_resolve(r, choice),
            None => self.resolve = Some(r),
        }
    }

    /// Write a fork of `bytes` as a new omp session titled `title`, under a new id, and return the id.
    /// Nothing else changes; the caller decides what the fork is shared as.
    fn write_fork(&mut self, conversation: &str, which: &str, bytes: &[u8], title: &str) -> Result<String, String> {
        let id = fork_id(conversation, which);
        let forked = team_import::fork_journal(bytes, &id, title).map_err(|e| format!("not forked: {e}"))?;
        self.write_native(&id, &forked).map_err(|e| format!("not forked: {e}"))?;
        self.refresh_rows();
        Ok(id)
    }

    /// Run the chosen option. Every option except "decide later" keeps both histories: the fork is
    /// written first, so a failed fork changes nothing else.
    fn confirm_resolve(&mut self, r: Resolve, choice: Choice) {
        self.mode = Mode::Browse;
        let share = |a: &mut App, fork: &str| {
            a.state.set_shares(fork, BTreeSet::from([r.space.clone()]));
            a.user_dirty = true;
        };
        match choice {
            Choice::Later => self.say("decided later: nothing changed"),
            Choice::TakeTheirsForkMine | Choice::TakeTheirsShareMine => {
                let Some((fmt, key)) = r.local_source.clone() else {
                    self.fail("take theirs: this machine holds no local copy to replace");
                    return;
                };
                let native = harness::get(&fmt).is_some_and(|h| h.kind() == Kind::Files && h.native_journal());
                if !native {
                    self.fail(format!("take theirs needs this copy to be an omp or Pi journal; this one is {fmt}"));
                    return;
                }
                let Some(mine) = r.local_journal.clone() else {
                    self.fail("take theirs: this machine's copy cannot be read as a journal; nothing changed");
                    return;
                };
                let fork = match self.write_fork(&r.conversation, "mine", &mine, &format!("{} (mine, forked)", r.title)) {
                    Ok(id) => id,
                    Err(e) => return self.fail(e),
                };
                if choice == Choice::TakeTheirsShareMine {
                    share(self, &fork);
                }
                // The state records the replacement before the file changes (run_pending_replace).
                self.state.supersede(&r.conversation, now_secs());
                self.user_dirty = true;
                self.pending_replace = Some(PendingReplace {
                    conversation: r.conversation.clone(),
                    path: PathBuf::from(key),
                    bytes: r.team_bytes.clone(),
                });
                self.say(format!("taking theirs for '{}'; this machine's version is kept as fork {fork}", r.title));
            }
            Choice::KeepMineForkTheirs | Choice::KeepMineShareTheirs => {
                let title = format!("{} ({}, forked)", r.title, r.team_who);
                let fork = match self.write_fork(&r.conversation, "theirs", &r.team_bytes, &title) {
                    Ok(id) => id,
                    Err(e) => return self.fail(e),
                };
                if choice == Choice::KeepMineShareTheirs {
                    share(self, &fork);
                }
                // This machine's copy goes over the team's, for exactly the team record reviewed.
                self.state.request_team_force(&r.space, &r.conversation, &r.local_fp, r.team_seq);
                self.user_dirty = true;
                self.say(format!("keeping mine on the team for '{}'; the team's version is kept as fork {fork}", r.title));
            }
        }
    }

    /// Perform a confirmed "take theirs": the state file is saved with its replacement mark, then the
    /// local copy is replaced under the conversation's claim, its old bytes archived and read back first.
    /// Called by the bin after each key, so the mark is on disk before the file changes.
    pub fn run_pending_replace(&mut self, state_path: &Path) {
        let Some(p) = self.pending_replace.take() else { return };
        if let Err(e) = self.save_user(state_path) {
            self.fail(format!("not replaced: the state file could not be saved: {e}"));
            return;
        }
        let Some(archive) = self.archive_dir.clone() else {
            self.fail("not replaced: no archive folder is set");
            return;
        };
        let target = p.path.display().to_string();
        let outcome = with_claim(&self.sesync_lock, Some(&self.lock_dir), &p.conversation, "tui",
            std::process::id(), &target, || {
                team_import::replace_confirmed(&archive, &p.conversation, &p.path, &p.bytes, true)
            });
        match outcome {
            ClaimOutcome::Ran(Ok(_)) => {
                self.say("took theirs: this machine's copy now matches the team copy; the old copy is archived");
                self.refresh_rows();
            }
            ClaimOutcome::Ran(Err(e)) => self.fail(format!("not replaced: {e:?}")),
            ClaimOutcome::Skipped(holder) => self.fail(format!("not replaced: {holder} is writing this conversation; try again when it is quiet")),
            ClaimOutcome::Failed(why) => self.fail(format!("not replaced: {why}")),
        }
    }

    /// Write `bytes` as session `id` into the first harness that keeps canonical journals (omp), filed
    /// the way that harness files its own. Refused when that harness already holds `id`. Returns the
    /// file and the harness's name.
    pub(super) fn write_native(&mut self, id: &str, bytes: &[u8]) -> Result<(PathBuf, String), String> {
        use crate::harness::journal;
        let Some(h) = harness::ALL.iter().find(|h| h.native_journal()) else {
            return Err("no harness keeps canonical journals".into());
        };
        let loc = self.roots.locations.of(h.id());
        let head = journal::session_head(bytes);
        let home = self.roots.home.display().to_string();
        let cwd = head.cwd.clone().unwrap_or_else(|| home.clone());
        let Some(folder) = h.folder_for_cwd(loc, &home, &cwd) else {
            return Err(format!("{} has no project folder to file it in", h.id()));
        };
        // The harness lists only `<timestamp>_<uuid>.jsonl`. A header without a usable start time
        // takes the import's; an id that cannot be written in that form is refused, so no file
        // is written that nothing shows.
        let mut name = journal::journal_filename(head.timestamp.as_deref().unwrap_or_default(), id);
        if !journal::is_listed_session_name(&name) {
            name = journal::journal_filename(&journal::now_iso_ms(), id);
        }
        if !journal::is_listed_session_name(&name) {
            return Err(format!("{id} is not a harness session id"));
        }
        if h.sessions(loc).iter().any(|p| h.id_in(&p.display().to_string()).as_deref() == Some(id)) {
            return Err(format!("{} already holds session {id}", h.id()));
        }
        let path = folder.join(name);
        match team_import::import_confirmed(&path, bytes, true) {
            Ok(_) => Ok((path, h.id().to_string())),
            Err(e) => Err(format!("{e:?}")),
        }
    }

    /// Import the team copy of a conversation (`i`): as a new session in the native journal harness.
    pub(super) fn import_team_session(&mut self, id: &str, bytes: &[u8]) {
        match self.write_native(id, bytes) {
            Ok((_, harness)) => {
                self.imported.push(id.to_string());
                self.say(format!("imported team session {id} into {harness}"));
                self.refresh_rows();
            }
            Err(e) => self.fail(format!("not imported: {e}")),
        }
    }

    /// Ask before writing a team copy into the harness. Only the row on screen right now is asked about.
    pub(super) fn ask_import(&mut self) {
        let Some(row) = self.team_selected() else {
            self.say("no team session selected");
            return;
        };
        let picked: Option<(String, Vec<u8>)> = self.newest_copies().get(&row.conversation)
            .map(|c| (c.conversation.clone(), c.text.clone()));
        let Some(sel) = picked else {
            self.say("this conversation is not on the team; nothing to import");
            return;
        };
        self.say(format!("import team session {} into your harness? y/n", sel.0));
        self.pending_import = Some(sel);
        self.mode = Mode::Confirm;
    }
}
