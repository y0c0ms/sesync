//! Plan usage for the phone's account screen: `accounts.list` and `accounts.subscribe`.
//!
//! One poller thread runs `omp usage --json` every `every` (the first run at once) and maps its
//! Claude (`anthropic`) and Codex (`openai-codex`) reports into the app's `AccountsSnapshot`
//! (`mobile/src/components/accounts-snapshot.ts`). The app's account rows carry email, and the 5h
//! ("session") and 7d ("weekly") bars with used percent and reset time.
//!
//! The omp schema this reads (derived from omp's embedded source, `commands/usage.ts` flags and
//! `cli/usage-cli.ts` `runUsageCommand`, `--json` branch of the default action):
//!   `{ generatedAt, reports: [UsageReport], accountsWithoutUsage: [credential], disabledCredentials, capacity }`
//!   * UsageReport: `{ provider, fetchedAt, limits: [UsageLimit], metadata: { accountId?, email?, ... } }`
//!   * UsageLimit: `{ id, label, scope: { provider, windowId, tier?, shared? }, window?: { durationMs?,
//!     resetsAt?, label? }, amount: { usedFraction?, used?, limit?, unit } }` (`ai/src/usage/claude.ts`
//!     `vX`, `ai/src/usage/codex` `sCn`). Claude windows are 5h = 18000000 ms and 7d = 604800000 ms.
//!   * credential without usage: `{ provider, type: "oauth", email?, accountId?, ... }`.
//!   * No credentials at all: nothing on stdout and exit status 1 (the message goes to stderr).
//!
//! The poller makes NO model call and no provider request of its own. The only provider traffic is
//! what `omp usage` itself does, once per refresh. Credentials are read by omp, never by this module.
//! Errors kept here are omp's exit status and one filtered stderr line (see [`safe_stderr_line`]).

use super::rpc::{Ctx, Failure};
use super::stream::{self, Conn, Sink};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The longest one `omp usage --json` run may take before its child is killed.
pub const USAGE_TIMEOUT: Duration = Duration::from_secs(60);
/// How often a running child is checked for exit.
const POLL: Duration = Duration::from_millis(20);
/// How long to wait for a child's output pipes after it exited.
const OUTPUT_WAIT: Duration = Duration::from_secs(5);
/// How often a subscription checks its stop flag while no snapshot changes.
const SUBSCRIBER_POLL: Duration = Duration::from_millis(250);
/// The longest stderr line shown to the phone.
const STDERR_CHARS: usize = 200;

const MINUTE_MS: u64 = 60_000;
const FIVE_HOURS_MS: u64 = 5 * 60 * MINUTE_MS;
const SEVEN_DAYS_MS: u64 = 7 * 24 * 60 * MINUTE_MS;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// The host-runtime target the app reads for Claude and Codex usage.
fn host_target() -> Value {
    json!({ "runtime": "host", "wslDistro": null })
}

#[derive(Default)]
struct Inner {
    /// The last snapshot that was read successfully. A failed refresh keeps it.
    snapshot: Option<Value>,
    /// The error text of the last refresh that failed; cleared by a success.
    error: Option<String>,
    /// One sender per open subscription. A send that fails means the subscription ended.
    subscribers: Vec<mpsc::Sender<Value>>,
}

/// The latest plan usage snapshot, shared by every request and subscription.
#[derive(Clone, Default)]
pub struct Store {
    inner: Arc<Mutex<Inner>>,
}

impl Store {
    /// The latest successful snapshot, in the app's `AccountsSnapshot` shape.
    pub fn current(&self) -> Option<Value> {
        self.inner.lock().snapshot.clone()
    }

    /// The error text of the last failed refresh, if the last refresh failed.
    pub fn last_error(&self) -> Option<String> {
        self.inner.lock().error.clone()
    }

