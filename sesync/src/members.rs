//! State of the team members dialog: the roster rows, the cursor, and the two-press rule for every
//! change. Pure: `press` answers with the command to run, and the App sends it to a worker. A
//! change is never sent on the first press; the second press on the same row confirms it.

use crate::team_cli::{Command, DeviceRow, PendingRow, RosterView};

/// One row of the dialog: a listed device, or a join request waiting for a decision.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Listed(DeviceRow),
    Waiting(PendingRow),
}

/// The changes the dialog can ask for, each on its own key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// `a`: sign a waiting device into the list.
    Approve,
    /// `d`: refuse a waiting device for good.
    Decline,
    /// `r`: sign one listed device out of the list.
    Revoke,
    /// `m`: sign every device of a member out, and drop the member.
    Remove,
}

impl Act {
    pub fn from_key(c: char) -> Option<Act> {
        match c {
            'a' => Some(Act::Approve),
            'd' => Some(Act::Decline),
            'r' => Some(Act::Revoke),
            'm' => Some(Act::Remove),
            _ => None,
        }
    }
}

/// What a key press in the dialog should do.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// First press: nothing is sent yet; the text says what the second press will do.
    Armed(String),
    /// The change is not possible on this row; the text says why.
    Refused(String),
    /// Second press on the armed row: run this command.
    Run(Command),
}

#[derive(Debug)]
pub struct Members {
    pub team: String,
    pub rows: Vec<Row>,
    /// The version of the list the rows were read from, once one has been read.
    pub version: Option<u64>,
    pub cursor: usize,
    /// The row and action waiting for a second press, with the text that asked for it.
    armed: Option<(usize, Act, String)>,
}

impl Members {
    pub fn new(team: &str) -> Members {
        Members { team: team.to_string(), rows: Vec::new(), version: None, cursor: 0, armed: None }
    }

    /// Replace the rows with a fresh roster read: listed devices first, then waiting requests.
    pub fn set_roster(&mut self, view: &RosterView) {
        self.version = Some(view.version);
        self.rows = view.devices.iter().cloned().map(Row::Listed)
            .chain(view.pending.iter().cloned().map(Row::Waiting))
            .collect();
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        self.armed = None;
    }

    /// (listed devices, waiting requests).
    pub fn counts(&self) -> (usize, usize) {
        let listed = self.rows.iter().filter(|r| matches!(r, Row::Listed(_))).count();
        (listed, self.rows.len() - listed)
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let last = self.rows.len().saturating_sub(1) as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
    }

    /// The text of the pending confirmation, if one is armed.
    pub fn prompt(&self) -> Option<&str> {
        self.armed.as_ref().map(|(_, _, text)| text.as_str())
    }

    pub fn disarm(&mut self) {
        self.armed = None;
    }

    /// Handle one change key on the row under the cursor.
    pub fn press(&mut self, act: Act) -> Step {
        let row = self.cursor;
        let (cmd, prompt) = match self.plan(act) {
            Ok(planned) => planned,
            Err(why) => {
                self.armed = None;
                return Step::Refused(why);
            }
        };
        if matches!(&self.armed, Some((r, a, _)) if *r == row && *a == act) {
            self.armed = None;
            return Step::Run(cmd);
        }
        self.armed = Some((row, act, prompt.clone()));
        Step::Armed(prompt)
    }

