//! Session titles read off the TUI's thread. The list is drawn from what is known without opening
//! a conversation (`manage::list_groups_deferred`); this worker reads the rest, in the order asked
//! (rows on screen first), and the TUI applies each answer between frames. Opening the TUI never
//! waits on reading every session file.

use crate::manage::{read_title, TitleRead};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

pub struct Worker {
    wanted: Sender<Vec<(String, String)>>,
    read: Receiver<TitleRead>,
    /// Conversations asked for and not answered yet.
    pending: Arc<AtomicUsize>,
}

impl Worker {
    pub fn start() -> Worker {
        let (wanted, asked) = mpsc::channel::<Vec<(String, String)>>();
        let (answer, read) = mpsc::channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let left = Arc::clone(&pending);
        std::thread::spawn(move || {
            for batch in asked {
                for (key, fmt) in batch {
                    // A conversation gone since it was listed has no title to report; the next
                    // listing drops its row.
                    let found = read_title(&key, &fmt);
                    left.fetch_sub(1, Ordering::AcqRel);
                    if let Some(read) = found {
                        if answer.send(read).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Worker { wanted, read, pending }
    }

    /// Read these titles, in this order.
    pub fn request(&self, batch: Vec<(String, String)>) {
        if batch.is_empty() {
            return;
        }
        self.pending.fetch_add(batch.len(), Ordering::AcqRel);
        if self.wanted.send(batch).is_err() {
            self.pending.store(0, Ordering::Release);
        }
    }

    /// Every title read since the last call, without waiting.
    pub fn take(&self) -> Vec<TitleRead> {
        self.read.try_iter().collect()
    }

    /// Nothing asked for is still being read.
    pub fn idle(&self) -> bool {
        self.pending.load(Ordering::Acquire) == 0
    }
}