    /// Store a good snapshot. Returns true when it differs from the stored one in anything but its
    /// timestamps; only then are subscribers sent it.
    fn record_ok(&self, snapshot: Value) -> bool {
        let mut inner = self.inner.lock();
        let changed = match &inner.snapshot {
            Some(previous) => comparable(previous) != comparable(&snapshot),
            None => true,
        };
        inner.error = None;
        if changed {
            inner.subscribers.retain(|tx| tx.send(snapshot.clone()).is_ok());
        }
        inner.snapshot = Some(snapshot);
        changed
    }

    /// Keep the last good snapshot and record why this refresh failed.
    fn record_err(&self, message: String) {
        self.inner.lock().error = Some(message);
    }

    /// A receiver that gets the current snapshot first (when there is one), then each later change.
    fn subscribe(&self) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel();
        let mut inner = self.inner.lock();
        if let Some(current) = &inner.snapshot {
            let _ = tx.send(current.clone());
        }
        inner.subscribers.push(tx);
        rx
    }
}

/// The snapshot without its `updatedAt` stamps: a poll that finds the same usage is not a change.
fn comparable(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != "updatedAt")
                .map(|(key, item)| (key.clone(), comparable(item)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(comparable).collect()),
        other => other.clone(),
    }
}

/// `accounts.list`: the latest snapshot. Before the first successful refresh there is none, and the
/// request fails with the last refresh's error, which the app shows.
pub fn accounts_list(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    check_list_params(params)?;
    ctx.usage.current().ok_or_else(|| {
        let message = match ctx.usage.last_error() {
            Some(error) => format!("No plan usage read yet: {error}"),
            None => "No plan usage read yet".to_string(),
        };
        ("unavailable", message)
    })
}

/// `ListAccountsParams` is `{ refreshUsage?: boolean }`. A non-boolean is a bad request.
fn check_list_params(params: &Value) -> Result<(), Failure> {
    match params.get("refreshUsage") {
        None | Some(Value::Null | Value::Bool(_)) => Ok(()),
        Some(_) => Err(("bad_request", "refreshUsage must be a boolean".to_string())),
    }
}

/// Subscription ids must be unique per process: the client releases a stream by the id it reads.
static SUBSCRIPTIONS: AtomicU64 = AtomicU64::new(0);

/// `accounts.subscribe`: a `ready` frame with the subscription id and the current snapshot (sent
/// once a snapshot exists), then one `snapshot` frame per change, until the stop flag or the end of
/// the connection.
pub fn accounts_subscribe(_params: &Value, ctx: &Ctx, conn: &Conn, id: &Value) -> Result<(), Failure> {
    let subscription_id = format!("accounts-{}", SUBSCRIPTIONS.fetch_add(1, Ordering::Relaxed) + 1);
    let stop = conn.register(subscription_id.clone(), id.clone());
    let rx = ctx.usage.subscribe();
    let sink = conn.sink.clone();
    let request = id.clone();
    thread::spawn(move || run_subscription(sink, request, subscription_id, rx, stop));
    Ok(())
}

