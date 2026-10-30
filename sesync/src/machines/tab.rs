//! The Machines tab's data: every saved machine, the conversations its bridge listed (`worktree.ps`), the rows
//! the tab draws from them, and the command a herdr tab runs to attach to one. The network calls run on a worker
//! thread (`Worker`), so the TUI never waits on a machine (invariant 22). The rows and the command are plain
//! functions, tested without a terminal or a network.

use super::client;
use super::store::{self, Machine};
use crate::session_cols::display_safe;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

/// How long the tab waits between refreshes while it is shown. ctrl+r refreshes at once.
pub const EVERY: Duration = Duration::from_secs(5);
/// How long one machine may take to answer a refresh: connect, pair and list. A slow or absent machine shows as
/// unreachable; it never holds up the others.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// The longest tab label, in characters.
const LABEL_MAX: usize = 40;

/// One line of the tab: a machine's header, or one of its conversations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Machine { name: String, endpoint: String, status: Status },
    Conversation(Conversation),
}

/// Where a machine stands at its last refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No refresh has answered yet.
    Loading,
    /// The last refresh failed, with the reason the client gave.
    Unreachable(String),
    /// The last refresh listed the machine's conversations.
    Listed,
}

/// A conversation on a machine, as its `worktree.ps` row describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub machine: String,
    pub worktree_id: String,
    pub title: String,
    /// The first open pane's state (`working`, `blocked`, `waiting` or `done`), or `closed` when no pane runs it.
    pub state: String,
    /// Whether a herdr pane on that machine runs the conversation.
    pub open: bool,
    pub harness: String,
    pub path: String,
}

/// The tab's rows: every saved machine in file order, each followed by the conversations its last answer listed. A
/// machine with no answer yet is `Loading`. One whose last refresh failed is `Unreachable` and lists nothing, so its
/// older conversations never pass for current ones.
pub fn rows(machines: &[Machine], answers: &BTreeMap<String, Result<Value, String>>) -> Vec<Row> {
    let mut out = Vec::new();
    for m in machines {
        let header = |status: Status| Row::Machine { name: m.name.clone(), endpoint: m.endpoint.clone(), status };
        match answers.get(&m.name) {
            None => out.push(header(Status::Loading)),
            Some(Err(why)) => out.push(header(Status::Unreachable(why.clone()))),
            Some(Ok(list)) => {
                out.push(header(Status::Listed));
                for row in list["worktrees"].as_array().into_iter().flatten() {
                    out.push(Row::Conversation(conversation(&m.name, row)));
                }
            }
        }
    }
    out
}

/// One `worktree.ps` row. Its state is the first open pane's; a conversation with no pane is `closed`.
fn conversation(machine: &str, row: &Value) -> Conversation {
    let text = |key: &str| row[key].as_str().unwrap_or_default().to_string();
    let (state, open) = match row["agents"].as_array().and_then(|agents| agents.first()) {
        Some(agent) => (agent["state"].as_str().unwrap_or("open").to_string(), true),
        None => ("closed".to_string(), false),
    };
    Conversation {
        machine: machine.to_string(),
        worktree_id: text("worktreeId"),
        title: text("displayName"),
        state,
        open,
        harness: text("conversationAgent"),
        path: text("path"),
    }
}

/// The conversation rows in order: the rows the tab's cursor moves over.
pub fn conversations(rows: &[Row]) -> Vec<&Conversation> {
    rows.iter()
        .filter_map(|row| match row {
            Row::Conversation(c) => Some(c),
            Row::Machine { .. } => None,
        })
        .collect()
}

/// The argument list of the command a herdr tab runs to show `worktree` on `machine`: this binary, `machine attach`,
/// and `--open` only for `o`, which may start a closed conversation there.
pub fn attach_argv(exe: &Path, machine: &str, worktree: &str, open: bool) -> Vec<String> {
    let mut argv = vec![exe.display().to_string(), "machine".into(), "attach".into(), machine.into(), worktree.into()];
    if open {
        argv.push("--open".into());
    }
    argv
}

/// The herdr tab a remote terminal opens in: `<machine> · <title>`, cut to [`LABEL_MAX`] characters.
pub fn tab_label(machine: &str, title: &str) -> String {
    let text: String = display_safe(&format!("{machine} · {title}")).chars().filter(|c| !c.is_control()).collect();
    if text.chars().count() <= LABEL_MAX {
        return text;
    }
    let cut: String = text.chars().take(LABEL_MAX - 1).collect();
    format!("{cut}…")
}

/// What the worker reports, in order: the saved machines it read, one answer per machine, then `Done`.
pub enum Update {
    Saved(Result<Vec<Machine>, String>),
    Answer { name: String, result: Result<Value, String> },
    Done,
}

/// The thread that refreshes the tab. Each request reads the saved machines and asks all of them at once, each with
/// its own timeout, so one slow machine delays only its own answer.
pub struct Worker {
    asked: Sender<()>,
    answers: Receiver<Update>,
}

