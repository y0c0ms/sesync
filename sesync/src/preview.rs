//! The session preview in the details pane: the highlighted conversation's last turns. A worker
//! thread reads them, so the list never waits on a large transcript or a conversion, and reads
//! them again when the conversation changes on disk: a new message shows without a keystroke.

use crate::harness::{self, Kind, Locations, Message, Tools};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// How many turns the preview holds.
pub const TURNS: usize = 20;
/// How often the highlighted conversation is checked for a change.
const RECHECK: Duration = Duration::from_secs(1);

/// What the details pane shows for the highlighted conversation.
#[derive(Default)]
pub struct Preview {
    /// The conversation shown (its key); empty before the first selection.
    pub key: String,
    pub messages: Vec<Message>,
    /// Why the conversation could not be read, when it could not.
    pub error: Option<String>,
    /// Set from the request until the first answer for it arrives.
    pub loading: bool,
    /// Lines scrolled back from the newest; 0 shows the end.
    pub scroll: usize,
    /// The largest useful `scroll`, as the last frame measured it (the pane's width decides how
    /// the text wraps, and only the drawing knows it).
    pub max_scroll: Cell<usize>,
}

/// An answer from the worker: the conversation it read and what it found.
pub struct Loaded {
    pub key: String,
    pub messages: Result<Vec<Message>, String>,
}

/// The thread that reads previews. Dropping it stops the thread.
pub struct Worker {
    select: Sender<(String, String)>,
    loaded: Receiver<Loaded>,
}

impl Worker {
    /// Start the thread. It reads with the same locations and converter the TUI uses.
    pub fn start(locations: Locations, home: PathBuf, sesync_conv: PathBuf) -> Worker {
        let (select, wanted) = mpsc::channel::<(String, String)>();
        let (answer, loaded) = mpsc::channel();
        std::thread::spawn(move || {
            let scratch = std::env::temp_dir().join(format!("sesync-preview-{}", std::process::id()));
            let tools = Tools { home: &home, sesync_conv: &sesync_conv };
            let mut current: Option<(String, String)> = None;
            // The stamp the last answer was read at; None until the current selection is read.
            let mut read_at: Option<Option<String>> = None;
            loop {
                match wanted.recv_timeout(RECHECK) {
                    Ok(next) => {
                        current = Some(next);
                        read_at = None;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                // Several selections in a row (the cursor held down): only the last one is read.
                while let Ok(next) = wanted.try_recv() {
                    current = Some(next);
                    read_at = None;
                }
                let Some((fmt, key)) = &current else { continue };
                let Some(h) = harness::get(fmt) else { continue };
                let loc = locations.of(fmt);
                let stamp = if h.kind() == Kind::Store {
                    h.fingerprint(loc, key)
                } else {
                    Some(crate::mirror::copy_stamp(fmt, Path::new(key)))
                };
                if read_at.as_ref() == Some(&stamp) {
                    continue;
                }
                read_at = Some(stamp);
                let messages = h.recent_messages(loc, &tools, key, &scratch, TURNS);
                if answer.send(Loaded { key: key.clone(), messages }).is_err() {
                    break;
                }
            }
            let _ = std::fs::remove_dir_all(&scratch);
        });
        Worker { select, loaded }
    }

    /// Show this conversation from now on. Returns at once: the reading happens on the worker.
    pub fn select(&self, fmt: &str, key: &str) {
        let _ = self.select.send((fmt.to_string(), key.to_string()));
    }

    /// The newest answer, if one arrived since the last call.
    pub fn take(&self) -> Option<Loaded> {
        let mut newest = None;
        while let Ok(l) = self.loaded.try_recv() {
            newest = Some(l);
        }
        newest
    }
}
