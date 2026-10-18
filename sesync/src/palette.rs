//! The command palette model: every action the TUI can do, how typing filters the list, and
//! whether an action can run right now. Pure data; the app owns any state and side effects,
//! so a chosen action is turned into the same event the usual key would send.

use crate::app::{Ev, View};

/// One thing the user can do, whatever view or key it is normally tied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Rename,
    Delete,
    Mark,
    Import,
    Share,
    Unshare,
    RefreshTeam,
    SwitchView,
    Quit,
    Theme,
    Enroll,
    Register,
}

struct Def {
    action: Action,
    title: &'static str,
    key: &'static str,
    /// The event the normal key sends. Actions without a key run only from here.
    event: Ev,
}

const ACTIONS: &[Def] = &[
    Def { action: Action::Rename, title: "Rename session", key: "r", event: Ev::Char('r') },
    Def { action: Action::Delete, title: "Delete session", key: "ctrl+d", event: Ev::CtrlD },
    Def { action: Action::Mark, title: "Mark / unmark for team", key: "m", event: Ev::Char('m') },
    Def { action: Action::Import, title: "Import team session", key: "i", event: Ev::Char('i') },
    Def { action: Action::Share, title: "Share with team", key: "", event: Ev::Char('s') },
    Def { action: Action::Unshare, title: "Unshare", key: "", event: Ev::Char('u') },
    Def { action: Action::RefreshTeam, title: "Refresh team", key: "ctrl+r", event: Ev::CtrlR },
    Def { action: Action::SwitchView, title: "Switch Sessions/Team", key: "tab", event: Ev::Tab },
    Def { action: Action::Quit, title: "Quit", key: "q", event: Ev::Char('q') },
    Def { action: Action::Theme, title: "Colour scheme", key: "ctrl+t", event: Ev::Theme },
    Def { action: Action::Enroll, title: "Join a team", key: "", event: Ev::OpenEnroll },
    Def { action: Action::Register, title: "Start a team", key: "", event: Ev::OpenRegister },
];

/// What a command can see when deciding whether it may run.
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    pub view: crate::app::View,
    pub session_selected: bool,
    pub team_selected: bool,
    pub team_configured: bool,
}

/// Ok, or the reason the action cannot run right now, shown beside it in the list.
pub fn enabled(action: Action, ctx: &Ctx) -> Result<(), &'static str> {
    match action {
        Action::Rename | Action::Delete | Action::Mark => match (ctx.view, ctx.session_selected) {
            (View::Sessions, true) => Ok(()),
            (View::Sessions, false) => Err("no local session selected"),
            (View::Team, _) => Err("older sessions are on the Team tab; press tab"),
        },
        Action::Import => match (ctx.view, ctx.team_selected) {
            (View::Team, true) => Ok(()),
            (View::Team, false) => Err("no team session selected"),
            (View::Sessions, _) => Err("team sessions are on the Team tab; press tab"),
        },
        Action::Share => Err("not available yet: sharing needs the team member list"),
        Action::Unshare => Err("not available yet: nothing is shared"),
        Action::RefreshTeam => {
            if ctx.team_configured {
                Ok(())
            } else {
                Err("no team server configured")
            }
        }
        Action::SwitchView | Action::Quit | Action::Theme | Action::Enroll | Action::Register => Ok(()),
    }
}

/// The event that runs the action in the normal key path.
pub fn event_of(action: Action) -> Ev {
    ACTIONS.iter().find(|d| d.action == action).map_or(Ev::Esc, |d| d.event.clone())
}

fn is_subsequence(hay: &[char], needle: &[char]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut it = hay.iter();
    needle.iter().all(|n| it.any(|h| h == n))
}

/// Whether `title` matches a typed query the same way the command list filters: the query's
/// letters must appear in order, case-insensitively. Used for the session filter too, so the
/// two search boxes behave alike.
pub fn fuzzy_matches(title: &str, query: &str) -> bool {
    is_subsequence(&title.to_lowercase().chars().collect::<Vec<_>>(), &query.to_lowercase().chars().collect::<Vec<_>>())
}

/// Every action whose title contains the query's letters in order. Prefix matches come first;
/// ties keep the declaration order. An empty query returns everything.
pub fn filter(query: &str) -> Vec<Action> {
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let mut prefix = Vec::new();
    let mut other = Vec::new();
    for def in ACTIONS {
        let t: Vec<char> = def.title.to_lowercase().chars().collect();
        if is_subsequence(&t, &q) {
            if t.starts_with(&q) {
                prefix.push(def.action);
            } else {
                other.push(def.action);
            }
        }
    }
    prefix.extend(other);
    prefix
}

/// Title as the list shows it.
pub fn title_of(action: Action) -> &'static str {
    ACTIONS.iter().find(|d| d.action == action).map_or("", |d| d.title)
}

/// Key chord shown beside the title, if the action has one.
pub fn key_of(action: Action) -> &'static str {
    ACTIONS.iter().find(|d| d.action == action).map_or("", |d| d.key)
}