fn run_subscription(
    sink: Sink,
    request: Value,
    subscription_id: String,
    rx: mpsc::Receiver<Value>,
    stop: Arc<AtomicBool>,
) {
    let mut ready = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv_timeout(SUBSCRIBER_POLL) {
            Ok(snapshot) => {
                let frame = if ready {
                    json!({ "type": "snapshot", "snapshot": snapshot })
                } else {
                    json!({ "type": "ready", "subscriptionId": subscription_id, "snapshot": snapshot })
                };
                ready = true;
                if !sink.json(&stream::streamed(&request, frame)) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    sink.json(&stream::end(&request));
}

/// Start the one background poller. It refreshes at once, then every `every`, and keeps the last good
/// snapshot when a refresh fails.
pub fn spawn_poller(store: Store, omp: PathBuf, every: Duration) {
    thread::spawn(move || loop {
        let _ = refresh_once(&store, &omp);
        thread::sleep(every);
    });
}

/// One refresh: run `omp usage --json`, map it, store it. Returns true when the stored usage changed.
/// On failure the last good snapshot stays and the error text is recorded.
fn refresh_once(store: &Store, omp: &Path) -> Result<bool, String> {
    match load_usage(omp).and_then(|usage| map_usage(&usage, now_ms())) {
        Ok(snapshot) => Ok(store.record_ok(snapshot)),
        Err(error) => {
            store.record_err(error.clone());
            Err(error)
        }
    }
}

/// The usage report for one refresh. `SESYNC_USAGE_FIXTURE`, when set, names a file holding an
/// `omp usage --json` report; the e2e harness uses it in place of running omp.
fn load_usage(omp: &Path) -> Result<Value, String> {
    match std::env::var_os("SESYNC_USAGE_FIXTURE") {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read SESYNC_USAGE_FIXTURE: {e}"))?;
            serde_json::from_str(&text).map_err(|e| format!("SESYNC_USAGE_FIXTURE is not JSON: {e}"))
        }
        None => run_usage(omp, USAGE_TIMEOUT),
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Whether `elapsed` has reached `timeout`.
fn timed_out(elapsed: Duration, timeout: Duration) -> bool {
    elapsed >= timeout
}

/// One line of omp's stderr that is fit for the phone: the first non-empty line, cut to
/// [`STDERR_CHARS`]. A line that names a credential (token, secret, password, bearer, authorization,
/// cookie, an API key, an `sk-`/`eyJ` prefix) or carries a long opaque run is dropped, not redacted.
fn safe_stderr_line(stderr: &str) -> Option<String> {
    let line = stderr.lines().map(str::trim).find(|line| !line.is_empty())?;
    let lower = line.to_ascii_lowercase();
    const SECRET_WORDS: [&str; 8] =
        ["token", "secret", "password", "bearer", "authorization", "cookie", "api key", "apikey"];
    let names_secret = SECRET_WORDS.iter().any(|word| lower.contains(word))
        || lower.contains("api_key")
        || lower.contains("sk-")
        || lower.contains("eyj");
    let opaque = line.split_whitespace().any(|word| {
        word.len() >= 40 && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_+/=.".contains(c))
    });
    if names_secret || opaque {
        return None;
    }
    Some(line.chars().take(STDERR_CHARS).collect())
}

/// Drain one child pipe on its own thread, so a full pipe never stalls the child.
fn drain<R: Read + Send + 'static>(mut pipe: R) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

/// Run `omp usage --json` and parse its stdout. A child still running at `timeout` is killed by PID.
/// A grandchild that inherited the pipes keeps only its own reader thread alive.
fn run_usage(omp: &Path, timeout: Duration) -> Result<Value, String> {
    let mut child = Command::new(omp)
        .args(["usage", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run omp usage --json: {e}"))?;
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("cannot read omp usage --json output".to_string());
    };
    let stdout_rx = drain(out);
    let stderr_rx = drain(err);
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if timed_out(started.elapsed(), timeout) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("omp usage --json timed out after {timeout:?}"));
            }
            Ok(None) => thread::sleep(POLL),
            Err(e) => return Err(format!("cannot wait for omp usage --json: {e}")),
        }
    };
    let stdout = stdout_rx.recv_timeout(OUTPUT_WAIT).unwrap_or_default();
    let stderr = stderr_rx.recv_timeout(OUTPUT_WAIT).unwrap_or_default();
    if !status.success() {
        let text = String::from_utf8_lossy(&stderr);
        return Err(match safe_stderr_line(&text) {
            Some(line) => format!("omp usage --json failed ({status}): {line}"),
            None => format!("omp usage --json failed ({status})"),
        });
    }
    serde_json::from_slice(&stdout).map_err(|e| format!("omp usage --json printed no JSON: {e}"))
}

