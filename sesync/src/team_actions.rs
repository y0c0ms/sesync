//! The work behind the team dialogs. The binary runs the network steps on worker threads so the
//! UI never waits on the team server; the functions here do no drawing and touch no App state.

use std::path::Path;

use crate::app::{Kind, LeaveRequest, SetupRequest};
use crate::team_cli::{self, Command, RosterView};
use crate::team_join;

/// What a finished join, start or invite reports back to the UI.
#[derive(Debug)]
pub struct SetupDone {
    /// Status text on success, or the error to show.
    pub result: Result<String, String>,
    /// A created team's name and its one-time authenticator URI, for the QR dialog.
    pub created: Option<(String, String)>,
    /// True when this machine now holds a team credential (a join, pending or listed).
    pub joined: bool,
}

fn failed(error: String) -> SetupDone {
    SetupDone { result: Err(error), created: None, joined: false }
}

/// Run a join, start or invite dialog. Every value comes from the dialog's fields.
pub fn setup(req: &SetupRequest, config_dir: &Path) -> SetupDone {
    let v = |name: &str| req.value_of(name);
    match req.kind {
        Kind::Enroll => match team_join::join(&v("server url"), &v("team"), &v("member"), &v("machine"),
            &v("code"), config_dir)
        {
            Ok(j) => SetupDone {
                result: Ok(if j.pending {
                    format!(
                        "asked to join team '{}' as '{}' on '{}': waiting for the admin to approve it. \
                         Give the admin this fingerprint: {}",
                        v("team"), v("member"), v("machine"), j.fingerprint)
                } else {
                    format!("joined team '{}' as '{}' on '{}'; the credential is in {}",
                        v("team"), v("member"), v("machine"), j.path.display())
                }),
                created: None,
                joined: true,
            },
            Err(e) => failed(format!("not joined: {e}")),
        },
        Kind::Register => {
            let token = match team_cli::read_admin_token(Path::new(&v("admin token file"))) {
                Ok(t) => t,
                Err(e) => return failed(format!("not created: {e}")),
            };
            let members: Vec<String> = v("members").split(',')
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect();
            let team = v("team");
            let cmd = Command::Create { name: team.clone(), members };
            match team_cli::run_detailed(&cmd, &v("server url"), &token, config_dir) {
                Ok((_, uri)) => {
                    // The admin dialogs (members, invite) work on this team from now on, with
                    // the server and token file typed here, across restarts.
                    let record = team_cli::AdminTeam {
                        team: team.clone(), server: v("server url"), token_file: v("admin token file"),
                    };
                    let result = match team_cli::remember_administered(config_dir, &record) {
                        Ok(()) => format!("team '{team}' created"),
                        Err(e) => format!("team '{team}' created, but the admin dialogs will not find it: {e}"),
                    };
                    SetupDone { result: Ok(result), created: uri.map(|uri| (team, uri)), joined: false }
                }
                Err(e) => failed(format!("not created: {e}")),
            }
        }
        Kind::Invite => {
            let token = match team_cli::read_admin_token(Path::new(&v("admin token file"))) {
                Ok(t) => t,
                Err(e) => return failed(format!("not invited: {e}")),
            };
            let cmd = Command::Invite { name: v("team"), member: v("member") };
            match team_cli::run(&cmd, &v("server url"), &token, config_dir) {
                Ok(text) => SetupDone { result: Ok(text), created: None, joined: false },
                Err(e) => failed(format!("not invited: {e}")),
            }
        }
    }
}

/// An admin action on a team: the server, the token file, and the command to run.
#[derive(Debug)]
pub struct AdminJob {
    pub command: Command,
    pub server: String,
    pub token_file: String,
}

/// What an admin action produced.
#[derive(Debug)]
pub enum AdminDone {
    /// A roster read: the rows the members dialog shows.
    Roster(RosterView),
    /// A change was signed and published; the text says what happened.
    Message(String),
}

/// Leave a team the user picked: a membership's credential goes, or the record of a team managed
/// here, or both when both were asked. Local files only, no server call. Says what was done, for
/// the status line; an error when neither record was there.
pub fn leave(req: &LeaveRequest, config_dir: &Path) -> Result<String, String> {
    let mut done = Vec::new();
    if req.membership && team_join::leave(config_dir, &req.team)? {
        done.push("left on this machine; its sessions stay on the server");
    }
    if req.admin && team_cli::forget_administered(config_dir, &req.team)? {
        done.push("no longer managed here");
    }
    if done.is_empty() {
        return Err(format!("'{}' is not recorded on this machine any more", req.team));
    }
    Ok(format!("team '{}': {}", req.team, done.join("; ")))
}

/// Run an admin action. Roster reads return rows; every other command returns its text.
pub fn admin(job: &AdminJob, config_dir: &Path) -> Result<AdminDone, String> {
    let token = team_cli::read_admin_token(Path::new(&job.token_file))?;
    match &job.command {
        Command::Roster { name } => {
            team_cli::roster_view(&job.server, &token, name, config_dir).map(AdminDone::Roster)
        }
        cmd => team_cli::run(cmd, &job.server, &token, config_dir).map(AdminDone::Message),
    }
}
