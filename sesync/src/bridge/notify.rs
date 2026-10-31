//! In-app notifications and their ntfy push. The bridge watches herdr's agent statuses. When a pane
//! running a conversation moves from `working` to idle or done, the conversation has finished. When
//! it moves to `blocked`, it needs input. Each such change is an event: it gets the next sequence
//! number, is kept in a bounded history, is streamed to every open `notifications.subscribe` in the
//! desktop's `MobileNotificationDispatchEvent` shape, and is pushed to ntfy when a push target is
//! configured.
//!
//! The first status seen for a pane records it and raises nothing, so a bridge restart does not
//! announce every agent that was already running. A pane that disappears is forgotten silently.

use super::catalog::Conversation;
use super::ntfy;
use super::rpc::{Ctx, Failure};
use super::stream::{self, Conn, Sink, CATALOG_REFRESH};
use super::herdr::{self, Change, StatusSource};
use super::terminals::{self, Running};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

/// How many events the history keeps; older ones are dropped.
pub const HISTORY: usize = 200;
/// How often a subscription checks its stop flag while no event is due.
const FEED_POLL: Duration = Duration::from_millis(250);

/// The event kinds the watcher raises.
pub const FINISHED: &str = "finished";
pub const NEEDS_INPUT: &str = "needs-input";
/// The kind of the synthetic event `notifications.testPush` sends. Never recorded in the history.
pub const TEST: &str = "test";

/// What a herdr status change means, if it means anything to the phone. `prev` is the status the
/// pane had at the last observation, `now` the one it has now.
pub fn transition(prev: &str, now: &str) -> Option<&'static str> {
    match (prev, now) {
        ("working", "idle") | ("working", "done") => Some(FINISHED),
        // Any change into a prompt the agent waits on: the spec's "->blocked = needs input". A pane that
        // was already blocked is not a new prompt.
        (prev, "blocked") if prev != "blocked" => Some(NEEDS_INPUT),
        _ => None,
    }
}

/// The heading an event shows: the title of its notification, in the app and in ntfy.
fn heading(kind: &str) -> &'static str {
    match kind {
        NEEDS_INPUT => "Agent needs input",
        _ => "Agent finished",
    }
}

/// The desktop's `agentState` for an event kind: `blocked` is needs input, `done` is finished.
fn agent_state(kind: &str) -> &'static str {
    match kind {
        NEEDS_INPUT => "blocked",
        _ => "done",
    }
}

/// The body an event shows: the conversation's title, or a placeholder when it has none.
fn body_of(title: &str) -> &str {
    if title.is_empty() {
        "(untitled)"
    } else {
        title
    }
}

/// One notification, as the history keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub seq: u64,
    pub kind: &'static str,
    pub worktree_id: String,
    /// The conversation's title.
    pub title: String,
    pub repo: String,
    /// The pane's terminal id; the phone opens it on tap.
    pub terminal_id: String,
    /// Milliseconds since the epoch.
    pub at: f64,
}

impl Event {
    /// The desktop's `MobileNotificationDispatchEvent` for this event. `epoch` is the hub's epoch,
    /// which names the counter `notificationSeq` belongs to.
    pub fn to_json(&self, epoch: &str) -> Value {
        if self.kind == TEST {
            return json!({
                "type": "notification",
                "source": "test",
                "title": "Test notification",
                "body": "Notifications from this host reach this app.",
                "notificationId": format!("test:{epoch}:{}", self.seq),
                "notificationEpoch": epoch,
                "emittedAt": self.at,
            });
        }
        json!({
            "type": "notification",
            "source": "agent",
            "title": heading(self.kind),
            "body": body_of(&self.title),
            "worktreeId": self.worktree_id,
            "paneKey": self.terminal_id,
            "notificationId": format!("{epoch}:{}", self.seq),
            "notificationSeq": self.seq,
            "notificationEpoch": epoch,
            "agentState": agent_state(self.kind),
            "emittedAt": self.at,
        })
    }
}

/// A transition the watcher found: the kind, the conversation the pane runs, and the pane's terminal id.
pub struct Raised<'a> {
    pub kind: &'static str,
    pub conversation: &'a Conversation,
    pub terminal_id: String,
}

/// The watcher's memory: the herdr status each pane had at the last observation.
#[derive(Default)]
pub struct Watch {
    last: HashMap<String, String>,
}