/// Map omp's `usage --json` output to the app's `AccountsSnapshot`.
///
/// Only Claude (`anthropic`) and Codex (`openai-codex`) reports are mapped; other providers have no
/// section in the app's snapshot and are dropped. Each credential becomes one account row. omp does
/// not say which account is active, so `activeAccountId` and the `claude`/`codex` `rateLimits` are null
/// and every account's usage is an inactive entry, which the app renders as a bar row.
fn map_usage(usage: &Value, now: u64) -> Result<Value, String> {
    let reports = usage
        .get("reports")
        .and_then(Value::as_array)
        .ok_or_else(|| "omp usage --json has no reports list".to_string())?;
    let orphans = usage.get("accountsWithoutUsage").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let claude = lane(reports, orphans, "anthropic", "claude", now);
    let codex = lane(reports, orphans, "openai-codex", "codex", now);
    Ok(json!({
        "claude": { "accounts": claude.accounts, "activeAccountId": null },
        "codex": { "accounts": codex.accounts, "activeAccountId": null },
        "rateLimits": {
            "claude": null,
            "codex": null,
            "claudeTarget": host_target(),
            "codexTarget": host_target(),
            "inactiveClaudeAccounts": claude.inactive,
            "inactiveCodexAccounts": codex.inactive,
        },
    }))
}

#[derive(Default)]
struct Lane {
    accounts: Vec<Value>,
    inactive: Vec<Value>,
}

impl Lane {
    /// Add one account unless its id is already listed: the first report for an account wins.
    fn add(&mut self, id: String, label: String, updated: u64, limits: Value) {
        if self.accounts.iter().any(|account| account["id"] == id.as_str()) {
            return;
        }
        self.inactive.push(json!({
            "accountId": id.clone(),
            "rateLimits": limits,
            "updatedAt": updated,
            "isFetching": false,
        }));
        self.accounts.push(json!({ "id": id, "email": label, "updatedAt": updated }));
    }
}

/// The accounts of one omp provider: its reports, then its OAuth credentials that have no report.
fn lane(reports: &[Value], orphans: &[Value], omp_provider: &str, provider: &str, now: u64) -> Lane {
    let mut lane = Lane::default();
    for (index, report) in reports.iter().enumerate() {
        if report.get("provider").and_then(Value::as_str) != Some(omp_provider) {
            continue;
        }
        let metadata = report.get("metadata");
        let (id, label) = identity(
            text(metadata, "accountId"),
            text(metadata, "email"),
            format!("{omp_provider}#{index}"),
        );
        let updated = non_negative_int(report.get("fetchedAt")).unwrap_or(now);
        let limits = provider_limits(report, provider, updated);
        lane.add(id, label, updated, limits);
    }
    for (index, entry) in orphans.iter().enumerate() {
        if entry.get("provider").and_then(Value::as_str) != Some(omp_provider)
            || entry.get("type").and_then(Value::as_str) != Some("oauth")
        {
            continue;
        }
        let (id, label) = identity(
            text(Some(entry), "accountId"),
            text(Some(entry), "email"),
            format!("{omp_provider}#orphan{index}"),
        );
        lane.add(id, label, now, Value::Null);
    }
    lane
}

/// The app's `ProviderRateLimits` for one report: 5h as `session`, untiered 7d as `weekly`, 7d with
/// tier `fable` as `fableWeekly`, and every other window with a duration as a named bucket.
/// A window without a duration or a used fraction cannot be drawn, so it is dropped.
fn provider_limits(report: &Value, provider: &str, updated: u64) -> Value {
    let mut session = None;
    let mut weekly = None;
    let mut fable_weekly = None;
    let mut buckets = Vec::new();
    for limit in report.get("limits").and_then(Value::as_array).into_iter().flatten() {
        let Some(used) = used_percent(limit) else { continue };
        let duration = non_negative_int(limit.pointer("/window/durationMs"));
        let Some(minutes) = duration.map(|ms| ms / MINUTE_MS).filter(|m| *m > 0) else { continue };
        let resets_at = non_negative_int(limit.pointer("/window/resetsAt"));
        let tier = limit.pointer("/scope/tier").and_then(Value::as_str).map(str::to_ascii_lowercase);
        match (tier.as_deref(), duration) {
            (None, Some(FIVE_HOURS_MS)) => first_of(&mut session, window(used, minutes, resets_at)),
            (None, Some(SEVEN_DAYS_MS)) => first_of(&mut weekly, window(used, minutes, resets_at)),
            (Some("fable"), Some(SEVEN_DAYS_MS)) => {
                first_of(&mut fable_weekly, window(used, minutes, resets_at))
            }
            _ => {
                if let Some(name) = text(Some(limit), "label").or_else(|| text(Some(limit), "id")) {
                    let mut bucket = window(used, minutes, resets_at);
                    bucket["name"] = json!(name);
                    buckets.push(bucket);
                }
            }
        }
    }
    let status = if session.is_some() || weekly.is_some() || fable_weekly.is_some() || !buckets.is_empty() {
        "ok"
    } else {
        "unavailable"
    };
    json!({
        "provider": provider,
        "session": session,
        "weekly": weekly,
        "fableWeekly": fable_weekly,
        "buckets": buckets,
        "updatedAt": updated,
        "error": null,
        "status": status,
    })
}

