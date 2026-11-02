//! The Machines tab: every saved machine and the conversations its bridge lists. The caller's worker refreshes them
//! while the tab is shown (`machines::tab::Worker`), and Enter or `o` opens one of them in a new herdr tab running
//! `sesync machine attach`.

use super::{App, View};
use crate::herdr_open::{self, PaneShell};
use crate::machines::store::Machine;
use crate::machines::tab::{self, Conversation, Row, Update};
use crate::session_cols::display_safe;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

/// The Machines tab's state. Its rows come from `saved` and `answers`; the cursor counts conversation rows only.
#[derive(Default)]
pub struct MachinesTab {
    /// The saved machines, as the last refresh read them.
    saved: Vec<Machine>,
    /// Why the saved machines could not be read, when they could not.
    saved_error: Option<String>,
    /// Each machine's last answer: its `worktree.ps` result, or why it did not answer.
    answers: BTreeMap<String, Result<Value, String>>,
    /// The cursor over the conversation rows.
    index: usize,
    /// The config directory of this run. Attach tabs are given it as `SESYNC_CONFIG_DIR`, since herdr starts a tab
    /// with its own environment.
    config_dir: Option<PathBuf>,
    /// A refresh is in flight on the worker.
    busy: bool,
    /// When the last refresh was asked for.
    asked: Option<Instant>,
    /// ctrl+r asked for a refresh now.
    refresh_now: bool,
    /// An attach waiting for the caller to run it (see [`App::run_pending_attach`]).
    pending: Option<PendingAttach>,
}

/// A herdr tab waiting to be opened: the argument list of `sesync machine attach` and the tab's label.
struct PendingAttach {
    argv: Vec<String>,
    label: String,
    /// The machine the attach is for: its tab goes in that machine's herdr workspace.
    machine: String,
}

impl App {
    /// The config directory of this run (`SESYNC_CONFIG_DIR`, or the default). Attach tabs read their machines from it.
    pub fn set_config_dir(&mut self, dir: PathBuf) {
        self.machines_tab.config_dir = Some(dir);
    }

    /// The Machines tab's rows, as the view draws them.
    pub fn machine_rows(&self) -> Vec<Row> {
        tab::rows(&self.machines_tab.saved, &self.machines_tab.answers)
    }

    /// How many machines are saved.
    pub fn machine_count(&self) -> usize {
        self.machines_tab.saved.len()
    }

    /// Why the saved machines could not be read, when they could not.
    pub fn machines_error(&self) -> Option<&str> {
        self.machines_tab.saved_error.as_deref()
    }

    /// The cursor over the conversation rows.
    pub fn machine_index(&self) -> usize {
        self.machines_tab.index
    }

    /// The conversation under the cursor.
    pub fn machine_selected(&self) -> Option<Conversation> {
        tab::conversations(&self.machine_rows()).into_iter().nth(self.machines_tab.index).cloned()
    }

    /// Move the cursor over the conversation rows, clamped to them.
    pub(super) fn move_machines(&mut self, delta: isize) {
        let last = tab::conversations(&self.machine_rows()).len().saturating_sub(1) as isize;
        let next = (self.machines_tab.index as isize).saturating_add(delta).clamp(0, last);
        self.machines_tab.index = next as usize;
    }