impl Watch {
    /// Record this pass's panes and return the transitions that belong to a conversation. The status
    /// map is replaced, so a pane absent from `panes` is forgotten without an event.
    pub fn observe<'a>(&mut self, panes: &[Running], conversations: &'a [Conversation]) -> Vec<Raised<'a>> {
        let mut raised = Vec::new();
        let mut next = HashMap::new();
        for running in panes {
            let id = running.pane.terminal_id.clone();
            let status = running.pane.agent_status.clone();
            if let Some(kind) = self.last.get(&id).and_then(|prev| transition(prev, &status)) {
                if let Some(conversation) = terminals::conversation_of(running, conversations) {
                    raised.push(Raised { kind, conversation, terminal_id: id.clone() });
                }
            }
            next.insert(id, status);
        }
        self.last = next;
        raised
    }
}

struct Subscriber {
    tx: mpsc::Sender<Event>,
    stop: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    next_seq: u64,
    test_seq: u64,
    events: VecDeque<Event>,
    subscribers: Vec<Subscriber>,
}

struct Inner {
    /// Identifies this process's counter. A restart changes it, so a stale cursor is never trusted.
    epoch: String,
    push: Option<ntfy::Config>,
    state: Mutex<State>,
}

/// The notification history, its live subscribers, and the optional ntfy push. Cheap to clone.
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

impl Hub {
    /// A hub that keeps the in-app stream only (`None`) or also pushes every event to ntfy.
    pub fn new(push: Option<ntfy::Config>) -> Hub {
        let epoch = format!("notify-{}-{}", terminals::now_ms() as u64, std::process::id());
        Hub { inner: Arc::new(Inner { epoch, push, state: Mutex::new(State::default()) }) }
    }

    /// Record a transition: assign the next sequence number, keep it in the history, send it to every
    /// live subscriber, and push it to ntfy off the calling thread.
    pub fn emit(&self, kind: &'static str, conversation: &Conversation, terminal_id: &str, at: f64) -> Event {
        let event = {
            let mut state = self.inner.state.lock();
            state.next_seq += 1;
            let event = Event {
                seq: state.next_seq,
                kind,
                worktree_id: conversation.worktree_id(),
                title: conversation.title.clone(),
                repo: conversation.repo_id(),
                terminal_id: terminal_id.to_string(),
                at,
            };
            state.events.push_back(event.clone());
            while state.events.len() > HISTORY {
                state.events.pop_front();
            }
            state.subscribers.retain(|s| !s.stop.load(Ordering::Relaxed) && s.tx.send(event.clone()).is_ok());
            event
        };
        if let Some(config) = self.inner.push.clone() {
            let (title, priority, tags) = push_shape(kind);
            let body = body_of(&event.title).to_string();
            thread::spawn(move || {
                if let Err(e) = ntfy::publish(&config, title, &body, priority, tags) {
                    eprintln!("sesync bridge: ntfy push failed: {e}");
                }
            });
        }
        event
    }

    /// Register a live subscriber and return this process's epoch. Registration happens under the
    /// history lock, so every event emitted after this call reaches the subscriber.
    fn attach(&self, tx: mpsc::Sender<Event>, stop: Arc<AtomicBool>) -> String {
        let mut state = self.inner.state.lock();
        state.subscribers.retain(|s| !s.stop.load(Ordering::Relaxed));
        state.subscribers.push(Subscriber { tx, stop });
        self.inner.epoch.clone()
    }

    /// The retained events after `last_seen`. A different `epoch` means the cursor belongs to an
    /// earlier process, so every retained event is returned. Returns the events and this epoch.
    pub fn missed_since(&self, last_seen: u64, epoch: Option<&str>) -> (Vec<Event>, String) {
        let state = self.inner.state.lock();
        let stale = epoch.is_some_and(|e| e != self.inner.epoch);
        let events: Vec<Event> = state.events.iter().filter(|e| stale || e.seq > last_seen).cloned().collect();
        (events, self.inner.epoch.clone())
    }