/// One `RateLimitWindow`: used percent (0 to 100), window length in whole minutes, reset time in ms.
fn window(used: f64, minutes: u64, resets_at: Option<u64>) -> Value {
    json!({
        "usedPercent": used,
        "windowMinutes": minutes,
        "resetsAt": resets_at,
        "resetDescription": null,
    })
}

fn first_of(slot: &mut Option<Value>, value: Value) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

/// A limit's used percent from its amount: `usedFraction`, else `used / limit`, clamped to 0 to 100.
fn used_percent(limit: &Value) -> Option<f64> {
    let amount = limit.get("amount")?;
    let fraction = amount.get("usedFraction").and_then(Value::as_f64).or_else(|| {
        let used = amount.get("used")?.as_f64()?;
        let cap = amount.get("limit")?.as_f64()?;
        (cap > 0.0).then(|| used / cap)
    })?;
    fraction.is_finite().then(|| (fraction * 100.0).clamp(0.0, 100.0))
}

/// A non-negative whole number (a timestamp or a duration in ms), or None.
fn non_negative_int(value: Option<&Value>) -> Option<u64> {
    let n = value?.as_f64()?;
    (n.is_finite() && (0.0..=MAX_SAFE_INTEGER).contains(&n)).then(|| n.round() as u64)
}

/// A non-empty string field of `value`.
fn text<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a str> {
    value?.get(key)?.as_str().filter(|s| !s.is_empty())
}