    /// Enter (`open` false) or `o` (`open` true) on the Machines tab: open the conversation under the cursor in a new
    /// herdr tab on its machine. A closed conversation is opened only by `o`, as the phone's Resume does. This only
    /// says what is about to happen; the caller then calls [`App::run_pending_attach`].
    pub(super) fn enter_machine(&mut self, open: bool) {
        let Some(c) = self.machine_selected() else {
            if self.machines_tab.saved.is_empty() {
                self.say("no machines saved; add one with `sesync machine add <name> <pairing link>`");
            } else {
                self.say("no conversation to open here");
            }
            return;
        };
        if !open && !c.open {
            self.say(format!("{} is not open on {}; press o to open it there", display_safe(&c.title), c.machine));
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                self.fail(format!("cannot find this sesync binary to run in the tab: {e}"));
                return;
            }
        };
        let label = tab::tab_label(&c.machine, &c.title);
        self.say(format!("opening {label} in herdr…"));
        self.machines_tab.pending = Some(PendingAttach { argv: tab::attach_argv(&exe, &c.machine, &c.worktree_id, open), label, machine: c.machine.clone() });
    }

    /// Whether an attach waits for [`App::run_pending_attach`].
    pub fn has_pending_attach(&self) -> bool {
        self.machines_tab.pending.is_some()
    }

    /// Run the attach the last key asked for: a new herdr tab, labelled, that runs the attach command. Without a herdr
    /// that answers, the status line says so and no tab is opened.
    pub fn run_pending_attach(&mut self) {
        let Some(plan) = self.machines_tab.pending.take() else { return };
        let shell = PaneShell::current(&self.roots.home);
        let command = match herdr_open::shell_line(&plan.argv, shell) {
            Ok(command) => command,
            Err(e) => {
                self.fail(e);
                return;
            }
        };
        let config = self.machines_tab.config_dir.as_ref().map(|dir| dir.display().to_string());
        let env: Vec<(&str, &str)> = config.iter().map(|dir| ("SESYNC_CONFIG_DIR", dir.as_str())).collect();
        // Bridge tabs go in the machine's own workspace, created on first use; a failed lookup means herdr is not answering.
        let workspace = match self.herdr.machine_workspace(&plan.machine, &self.roots.home) {
            Ok(id) => id,
            Err(e) => {
                self.fail(format!("no herdr answers from here ({e}), so {} was not opened", plan.label));
                return;
            }
        };
        let (tab, pane) = match self.herdr.create_tab_in_workspace(Some(&workspace), &self.roots.home, &plan.label, &env) {
            Ok(ids) => ids,
            Err(e) => {
                self.fail(format!("no herdr answers from here ({e}), so {} was not opened", plan.label));
                return;
            }
        };
        if let Err(e) = self.herdr.run_in_pane(&pane, &command) {
            let _ = self.herdr.close_tab(&tab);
            self.fail(format!("could not start the attach in herdr: {e}"));
            return;
        }
        match self.herdr.focus_tab(&tab) {
            Ok(()) => self.say(format!("opened {} in a new herdr tab", plan.label)),
            Err(e) => self.fail(format!("opened {} in a new herdr tab, but could not switch to it ({e})", plan.label)),
        }
        // The tab is open, so the popup has done its job: close it like the other herdr opens do.
        self.quit = true;
    }

    /// Whether the Machines tab should ask its worker for a refresh now: the tab is shown, no refresh is in flight,
    /// and either the interval has passed or ctrl+r asked.
    pub fn machines_due(&self, now: Instant) -> bool {
        let t = &self.machines_tab;
        self.view == View::Machines
            && !t.busy
            && (t.refresh_now || t.asked.map_or(true, |at| now.saturating_duration_since(at) >= tab::EVERY))
    }

    /// The caller asked the worker for a refresh at `now`. No other is asked until its answers end.
    pub fn machines_asked(&mut self, now: Instant) {
        let t = &mut self.machines_tab;
        t.busy = true;
        t.asked = Some(now);
        t.refresh_now = false;
    }

    /// Apply what the worker reported since the last call (see [`Update`]).
    pub fn apply_machine_updates(&mut self, updates: Vec<Update>) {
        let t = &mut self.machines_tab;
        for update in updates {
            match update {
                Update::Saved(Ok(saved)) => {
                    let names: Vec<String> = saved.iter().map(|m| m.name.clone()).collect();
                    t.answers.retain(|name, _| names.contains(name));
                    t.saved = saved;
                    t.saved_error = None;
                }
                Update::Saved(Err(why)) => {
                    t.saved.clear();
                    t.answers.clear();
                    t.saved_error = Some(why);
                }
                Update::Answer { name, result } => {
                    t.answers.insert(name, result);
                }
                Update::Done => t.busy = false,
            }
        }
    }

    /// ctrl+r on the Machines tab: refresh at the next frame, not at the next interval.
    pub(super) fn refresh_machines_now(&mut self) {
        self.machines_tab.refresh_now = true;
    }
}