    /// The command this change would send on the row under the cursor, and the text that asks for
    /// confirmation. Refusals explain themselves here.
    fn plan(&self, act: Act) -> Result<(Command, String), String> {
        let name = self.team.clone();
        match (self.rows.get(self.cursor), act) {
            (None, _) => Err("nothing to change: the list is empty".into()),
            (Some(Row::Listed(_)), Act::Approve | Act::Decline) => {
                Err("already listed: only waiting requests can be approved (a) or refused (d)".into())
            }
            (Some(Row::Listed(d)), Act::Revoke) => {
                if d.admin {
                    return Err("the admin's own key cannot be revoked from here".into());
                }
                match (&d.member, &d.machine) {
                    (Some(member), Some(machine)) => Ok((
                        Command::Revoke { name, member: member.clone(), machine: machine.clone() },
                        format!("press r again to sign {member} / {machine} out of the list"),
                    )),
                    _ => Err("this key has no member name, so it cannot be revoked here".into()),
                }
            }
            (Some(Row::Listed(d)), Act::Remove) => {
                if d.admin {
                    return Err("the admin's own key cannot be removed from here".into());
                }
                match &d.member {
                    Some(member) => Ok((
                        Command::Remove { name, member: member.clone() },
                        format!("press m again to sign every device of {member} out and drop the member"),
                    )),
                    None => Err("this key has no member name, so it cannot be removed here".into()),
                }
            }
            (Some(Row::Waiting(p)), Act::Approve) => Ok((
                Command::Approve {
                    name,
                    member: p.member.clone(),
                    machine: p.machine.clone(),
                    fingerprint: Some(p.fingerprint.clone()),
                },
                format!(
                    "compare {} with what {} reads on their screen, then press a again to approve",
                    p.fingerprint, p.member),
            )),
            (Some(Row::Waiting(p)), Act::Decline) => Ok((
                Command::Decline { name, member: p.member.clone(), machine: p.machine.clone() },
                format!("press d again to refuse {} / {} for good", p.member, p.machine),
            )),
            (Some(Row::Waiting(_)), Act::Revoke | Act::Remove) => {
                Err("a waiting request is approved (a) or refused (d); it is not on the list yet".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(member: Option<&str>, admin: bool) -> DeviceRow {
        DeviceRow {
            member: member.map(str::to_string),
            machine: member.map(|_| "laptop-1".to_string()),
            fingerprint: "aaaa-bbbb-cccc-dddd".into(),
            admin,
        }
    }

    fn waiting() -> PendingRow {
        PendingRow {
            member: "pixel".into(),
            machine: "pixel-7".into(),
            fingerprint: "1111-2222-3333-4444".into(),
            age_secs: 90,
        }
    }

    fn roster(devices: Vec<DeviceRow>, pending: Vec<PendingRow>) -> RosterView {
        RosterView { name: "main".into(), version: 4, devices, pending }
    }

    #[test]
    fn approve_needs_a_second_press_and_sends_the_fingerprint() {
        let mut m = Members::new("main");
        m.set_roster(&roster(vec![device(Some("laptop"), false)], vec![waiting()]));
        m.move_cursor(1);
        let first = m.press(Act::Approve);
        assert!(matches!(&first, Step::Armed(t) if t.contains("1111-2222-3333-4444")), "{first:?}");
        assert_eq!(m.press(Act::Approve), Step::Run(Command::Approve {
            name: "main".into(),
            member: "pixel".into(),
            machine: "pixel-7".into(),
            fingerprint: Some("1111-2222-3333-4444".into()),
        }));
        assert!(m.prompt().is_none(), "the confirmation is consumed");
    }

    #[test]
    fn moving_the_cursor_between_presses_arms_the_new_row_instead() {
        let mut m = Members::new("main");
        m.set_roster(&roster(vec![device(Some("laptop"), false)], vec![waiting()]));
        m.move_cursor(1);
        assert!(matches!(m.press(Act::Decline), Step::Armed(_)));
        m.move_cursor(-1);
        assert!(matches!(m.press(Act::Revoke), Step::Armed(_)), "a different row re-arms, never runs");
    }

    #[test]
    fn a_different_key_on_the_same_row_does_not_confirm_the_armed_change() {
        let mut m = Members::new("main");
        m.set_roster(&roster(vec![], vec![waiting()]));
        assert!(matches!(m.press(Act::Decline), Step::Armed(_)));
        assert!(matches!(m.press(Act::Approve), Step::Armed(_)), "another act arms, it does not run decline");
    }

    #[test]
    fn the_admin_key_is_never_revoked_or_removed_from_the_dialog() {
        let mut m = Members::new("main");
        m.set_roster(&roster(vec![device(Some("admin"), true)], vec![]));
        assert_eq!(m.press(Act::Revoke), Step::Refused("the admin's own key cannot be revoked from here".into()));
        assert_eq!(m.press(Act::Remove), Step::Refused("the admin's own key cannot be removed from here".into()));
    }

    #[test]
    fn a_listed_device_cannot_be_approved_and_a_request_cannot_be_revoked() {
        let mut m = Members::new("main");
        m.set_roster(&roster(vec![device(Some("laptop"), false)], vec![waiting()]));
        assert!(matches!(m.press(Act::Approve), Step::Refused(_)), "a listed device is not approved again");
        m.move_cursor(1);
        assert!(matches!(m.press(Act::Revoke), Step::Refused(_)), "a request is not listed yet");
        m.move_cursor(-1);
        assert!(matches!(m.press(Act::Decline), Step::Refused(_)), "listed rows are not declined");
    }

    #[test]
    fn a_device_without_a_member_name_is_not_revoked_blindly() {
        let mut m = Members::new("main");
        m.set_roster(&roster(vec![device(None, false)], vec![]));
        assert!(matches!(m.press(Act::Revoke), Step::Refused(t) if t.contains("no member name")));
    }

    #[test]
    fn a_refresh_clears_the_pending_confirmation_and_keeps_the_cursor_in_range() {
        let mut m = Members::new("main");
        m.set_roster(&roster(vec![device(Some("laptop"), false)], vec![waiting()]));
        m.move_cursor(1);
        m.press(Act::Decline);
        m.set_roster(&roster(vec![device(Some("laptop"), false)], vec![]));
        assert!(m.prompt().is_none());
        assert_eq!(m.cursor, 0, "the cursor follows the shorter list");
        assert_eq!(m.counts(), (1, 0));
    }
}