/// An account's id (account id, else email, else a positional fallback) and its label (email, else id).
fn identity(account_id: Option<&str>, email: Option<&str>, fallback: String) -> (String, String) {
    let id = account_id.or(email).map_or(fallback, |s| s.to_string());
    let label = email.map_or_else(|| id.clone(), |s| s.to_string());
    (id, label)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two Claude accounts (one with a report, one only as a credential), one Codex account with a
    /// window that has no duration, one Gemini report the bridge does not map.
    const FIXTURE: &str = r#"{
      "generatedAt": 1790000000000,
      "reports": [
        {
          "provider": "anthropic",
          "fetchedAt": 1790000000000,
          "limits": [
            { "id": "anthropic:5h", "label": "Claude 5 Hour",
              "scope": { "provider": "anthropic", "windowId": "5h", "shared": true },
              "window": { "id": "5h", "label": "5 Hour", "durationMs": 18000000, "resetsAt": 1790003600000 },
              "amount": { "used": 25, "limit": 100, "remaining": 75, "usedFraction": 0.25, "remainingFraction": 0.75, "unit": "percent" },
              "status": "ok" },
            { "id": "anthropic:7d", "label": "Claude 7 Day",
              "scope": { "provider": "anthropic", "windowId": "7d", "shared": true },
              "window": { "id": "7d", "label": "7 Day", "durationMs": 604800000, "resetsAt": 1790500000000 },
              "amount": { "usedFraction": 0.5, "unit": "percent" },
              "status": "ok" },
            { "id": "anthropic:7d:fable", "label": "Claude 7 Day (Fable)",
              "scope": { "provider": "anthropic", "windowId": "7d", "tier": "fable" },
              "window": { "id": "7d", "label": "7 Day", "durationMs": 604800000 },
              "amount": { "usedFraction": 0.75, "unit": "percent" } },
            { "id": "anthropic:7d:opus", "label": "Claude 7 Day (Opus)",
              "scope": { "provider": "anthropic", "windowId": "7d", "tier": "opus" },
              "window": { "id": "7d", "label": "7 Day", "durationMs": 604800000 },
              "amount": { "usedFraction": 0.5, "unit": "percent" } }
          ],
          "metadata": { "source": "ratelimit-headers", "accountId": "acct-claude-1", "email": "alice@example.com" }
        },
        {
          "provider": "openai-codex",
          "fetchedAt": 1790000000000,
          "limits": [
            { "id": "openai-codex:primary", "label": "5 hours",
              "scope": { "provider": "openai-codex", "windowId": "5h" },
              "window": { "id": "5h", "label": "5 hours", "durationMs": 18000000, "resetsAt": 1790003600000 },
              "amount": { "usedFraction": 0.25, "unit": "percent" } },
            { "id": "openai-codex:secondary", "label": "7 days",
              "scope": { "provider": "openai-codex", "windowId": "7d" },
              "window": { "id": "7d", "label": "7 days", "durationMs": 604800000 },
              "amount": { "usedFraction": 0.5, "unit": "percent" } },
            { "id": "openai-codex:primary", "label": "Primary window",
              "scope": { "provider": "openai-codex", "windowId": "primary" },
              "window": { "id": "primary", "label": "Primary window" },
              "amount": { "usedFraction": 0.9, "unit": "percent" } }
          ],
          "metadata": { "planType": "plus", "email": "bob@example.com", "accountId": "acct-codex-1" }
        },
        {
          "provider": "gemini",
          "fetchedAt": 1790000000000,
          "limits": [
            { "id": "gemini:x", "label": "x", "scope": { "provider": "gemini" },
              "window": { "durationMs": 18000000 }, "amount": { "usedFraction": 0.1, "unit": "percent" } }
          ],
          "metadata": { "email": "dan@example.com" }
        }
      ],
      "accountsWithoutUsage": [
        { "provider": "anthropic", "type": "oauth", "email": "carol@example.com", "accountId": "acct-claude-2", "authorizedAt": 1789000000000 },
        { "provider": "openai-codex", "type": "api_key" }
      ],
      "disabledCredentials": [],
      "capacity": {}
    }"#;

    const NOW: u64 = 1_790_000_000_000;

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).unwrap()
    }

    fn mapped() -> Value {
        map_usage(&fixture(), NOW).unwrap()
    }

    /// A scratch directory under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("sesync-usage-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An executable `#!/bin/sh` script with `body`, at `dir/name`.
    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn maps_fixture_to_app_snapshot() {
        let snap = mapped();
        let claude_accounts = snap["claude"]["accounts"].as_array().unwrap();
        assert_eq!(claude_accounts.len(), 2);
        assert_eq!(claude_accounts[0]["id"], "acct-claude-1");
        assert_eq!(claude_accounts[0]["email"], "alice@example.com");
        assert_eq!(claude_accounts[1]["id"], "acct-claude-2");
        assert_eq!(claude_accounts[1]["email"], "carol@example.com");
        assert!(snap["claude"]["activeAccountId"].is_null());
        assert!(snap["rateLimits"]["claude"].is_null());
        assert_eq!(snap["rateLimits"]["claudeTarget"], json!({ "runtime": "host", "wslDistro": null }));

        let claude = &snap["rateLimits"]["inactiveClaudeAccounts"];
        assert_eq!(claude[0]["accountId"], "acct-claude-1");
        assert_eq!(claude[0]["isFetching"], false);
        assert_eq!(claude[0]["updatedAt"], NOW);
        let limits = &claude[0]["rateLimits"];
        assert_eq!(limits["provider"], "claude");
        assert_eq!(limits["status"], "ok");
        assert_eq!(limits["error"], Value::Null);
        assert_eq!(
            limits["session"],
            json!({ "usedPercent": 25.0, "windowMinutes": 300, "resetsAt": 1790003600000u64, "resetDescription": null })
        );
        assert_eq!(
            limits["weekly"],
            json!({ "usedPercent": 50.0, "windowMinutes": 10080, "resetsAt": 1790500000000u64, "resetDescription": null })
        );
        assert_eq!(limits["fableWeekly"]["usedPercent"], 75.0);
        assert!(limits["fableWeekly"]["resetsAt"].is_null());
        assert_eq!(limits["buckets"].as_array().unwrap().len(), 1);
        assert_eq!(limits["buckets"][0]["name"], "Claude 7 Day (Opus)");
        assert_eq!(limits["buckets"][0]["usedPercent"], 50.0);

        // A credential without a report is a row with unavailable bars, not a fetch.
        assert_eq!(claude[1]["accountId"], "acct-claude-2");
        assert!(claude[1]["rateLimits"].is_null());
    }

    #[test]
    fn maps_codex_and_drops_unmapped_providers() {
        let snap = mapped();
        let codex = snap["codex"]["accounts"].as_array().unwrap();
        assert_eq!(codex.len(), 1, "the api_key credential and the gemini report are not accounts");
        assert_eq!(codex[0]["id"], "acct-codex-1");
        assert_eq!(codex[0]["email"], "bob@example.com");
        assert!(codex[0]["updatedAt"].is_u64());

        let limits = &snap["rateLimits"]["inactiveCodexAccounts"][0]["rateLimits"];
        assert_eq!(limits["provider"], "codex");
        assert_eq!(limits["session"]["usedPercent"], 25.0);
        assert_eq!(limits["session"]["windowMinutes"], 300);
        assert_eq!(limits["weekly"]["usedPercent"], 50.0);
        // A window with no duration cannot be drawn; it is dropped rather than shown wrong.
        assert_eq!(limits["buckets"], json!([]));

        let text = snap.to_string();
        assert!(!text.contains("dan@example.com"), "gemini is not part of the app snapshot");
    }

    #[test]
    fn changed_ignores_timestamps_only() {
        let first = mapped();
        let mut later = first.clone();
        later["claude"]["accounts"][0]["updatedAt"] = json!(NOW + 60_000);
        later["rateLimits"]["inactiveClaudeAccounts"][0]["updatedAt"] = json!(NOW + 60_000);
        later["rateLimits"]["inactiveClaudeAccounts"][0]["rateLimits"]["updatedAt"] = json!(NOW + 60_000);
        assert_eq!(comparable(&first), comparable(&later));

        let mut moved = first.clone();
        moved["rateLimits"]["inactiveClaudeAccounts"][0]["rateLimits"]["session"]["usedPercent"] = json!(50.0);
        assert_ne!(comparable(&first), comparable(&moved));

        let store = Store::default();
        assert!(store.record_ok(first.clone()), "the first snapshot is a change");
        assert!(!store.record_ok(later), "a poll that only moved timestamps is not a change");
        assert!(store.record_ok(moved), "a moved percent is a change");
    }

    #[test]
    fn subscribers_get_current_then_changes_only() {
        let store = Store::default();
        let first = mapped();
        store.record_ok(first.clone());
        let rx = store.subscribe();
        assert_eq!(rx.try_recv().unwrap(), first, "a new subscriber gets the current snapshot first");
        assert!(rx.try_recv().is_err());

        assert!(!store.record_ok(first.clone()));
        assert!(rx.try_recv().is_err(), "an unchanged poll sends nothing");

        let mut moved = first.clone();
        moved["rateLimits"]["inactiveClaudeAccounts"][0]["rateLimits"]["session"]["usedPercent"] = json!(50.0);
        assert!(store.record_ok(moved.clone()));
        assert_eq!(rx.try_recv().unwrap(), moved);
    }

    #[test]
    fn timed_out_is_inclusive_at_the_limit() {
        assert!(timed_out(Duration::from_millis(5), Duration::from_millis(5)));
        assert!(timed_out(Duration::from_secs(61), USAGE_TIMEOUT));
        assert!(!timed_out(Duration::from_millis(4), Duration::from_millis(5)));
    }

    #[test]
    fn stderr_line_is_cut_and_secret_lines_are_dropped() {
        assert_eq!(
            safe_stderr_line("\n  No credentials found. Run `omp` and use /login\nsecond").as_deref(),
            Some("No credentials found. Run `omp` and use /login")
        );
        assert_eq!(safe_stderr_line("refresh failed: bad token sk-abc123"), None);
        assert_eq!(safe_stderr_line("Authorization: Bearer x"), None);
        assert_eq!(safe_stderr_line("ok aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), None);
        assert_eq!(safe_stderr_line("   \n  "), None);
        let long = "word ".repeat(100);
        assert_eq!(safe_stderr_line(&long).unwrap().chars().count(), STDERR_CHARS);
    }

    #[cfg(unix)]
    #[test]
    fn fake_omp_end_to_end_and_unchanged_poll() {
        let scratch = Scratch::new("e2e");
        let fixture_path = scratch.0.join("usage.json");
        std::fs::write(&fixture_path, FIXTURE).unwrap();
        let omp = write_script(&scratch.0, "omp", &format!("cat '{}'", fixture_path.display()));

        let store = Store::default();
        assert_eq!(refresh_once(&store, &omp), Ok(true));
        assert_eq!(refresh_once(&store, &omp), Ok(false), "the same usage is not a change");
        assert!(store.last_error().is_none());
        let snap = store.current().unwrap();
        assert_eq!(snap["claude"]["accounts"][0]["id"], "acct-claude-1");
        assert_eq!(snap["rateLimits"]["inactiveCodexAccounts"][0]["rateLimits"]["weekly"]["windowMinutes"], 10080);
    }

    #[cfg(unix)]
    #[test]
    fn failing_omp_keeps_last_good_snapshot() {
        let scratch = Scratch::new("fail");
        let fixture_path = scratch.0.join("usage.json");
        std::fs::write(&fixture_path, FIXTURE).unwrap();
        let good = write_script(&scratch.0, "good", &format!("cat '{}'", fixture_path.display()));
        let bad = write_script(
            &scratch.0,
            "bad",
            "echo 'No credentials found. Run omp and use /login' >&2\nexit 1",
        );

        let store = Store::default();
        assert_eq!(refresh_once(&store, &good), Ok(true));
        let before = store.current().unwrap();

        let error = refresh_once(&store, &bad).unwrap_err();
        assert!(error.contains("No credentials found"), "{error}");
        assert_eq!(store.current().unwrap(), before, "a failed refresh keeps the last good snapshot");
        assert_eq!(store.last_error().as_deref(), Some(error.as_str()));

        assert_eq!(refresh_once(&store, &good), Ok(false));
        assert!(store.last_error().is_none(), "a good refresh clears the error");
    }

    #[cfg(unix)]
    #[test]
    fn failing_omp_without_a_snapshot_has_no_current_value() {
        let scratch = Scratch::new("nothing");
        let bad = write_script(&scratch.0, "bad", "exit 1");
        let store = Store::default();
        assert!(refresh_once(&store, &bad).is_err());
        assert!(store.current().is_none());
        assert!(store.last_error().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_the_child() {
        let scratch = Scratch::new("slow");
        // `exec` makes the script's process the sleep itself, so the kill reaches the process that runs.
        let slow = write_script(&scratch.0, "slow", "exec sleep 30");
        let started = Instant::now();
        let error = run_usage(&slow, Duration::from_millis(200)).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10), "the child was not killed");
    }
}