impl Worker {
    /// Start the thread. `machines_file` is `<config>/machines.json`.
    pub fn start(machines_file: PathBuf) -> Worker {
        let (asked, requests) = mpsc::channel::<()>();
        let (answer, answers) = mpsc::channel::<Update>();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
            for () in requests {
                match (store::load(&machines_file), &runtime) {
                    (Ok(machines), Ok(runtime)) => {
                        let _ = answer.send(Update::Saved(Ok(machines.clone())));
                        runtime.block_on(futures_util::future::join_all(machines.iter().map(|m| {
                            let answer = answer.clone();
                            async move {
                                let result = fetch(m).await;
                                let _ = answer.send(Update::Answer { name: m.name.clone(), result });
                            }
                        })));
                    }
                    (Err(why), _) => {
                        let _ = answer.send(Update::Saved(Err(why)));
                    }
                    (Ok(_), Err(why)) => {
                        let _ = answer.send(Update::Saved(Err(format!("cannot start the network: {why}"))));
                    }
                }
                let _ = answer.send(Update::Done);
            }
        });
        Worker { asked, answers }
    }

    /// Refresh every machine, on the worker's thread. Returns at once.
    pub fn request(&self) {
        let _ = self.asked.send(());
    }

    /// Everything the worker reported since the last call, without waiting.
    pub fn take(&self) -> Vec<Update> {
        self.answers.try_iter().collect()
    }
}

/// One machine's `worktree.ps` answer, or why it did not come within [`FETCH_TIMEOUT`].
async fn fetch(machine: &Machine) -> Result<Value, String> {
    let listing = async {
        let (mut writer, mut reader) = client::connect(machine).await?;
        client::call(&mut writer, &mut reader, "worktree.ps", json!({})).await
    };
    tokio::time::timeout(FETCH_TIMEOUT, listing)
        .await
        .map_err(|_| format!("{} did not answer within {}s", machine.name, FETCH_TIMEOUT.as_secs()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr_open::{shell_line, PaneShell};

    fn machine(name: &str) -> Machine {
        Machine { name: name.into(), endpoint: format!("ws://{name}:6768"), device_token: "t".into(), public_key_b64: "k".into(), grant_device_id: None }
    }

    #[test]
    fn a_conversation_shows_its_first_agents_state_and_one_with_no_pane_is_closed() {
        let list = json!({ "worktrees": [
            { "worktreeId": "omp::a", "displayName": "Main agent", "conversationAgent": "omp", "path": "/w/a",
              "agents": [{ "paneKey": "w1:p1", "state": "blocked" }, { "paneKey": "w1:p2", "state": "done" }] },
            { "worktreeId": "claude::b", "displayName": "Older", "conversationAgent": "claude", "path": "/w/b", "agents": [] },
        ]});
        let rows = rows(&[machine("local")], &BTreeMap::from([("local".to_string(), Ok(list))]));
        let conv = conversations(&rows);
        assert_eq!(conv.len(), 2, "every conversation has a row, open or closed");
        assert_eq!((conv[0].state.as_str(), conv[0].open), ("blocked", true), "the first pane's state wins");
        assert_eq!((conv[1].state.as_str(), conv[1].open), ("closed", false));
        assert_eq!((conv[0].worktree_id.as_str(), conv[0].harness.as_str()), ("omp::a", "omp"));
        assert_eq!((conv[1].title.as_str(), conv[1].path.as_str()), ("Older", "/w/b"));
    }

    #[test]
    fn a_machine_that_did_not_answer_lists_nothing_and_says_why_while_the_others_still_list() {
        let list = json!({ "worktrees": [{ "worktreeId": "omp::a", "displayName": "Main agent", "agents": [] }] });
        let answers = BTreeMap::from([
            ("work-pc".to_string(), Err("work-pc did not answer within 8s".to_string())),
            ("local".to_string(), Ok(list)),
        ]);
        let rows = rows(&[machine("work-pc"), machine("local"), machine("late")], &answers);
        assert_eq!(rows.len(), 4, "three headers and one conversation, in file order: {rows:?}");
        assert_eq!(
            rows[0],
            Row::Machine { name: "work-pc".into(), endpoint: "ws://work-pc:6768".into(), status: Status::Unreachable("work-pc did not answer within 8s".into()) }
        );
        assert!(matches!(&rows[1], Row::Machine { name, status: Status::Listed, .. } if name == "local"));
        assert!(matches!(&rows[2], Row::Conversation(c) if c.worktree_id == "omp::a"));
        assert!(matches!(rows[3], Row::Machine { ref name, status: Status::Loading, .. } if name == "late"));
    }

    #[test]
    fn enter_and_o_run_the_same_attach_and_o_adds_open_quoted_for_the_pane_shell() {
        let exe = Path::new("/home/u/my tools/sesync");
        let enter = shell_line(&attach_argv(exe, "local", "omp::a b", false), PaneShell::Posix).unwrap();
        let open = shell_line(&attach_argv(exe, "local", "omp::a b", true), PaneShell::Posix).unwrap();
        assert_eq!(enter, "'/home/u/my tools/sesync' machine attach local 'omp::a b'");
        assert_eq!(open, "'/home/u/my tools/sesync' machine attach local 'omp::a b' --open");
        let ps = shell_line(&attach_argv(exe, "local", "omp::a b", false), PaneShell::PowerShell).unwrap();
        assert_eq!(ps, "& '/home/u/my tools/sesync' 'machine' 'attach' 'local' 'omp::a b'", "PowerShell calls the program with &, each argument quoted");
    }
}