    /// Send a test notification to every live subscriber and return how many took it. It is not kept in
    /// the history, takes no sequence number, and never goes to ntfy.
    pub fn test_push(&self, at: f64) -> usize {
        let mut state = self.inner.state.lock();
        state.test_seq += 1;
        let event = Event {
            seq: state.test_seq,
            kind: TEST,
            worktree_id: String::new(),
            title: String::new(),
            repo: String::new(),
            terminal_id: String::new(),
            at,
        };
        let mut delivered = 0;
        state.subscribers.retain(|s| {
            let live = !s.stop.load(Ordering::Relaxed) && s.tx.send(event.clone()).is_ok();
            delivered += usize::from(live);
            live
        });
        delivered
    }
}

/// ntfy's priority and tags for each event kind. The title and body are `heading` and `body_of`.
fn push_shape(kind: &str) -> (&'static str, u8, &'static [&'static str]) {
    match kind {
        NEEDS_INPUT => (heading(kind), 4, &["raised_hand"][..]),
        _ => (heading(kind), 3, &["white_check_mark"][..]),
    }
}

/// `notifications.subscribe`: the first frame is `ready` (the subscription id and the epoch); each
/// later event is one notification frame, until the subscription stops. Catch-up for events missed
/// before this call is `notifications.getMissedSince`.
pub fn subscribe(_params: &Value, ctx: &Ctx, conn: &Conn, id: &Value) -> Result<(), Failure> {
    static SUBSCRIPTIONS: AtomicU64 = AtomicU64::new(0);
    let key = format!("notifications-{}", SUBSCRIPTIONS.fetch_add(1, Ordering::Relaxed) + 1);
    let stop = conn.register(key.clone(), id.clone());
    let (tx, rx) = mpsc::channel();
    let epoch = ctx.notify.attach(tx, Arc::clone(&stop));
    let ready = json!({ "type": "ready", "subscriptionId": key, "epoch": epoch });
    let _ = conn.sink.json(&stream::streamed(id, ready));
    let feed = Feed { sink: conn.sink.clone(), request_id: id.clone(), epoch, stop, rx };
    thread::spawn(move || run_feed(feed));
    Ok(())
}

struct Feed {
    sink: Sink,
    request_id: Value,
    epoch: String,
    stop: Arc<AtomicBool>,
    rx: mpsc::Receiver<Event>,
}

fn run_feed(t: Feed) {
    loop {
        if t.stop.load(Ordering::Relaxed) {
            break;
        }
        match t.rx.recv_timeout(FEED_POLL) {
            Ok(event) => {
                if !t.sink.json(&stream::streamed(&t.request_id, event.to_json(&t.epoch))) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    t.sink.json(&stream::end(&t.request_id));
}

/// `notifications.getMissedSince`: the events after `lastSeenSeq`, with this host's epoch, as the
/// desktop's notification shape. The app also sends `deliveredPushes` to reconcile tray
/// notifications; this host keeps no dismissals, so it reports an empty `dismissedPushes`.
pub fn get_missed_since(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    missed_reply(&ctx.notify, params)
}

/// `notifications.testPush`: a synthetic notification to every open `notifications.subscribe` stream.
/// `accepted` is false when no stream is open, so the phone reports the test as not delivered.
pub fn test_push(_params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let delivered = ctx.notify.test_push(terminals::now_ms());
    Ok(if delivered > 0 {
        json!({ "accepted": true })
    } else {
        json!({ "accepted": false, "reason": "unavailable" })
    })
}

fn missed_reply(hub: &Hub, params: &Value) -> Result<Value, Failure> {
    let last_seen = params["lastSeenSeq"]
        .as_u64()
        .ok_or_else(|| ("bad_request", "lastSeenSeq must be a non-negative integer".to_string()))?;
    let epoch = match params.get("epoch") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_str().ok_or_else(|| ("bad_request", "epoch must be a string".to_string()))?),
    };
    let (events, current) = hub.missed_since(last_seen, epoch);
    let mut reply = json!({
        "notifications": events.iter().map(|e| e.to_json(&current)).collect::<Vec<Value>>(),
        "epoch": current,
    });
    if params.get("deliveredPushes").is_some_and(Value::is_array) {
        reply["dismissedPushes"] = json!([]);
    }
    Ok(reply)
}

/// How long the pane list may go unread while nothing changes: the safety net for a change herdr reports on no
/// event. Panes that open, close or change status are read as herdr reports them.
const SNAPSHOT_EVERY: Duration = Duration::from_secs(30);

/// Start the watcher: one background thread.
pub fn spawn_watcher(ctx: Ctx, every: Duration) {
    thread::spawn(move || watch(&ctx, every));
}

/// What the watcher keeps between changes: the conversations (read while an agent runs), the panes with their
/// processes (read when herdr reports a change to them), the status subscription, and the transition memory.
#[derive(Default)]
struct Watcher {
    memory: Watch,
    conversations: Vec<Conversation>,
    catalog_at: Option<Instant>,
    catalog_error: Option<String>,
    panes: Vec<Running>,
    labels: HashMap<String, String>,
    snapshot_at: Option<Instant>,
    herdr_error: Option<String>,
    /// The pane ids the subscription names, and the subscription itself (None when herdr has none to give).
    subscribed: Vec<String>,
    events: Option<Box<dyn StatusSource>>,
    /// A change asked for the pane list to be read again.
    pending: bool,
}

impl Watcher {
    /// Whether some pane has an agent in it. Only an agent's pane changes status, so only then are the
    /// conversations needed to tell which conversation a change belongs to.
    fn agents_run(&self) -> bool {
        self.panes.iter().any(|r| r.pane.agent.is_some() || r.pane.agent_status != "unknown")
    }

    /// Whether the conversations are due: every `CATALOG_REFRESH` while an agent runs, and before a change is
    /// observed, so a conversation that appeared since the last read is known when its pane changes.
    fn catalog_due(&self) -> bool {
        (self.pending || self.agents_run()) && self.catalog_at.is_none_or(|at| at.elapsed() >= CATALOG_REFRESH)
    }

    fn snapshot_due(&self) -> bool {
        self.snapshot_at.is_none_or(|at| at.elapsed() >= SNAPSHOT_EVERY)
    }

    /// The time to wait for a change before the next timed read is due.
    fn until_next_read(&self) -> Duration {
        let snapshot = self.snapshot_at.map_or(Duration::ZERO, |at| (at + SNAPSHOT_EVERY).saturating_duration_since(Instant::now()));
        if !self.agents_run() {
            return snapshot;
        }
        let catalog = self.catalog_at.map_or(Duration::ZERO, |at| (at + CATALOG_REFRESH).saturating_duration_since(Instant::now()));
        snapshot.min(catalog)
    }

    /// Re-read the conversations. True when what the pane rules match changed, so the panes must be read again.
    fn read_catalog(&mut self, ctx: &Ctx) -> bool {
        self.catalog_at = Some(Instant::now());
        match ctx.catalog.conversations() {
            Ok(list) => {
                self.catalog_error = None;
                let changed = !same_matching(&self.conversations, &list);
                self.conversations = list;
                changed
            }
            Err(e) => {
                if self.catalog_error.as_deref() != Some(e.as_str()) {
                    eprintln!("sesync bridge: notifications: cannot read conversations: {e}");
                }
                self.catalog_error = Some(e);
                false
            }
        }
    }

    /// Read the pane list and the processes of its panes. The status subscription names the panes listed, so a
    /// pane that opened is subscribed to; the list is read once more after subscribing, so a pane that opened
    /// meanwhile is not missed.
    fn read_panes(&mut self, ctx: &Ctx) -> Result<(), String> {
        let h = ctx.herdr.as_ref();
        let mut listed = herdr::list_panes(h)?;
        for _ in 0..3 {
            let ids = watched_ids(&listed);
            if self.events.is_some() && self.subscribed == ids {
                break;
            }
            self.events = None;
            self.subscribed = ids.clone();
            // Without an event stream (a canned or unreachable herdr) the list is read on each pass.
            self.events = ctx.herdr.status_events(&ids).ok();
            if self.events.is_none() {
                break;
            }
            let again = herdr::list_panes(h)?;
            if watched_ids(&again) == ids {
                break;
            }
            listed = again;
        }
        let (panes, labels) = terminals::running_listed(h, listed, &self.conversations)?;
        self.panes = panes;
        self.labels = labels;
        self.snapshot_at = Some(Instant::now());
        Ok(())
    }

    /// Apply one status change to its pane. True when the pane list must be read again: a pane not listed, or a
    /// status that moved (a turn started or ended, and its session may have changed). A title or agent change only
    /// re-places the pane, from the rules over what is already read.
    fn apply(&mut self, pane_id: &str, status: &str, agent: Option<String>, title: Option<String>) -> bool {
        let Some(running) = self.panes.iter_mut().find(|r| r.pane.pane_id == pane_id) else { return true };
        let moved = running.pane.agent_status != status;
        running.pane.agent_status = status.to_string();
        running.pane.agent = agent;
        running.pane.title = title;
        if moved {
            return true;
        }
        terminals::reassign_fallbacks(&mut self.panes, &self.conversations, &self.labels);
        false
    }
}

/// The panes whose status is subscribed to, sorted: those with an agent or a status herdr reports. A pane with
/// neither never changes status, and herdr polls each subscribed pane's status on its own, so a shell pane is not
/// watched. An agent detected in one later is a pane list change, which subscribes to it.
fn watched_ids(panes: &[herdr::Pane]) -> Vec<String> {
    let mut ids: Vec<String> = panes
        .iter()
        .filter(|p| p.agent.is_some() || p.agent_status != "unknown")
        .map(|p| p.pane_id.clone())
        .collect();
    ids.sort();
    ids
}

/// Whether two conversation lists hold the same conversations as the pane rules match them: the same titles,
/// directories and session ids, in any order. A change of time alone does not count.
fn same_matching(a: &[Conversation], b: &[Conversation]) -> bool {
    let keys = |list: &[Conversation]| {
        let mut keys: Vec<(String, String, String, String, Vec<String>)> = list
            .iter()
            .map(|c| (c.harness.clone(), c.key.clone(), c.title.clone(), c.dir.clone(), c.idents.clone()))
            .collect();
        keys.sort();
        keys
    };
    keys(a) == keys(b)
}

/// The watcher loop. The pane list is read when herdr reports that it changed, when a pane's status moved, when
/// the safety net is due, and when the stream is not there. The conversations are read every `CATALOG_REFRESH`
/// while an agent runs. In between, the watcher waits on herdr's events: a title or agent change re-places its pane,
/// and every status herdr reports is seen, however short the turn. Each observation records the baseline and raises
/// the transitions since the last one. Without an event stream the pane list is read every `every`.
fn watch(ctx: &Ctx, every: Duration) {
    let mut w = Watcher { pending: true, ..Watcher::default() };
    loop {
        if w.catalog_due() && w.read_catalog(ctx) {
            w.pending = true;
        }
        if w.pending || w.snapshot_due() || w.events.is_none() {
            if let Err(e) = w.read_panes(ctx) {
                if w.herdr_error.as_deref() != Some(e.as_str()) {
                    eprintln!("sesync bridge: notifications: herdr status unavailable: {e}");
                }
                w.herdr_error = Some(e);
                thread::sleep(every);
                continue;
            }
            w.herdr_error = None;
            w.pending = false;
            emit_raised(ctx, w.memory.observe(&w.panes, &w.conversations));
        }
        let wait = w.until_next_read();
        let Some(events) = w.events.as_mut() else {
            thread::sleep(every);
            continue;
        };
        match events.next(wait) {
            Ok(Some(Change::Status { pane_id, status, agent, title })) => {
                if w.apply(&pane_id, &status, agent, title) {
                    w.pending = true;
                }
            }
            Ok(Some(Change::Panes)) => w.pending = true,
            Ok(None) => {}
            // The stream ended: pause a pass, then read the panes and subscribe again. A dead stream is never spun on.
            Err(_) => {
                w.events = None;
                w.pending = true;
                thread::sleep(every);
            }
        }
    }
}

fn emit_raised(ctx: &Ctx, raised: Vec<Raised<'_>>) {
    for r in raised {
        ctx.notify.emit(r.kind, r.conversation, &r.terminal_id, terminals::now_ms());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::herdr::Pane;

    fn pane(terminal: &str, status: &str) -> Running {
        Running {
            pane: Pane {
                pane_id: format!("p-{terminal}"),
                terminal_id: terminal.into(),
                tab_id: "tab".into(),
                focused: false,
                agent_status: status.into(),
                agent: Some("omp".into()),
                title: None,
                rows: None,
                foreground_cwd: None,
            },
            argvs: Vec::new(),
            prompt: None,
            owner: None,
            fallback: None,
            reported: None,
        }
    }

    fn conversation(title: &str, ident: &str) -> Conversation {
        Conversation {
            harness: "omp".into(),
            key: format!("key-{ident}"),
            title: title.into(),
            dir: "/work/repo".into(),
            updated: 0.0,
            idents: vec![ident.into()],
        }
    }

    /// A pane whose foreground argv carries the conversation's identifier, so it runs that conversation.
    fn running_conversation(terminal: &str, status: &str, ident: &str) -> Running {
        let mut r = pane(terminal, status);
        r.argvs = vec![vec!["omp".into(), "--session".into(), ident.into()]];
        r
    }

    #[test]
    fn the_transition_table_raises_only_finished_and_needs_input() {
        assert_eq!(transition("working", "idle"), Some(FINISHED));
        assert_eq!(transition("working", "done"), Some(FINISHED));
        assert_eq!(transition("working", "blocked"), Some(NEEDS_INPUT));
        assert_eq!(transition("idle", "blocked"), Some(NEEDS_INPUT));
        assert_eq!(transition("blocked", "blocked"), None, "a pane that stays blocked is not a new prompt");
        for (prev, now) in [
            ("idle", "working"),
            ("blocked", "working"),
            ("done", "idle"),
            ("blocked", "idle"),
            ("working", "working"),
            ("working", "unknown"),
            ("unknown", "idle"),
            ("idle", "done"),
        ] {
            assert_eq!(transition(prev, now), None, "{prev} -> {now} raises nothing");
        }
    }

    #[test]
    fn the_first_observation_of_a_pane_records_it_and_raises_nothing() {
        let convs = [conversation("t", "s1")];
        let mut memory = Watch::default();
        assert!(memory.observe(&[running_conversation("term_a", "blocked", "s1")], &convs).is_empty());
        let raised = memory.observe(&[running_conversation("term_a", "working", "s1")], &convs);
        assert!(raised.is_empty());
        let raised = memory.observe(&[running_conversation("term_a", "idle", "s1")], &convs);
        assert_eq!(raised.len(), 1);
        assert_eq!(raised[0].kind, FINISHED);
        assert_eq!(raised[0].terminal_id, "term_a");
    }

    #[test]
    fn a_pane_that_disappears_is_forgotten_and_its_return_is_a_first_observation() {
        let convs = [conversation("t", "s1")];
        let mut memory = Watch::default();
        memory.observe(&[running_conversation("term_a", "working", "s1")], &convs);
        assert!(memory.observe(&[], &convs).is_empty(), "vanishing raises nothing");
        assert!(memory.observe(&[running_conversation("term_a", "idle", "s1")], &convs).is_empty());
        assert!(memory.last.contains_key("term_a"), "the returning pane is recorded again");
    }

    #[test]
    fn a_transition_on_a_pane_that_runs_no_conversation_raises_nothing_but_is_tracked() {
        let convs = [conversation("t", "s1")];
        let mut memory = Watch::default();
        memory.observe(&[pane("term_x", "working")], &convs);
        assert!(memory.observe(&[pane("term_x", "blocked")], &convs).is_empty());
        assert_eq!(memory.last.get("term_x").map(String::as_str), Some("blocked"));
    }

    #[test]
    fn a_catalog_that_only_reorders_or_moves_in_time_is_the_same_for_the_pane_rules() {
        let a = [conversation("a", "s1"), conversation("b", "s2")];
        let mut moved = a.clone();
        moved.reverse();
        moved[0].updated = 99.0;
        assert!(same_matching(&a, &moved), "a turn's new time reorders the list without changing what panes match");
        let mut renamed = a.clone();
        renamed[0].title = "renamed".into();
        assert!(!same_matching(&a, &renamed), "a new title is a change the panes are placed by again");
    }

    #[test]
    fn a_status_event_moves_its_pane_and_raises_the_transition() {
        let convs = [conversation("t", "s1")];
        let mut w = Watcher::default();
        w.panes = vec![running_conversation("term_a", "working", "s1"), pane("term_b", "working")];
        assert!(w.memory.observe(&w.panes, &convs).is_empty());
        assert!(w.apply("p-term_a", "idle", None, None), "a status that moved asks for the panes to be read");
        let raised = w.memory.observe(&w.panes, &convs);
        assert_eq!(raised.len(), 1);
        assert_eq!(raised[0].kind, FINISHED);
        assert_eq!(raised[0].terminal_id, "term_a");
        assert!(w.apply("p-gone", "idle", None, None), "a pane no listing has asks for the panes to be read");
    }

    fn hub_with_three() -> Hub {
        let hub = Hub::new(None);
        let c = conversation("the title", "s1");
        hub.emit(FINISHED, &c, "term_a", 1.0);
        hub.emit(NEEDS_INPUT, &c, "term_a", 2.0);
        hub.emit(FINISHED, &c, "term_b", 3.0);
        hub
    }

    fn seqs(hub: &Hub, cursor: u64) -> Vec<u64> {
        hub.missed_since(cursor, None).0.iter().map(|e| e.seq).collect()
    }

    #[test]
    fn sequence_numbers_are_monotonic_from_one() {
        assert_eq!(seqs(&hub_with_three(), 0), vec![1, 2, 3]);
    }

    #[test]
    fn a_cursor_returns_only_the_events_after_it() {
        let hub = hub_with_three();
        assert_eq!(seqs(&hub, 1), vec![2, 3]);
        assert_eq!(seqs(&hub, 2), vec![3]);
        assert!(seqs(&hub, 3).is_empty());
        assert!(seqs(&hub, 99).is_empty(), "a cursor past the newest event returns nothing");
    }

    #[test]
    fn a_different_epoch_returns_the_whole_history() {
        let hub = hub_with_three();
        let (all, epoch) = hub.missed_since(3, Some("an-old-process"));
        assert_eq!(all.len(), 3);
        assert_ne!(epoch, "an-old-process");
        let (same, current) = hub.missed_since(3, Some(&epoch));
        assert!(same.is_empty(), "the current epoch honours the cursor");
        assert_eq!(current, epoch);
    }

    #[test]
    fn the_history_keeps_the_newest_two_hundred_events() {
        let hub = Hub::new(None);
        let c = conversation("t", "s1");
        for i in 0..(HISTORY as u64 + 5) {
            hub.emit(FINISHED, &c, "term_a", i as f64);
        }
        let (kept, _) = hub.missed_since(0, None);
        assert_eq!(kept.len(), HISTORY);
        assert_eq!(kept[0].seq, 6, "the five oldest were dropped");
        assert_eq!(kept.last().unwrap().seq, HISTORY as u64 + 5);
    }

    #[test]
    fn an_event_frame_has_the_desktop_shape_the_app_reads() {
        let hub = Hub::new(None);
        let c = conversation("Fix the parser", "s1");
        let event = hub.emit(NEEDS_INPUT, &c, "term_a", 42.0);
        let epoch = hub.missed_since(0, None).1;
        assert_eq!(
            event.to_json(&epoch),
            json!({
                "type": "notification", "source": "agent",
                "title": "Agent needs input", "body": "Fix the parser",
                "worktreeId": c.worktree_id(), "paneKey": "term_a",
                "notificationId": format!("{epoch}:1"), "notificationSeq": 1,
                "notificationEpoch": epoch, "agentState": "blocked", "emittedAt": 42.0,
            })
        );
    }

    #[test]
    fn finished_events_are_agent_state_done_and_untitled_ones_get_a_placeholder_body() {
        let hub = Hub::new(None);
        let c = conversation("", "s1");
        let event = hub.emit(FINISHED, &c, "term_a", 1.0);
        let frame = event.to_json("e");
        assert_eq!(frame["agentState"], "done");
        assert_eq!(frame["title"], "Agent finished");
        assert_eq!(frame["body"], "(untitled)");
        assert_eq!(frame["notificationId"], "e:1");
    }

    #[test]
    fn get_missed_since_reads_the_cursor_and_reports_the_epoch() {
        let hub = hub_with_three();
        let reply = missed_reply(&hub, &json!({ "lastSeenSeq": 1 })).unwrap();
        let got: Vec<u64> = reply["notifications"].as_array().unwrap().iter().map(|n| n["notificationSeq"].as_u64().unwrap()).collect();
        assert_eq!(got, vec![2, 3]);
        assert!(reply["epoch"].as_str().is_some_and(|e| !e.is_empty()));
        assert!(reply.get("dismissedPushes").is_none(), "no deliveredPushes, no dismissal reply");
    }

    #[test]
    fn get_missed_since_answers_delivered_pushes_with_an_empty_dismissal_list() {
        let hub = hub_with_three();
        let reply = missed_reply(&hub, &json!({ "lastSeenSeq": 3, "deliveredPushes": [] })).unwrap();
        assert_eq!(reply["dismissedPushes"], json!([]));
    }

    #[test]
    fn get_missed_since_rejects_a_bad_cursor() {
        let hub = Hub::new(None);
        assert_eq!(missed_reply(&hub, &json!({})).unwrap_err().0, "bad_request");
        assert_eq!(missed_reply(&hub, &json!({ "lastSeenSeq": -1 })).unwrap_err().0, "bad_request");
        assert_eq!(missed_reply(&hub, &json!({ "lastSeenSeq": 1.5 })).unwrap_err().0, "bad_request");
        assert_eq!(missed_reply(&hub, &json!({ "lastSeenSeq": 0, "epoch": 7 })).unwrap_err().0, "bad_request");
    }

    #[test]
    fn push_shapes_match_the_kind() {
        assert_eq!(push_shape(NEEDS_INPUT), ("Agent needs input", 4, &["raised_hand"][..]));
        assert_eq!(push_shape(FINISHED), ("Agent finished", 3, &["white_check_mark"][..]));
    }

    #[test]
    fn a_stopped_subscriber_is_dropped_and_a_live_one_receives_events() {
        let hub = Hub::new(None);
        let c = conversation("t", "s1");
        let (live_tx, live_rx) = mpsc::channel();
        hub.attach(live_tx, Arc::new(AtomicBool::new(false)));
        let (dead_tx, _dead_rx) = mpsc::channel();
        hub.attach(dead_tx, Arc::new(AtomicBool::new(true)));
        let event = hub.emit(FINISHED, &c, "term_a", 1.0);
        assert_eq!(live_rx.try_recv().unwrap(), event);
        assert_eq!(hub.inner.state.lock().subscribers.len(), 1, "the stopped subscriber was pruned");
    }

    #[test]
    fn attach_returns_the_epoch_and_does_not_replay_history() {
        let hub = hub_with_three();
        let (tx, rx) = mpsc::channel();
        let epoch = hub.attach(tx, Arc::new(AtomicBool::new(false)));
        assert_eq!(epoch, hub.missed_since(0, None).1);
        assert!(rx.try_recv().is_err(), "history is served by getMissedSince, not the stream");
    }

    #[test]
    fn a_test_push_reaches_a_live_stream_and_is_not_kept_in_the_history() {
        let hub = Hub::new(None);
        let (tx, rx) = mpsc::channel();
        let epoch = hub.attach(tx, Arc::new(AtomicBool::new(false)));
        assert_eq!(hub.test_push(1000.0), 1);
        let frame = rx.try_recv().unwrap().to_json(&epoch);
        assert_eq!(frame["type"], "notification");
        assert_eq!(frame["source"], "test");
        assert_eq!(frame["notificationId"], format!("test:{epoch}:1"));
        assert!(frame.get("notificationSeq").is_none(), "a test takes no place in the event sequence");
        assert!(hub.missed_since(0, None).0.is_empty());
    }

    #[test]
    fn a_test_push_with_no_open_stream_is_refused_and_a_stopped_stream_gets_nothing() {
        let hub = Hub::new(None);
        assert_eq!(hub.test_push(1000.0), 0);
        let (tx, rx) = mpsc::channel();
        hub.attach(tx, Arc::new(AtomicBool::new(true)));
        assert_eq!(hub.test_push(1000.0), 0);
        assert!(rx.try_recv().is_err());
        let ctx = crate::bridge::testkit::ctx_with(vec![], &std::env::temp_dir());
        assert_eq!(test_push(&json!({}), &ctx).unwrap(), json!({ "accepted": false, "reason": "unavailable" }));
    }

    #[test]
    fn the_test_push_request_is_accepted_once_a_stream_is_open() {
        let ctx = crate::bridge::testkit::ctx_with(vec![], &std::env::temp_dir());
        let (tx, _rx) = mpsc::channel();
        ctx.notify.attach(tx, Arc::new(AtomicBool::new(false)));
        assert_eq!(test_push(&json!({}), &ctx).unwrap(), json!({ "accepted": true }));
    }
}
