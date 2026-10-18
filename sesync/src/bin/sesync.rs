//! sesync: Rust replacement for `python daemon/sesync.py`.
//! Modes: `--status` prints the mirror view; `--once` runs one sync pass.
//! Continuous polling is the same pass repeated on an interval (not yet wired).

use sesync::mirror::file_hash;
use sesync::pass::{format_status, PassSource};
use sesync::scan::{should_sync, ScanInput, MIN_BYTES};
use sesync::state::{Mirror, Source, State};
use sesync::{discovery, titles};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct Env {
    state_dir: PathBuf,
    omp: PathBuf,
    pi: PathBuf,
    claude: PathBuf,
    gemini: PathBuf,
    opencode_db: PathBuf,
    opencode_bin: PathBuf,
    sesync_conv: PathBuf,
    sesync_lock: PathBuf,
    quiet_secs: f64,
    max_age_secs: f64,
}

impl Env {
    fn from_env() -> Env {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let get = |k: &str, d: PathBuf| std::env::var_os(k).map(PathBuf::from).unwrap_or(d);
        let num = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        // install.sh puts sesync-conv and sesync-lock next to this binary (~/.local/bin by default).
        // Look there first, so the defaults match what is installed rather than a ~/bin that may not exist.
        let beside_me = |name: &str| -> PathBuf {
            std::env::current_exe().ok()
                .and_then(|e| e.parent().map(|d| d.join(name)))
                .filter(|p| p.exists())
                .unwrap_or_else(|| home.join(".local/bin").join(name))
        };
        Env {
            state_dir: get("SESYNC_DIR", home.join(".local/share/sesync")),
            omp: get("OMP_AGENT_DIR", home.join(".omp/agent")),
            pi: get("PI_AGENT_DIR", home.join(".pi/agent")),
            claude: get("CLAUDE_CONFIG_DIR", home.join(".claude")),
            gemini: get("GEMINI_CLI_HOME", home.join(".gemini")),
            opencode_db: get("OPENCODE_DB", home.join(".local/share/opencode/opencode.db")),
            opencode_bin: get("OPENCODE_BIN", PathBuf::from("opencode")),
            sesync_conv: get("SESYNC_CONV", beside_me("sesync-conv")),
            sesync_lock: get("SESYNC_LOCK", beside_me("sesync-lock")),
            quiet_secs: num("SESYNC_QUIET", 20.0),
            max_age_secs: num("SESYNC_MAX_AGE_DAYS", 14.0) * 86400.0,
        }
    }
    fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }
    fn lock_dir(&self) -> PathBuf {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        sesync::lock::lock_dir_for(&home, std::env::var_os("SESYNC_LOCK_DIR").map(PathBuf::from))
    }
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// The one place team material lives: joined credentials, the device key, and the admin key.
/// The daemon, the TUI and every `sesync team` command use this, so a credential written by
/// `team join` is visible to all of them.
fn team_config_dir() -> PathBuf {
    std::env::var_os("SESYNC_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".config").join("sesync")
    })
}

/// A duration from the environment, in seconds. A value that is not a finite, non-negative number
/// is ignored and the default is used, so a typo never panics the daemon.
fn duration_from_env(key: &str, default_secs: f64) -> std::time::Duration {
    std::env::var(key).ok()
        .and_then(|v| v.parse::<f64>().ok())
        .and_then(|s| std::time::Duration::try_from_secs_f64(s).ok())
        .unwrap_or_else(|| std::time::Duration::from_secs_f64(default_secs))
}

fn mtime_secs(p: &Path) -> f64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Mirrors the daemon created, so a change to them is never treated as a source.
fn is_mirror(state: &State, key: &str) -> bool {
    state.mirror_keys.contains(key)
}

/// Find sources that need a sync, and the title to give their mirror.
fn scan(env: &Env, state: &mut State) -> Vec<PassSource> {
    let now = now_secs();
    let mut out = Vec::new();
    // Claude sessions are sources too (the Python daemon synced them; the Rust port had
    // dropped this path). Each format lists its own main sessions.
    let claude_root = env.claude.clone();
    let mut sources: Vec<(&str, Vec<PathBuf>)> = vec![
        ("omp", if env.omp.is_dir() { discovery::jsonl_sessions(&env.omp) } else { Vec::new() }),
        ("pi", if env.pi.is_dir() { discovery::jsonl_sessions(&env.pi) } else { Vec::new() }),
        ("claude", discovery::claude_sessions(&claude_root)),
    ];
    for (fmt, paths) in sources.drain(..) {
        if paths.is_empty() {
            continue;
        }
        for path in paths {
            let key = path.display().to_string();
            if is_mirror(state, &key) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let mtime = mtime_secs(&path);
            let src = state.sources.entry(key.clone()).or_insert_with(|| Source::new(fmt, &key));
            if mtime > src.last_seen_change {
                src.last_seen_change = mtime;
            }
            let stamp = format!("{}:{}", meta.len(), mtime);
            let hash = if stamp == src.last_stamp && !src.last_hash.is_empty() {
                src.last_hash.clone()
            } else {
                let h = file_hash(&path).unwrap_or_default();
                src.last_stamp = stamp;
                h
            };
            let hash_changed = hash != src.last_hash;
            let live: Vec<&Mirror> = src.mirrors.values().filter(|m| !m.diverged).collect();
            let live_current = live.iter().all(|m| m.source_hash == hash);
            // Mirrors are wanted only for targets the user chose for this session. A source
            // with no choice is "changed" only when its content changed, and writes nothing.
            let wanted = src.chosen_targets.len();
            let input = ScanInput {
                size: meta.len(),
                mtime,
                now,
                is_mirror: false,
                max_age_secs: env.max_age_secs,
                quiet_secs: env.quiet_secs,
                hash_changed,
                live_mirrors_current: live_current,
                mirror_count: src.mirrors.len(),
                wanted_mirrors: wanted,
            };
            if !should_sync(&input) || meta.len() < MIN_BYTES {
                continue;
            }
            let title = std::fs::read_to_string(&path)
                .map(|text| {
                    let lines: Vec<&str> = text.lines().collect();
                    if fmt == "claude" {
                        titles::claude_title_from_lines(&lines).0
                    } else {
                        titles::omp_title_from_lines(&lines).0
                    }
                })
                .unwrap_or_default();
            src.last_hash = hash;
            out.push(PassSource { key, title: if title.is_empty() { "untitled".into() } else { title } });
        }
    }
    out.extend(scan_opencode(env, state, now));
    record_subagents(env, state);
    out
}

/// Record every subagent transcript under its parent session, so the TUI can show it there.
/// Claude and Gemini keep subagent files next to their parent; OpenCode child sessions are
/// rows with a parent_id and are handled by the source filter, not here.
fn record_subagents(env: &Env, state: &mut State) {
    let mut found: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (parent, path) in discovery::claude_subagents(&env.claude)
        .into_iter()
        .chain(discovery::gemini_subagents(&env.gemini))
    {
        found.entry(parent).or_default().push(path.display().to_string());
    }
    let store = env.state_dir.join("archive");
    for (parent, files) in found.iter_mut() {
        files.sort();
        // Keep a verified, compressed copy of each subagent transcript under its parent.
        // A copy that fails to write is logged; it never blocks the pass.
        for f in files.iter() {
            if let Err(e) = sesync::archive_store::store_session(&store, parent, Path::new(f)) {
                eprintln!("archive {f}: {e}");
            }
        }
    }
    state.subagents = found;
}

/// O-penCode sessions that changed. Same rules as the Python: live sessions only,
/// no sessions older than the window, wait for quiet, and re-sync only when
/// time_updated moved. The title is the store's title, or the first prompt when the
/// store has only the default name.
fn scan_opencode(env: &Env, state: &mut State, now: f64) -> Vec<PassSource> {
    let mut out = Vec::new();
    if !env.opencode_db.exists() {
        return out;
    }
    for s in sesync::opencode::sessions(&env.opencode_db) {
        if state.mirror_keys.contains(&s.id) || now - s.updated > env.max_age_secs {
            continue;
        }
        let src = state.sources.entry(s.id.clone()).or_insert_with(|| Source::new("opencode", &s.id));
        if s.updated > src.last_seen_change {
            src.last_seen_change = s.updated;
        }
        if now - src.last_seen_change < env.quiet_secs {
            continue;
        }
        let stamp = format!("{:.3}", s.updated);
        if stamp == src.last_hash {
            continue;
        }
        src.last_hash = stamp;
        let title = sesync::opencode::clean_title(&s.title);
        let title = if title.is_empty() {
            let p = sesync::opencode::first_prompt(&env.opencode_db, &s.id);
            if p.is_empty() { "untitled session".to_string() } else { p }
        } else {
            title
        };
        // Keep the name on the source, so the TUI can show it without re-reading the database.
        if let Some(src) = state.sources.get_mut(&s.id) {
            src.title = title.clone();
        }
        out.push(PassSource { key: s.id.clone(), title });
    }
    out
}

/// One pass: scan the stores and record what changed in sesync's own state. Nothing is written
/// into any harness store, so a session stays exactly where its harness keeps it.
fn one_pass(env: &Env, state: &mut State) -> usize {
    let changed = scan(env, state);
    let attempted = changed.len();
    for s in &changed {
        sync_chosen(env, state, s);
    }
    if let Err(e) = state.save(&env.state_file()) {
        eprintln!("sesync: cannot save state: {e}");
    }
    attempted
}

/// Write mirrors for one changed source, only into the targets the user chose for it. A source
/// with no chosen target writes nothing. A failure is counted on the source and the pass goes on.
fn sync_chosen(env: &Env, state: &mut State, s: &PassSource) {
    let Some(src) = state.sources.get(&s.key) else { return };
    let fmt = src.fmt.clone();
    let chosen: Vec<String> = src.chosen_targets.iter().cloned().collect();
    if fmt == "opencode" {
        // O-penCode targets are not file writes; the bundle-import path is wired separately.
        return;
    }
    let stores = sesync::mirror_write::Stores {
        omp: &env.omp,
        pi: &env.pi,
        claude: &env.claude,
        sesync_conv: &env.sesync_conv,
        sesync_lock: &env.sesync_lock,
    };
    let conv = sesync::discovery::parse_omp_name(
        Path::new(&s.key).file_name().and_then(|n| n.to_str()).unwrap_or(""),
    ).unwrap_or_else(|| s.key.clone());
    for target in chosen {
        if target == fmt {
            continue;
        }
        if let Err(e) = sesync::mirror_write::write_mirror(&stores, state, &s.key, &fmt, &conv, &s.title, &target) {
            if let Some(src) = state.sources.get_mut(&s.key) {
                src.failures += 1;
            }
            eprintln!("mirror {} -> {target} failed: {e}", s.key);
        }
    }
}

/// Take the daemon-wide instance lock in the state directory. Held for the life of the
/// returned file; the OS drops it when the process exits.
fn acquire_instance(env: &Env) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(&env.state_dir).map_err(|e| e.to_string())?;
    let path = env.state_dir.join("daemon.lock");
    let file = std::fs::OpenOptions::new()
        .create(true).read(true).write(true).truncate(false)
        .open(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) =>
            Err("already running: another sesync --watch holds this state directory".into()),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

/// Clear locks whose process is gone, using sesync_lock's own sweep. Missing binary is a no-op.
fn sweep_stale_locks(env: &Env) {
    if env.sesync_lock.exists() {
        // Pinned to the same directory as the TUI and the claims, never left to the client's default.
        let _ = std::process::Command::new(&env.sesync_lock)
            .arg("sweep")
            .env("SESYNC_LOCK_DIR", env.lock_dir())
            .output();
    }
}

/// `sesync mirrors`: list the mirror copies sesync wrote, and whether each can go. Dry run unless
/// `--apply` is given. Stop the daemon first, so the two never write state at the same time.
fn run_mirrors(env: &Env, state: &mut State, apply: bool) -> i32 {
    let _instance = match acquire_instance(env) {
        Ok(f) => f,
        Err(m) => {
            eprintln!("sesync: {m}; stop the daemon before cleaning up mirrors");
            return 3;
        }
    };
    let planned = sesync::mirror_cleanup::plan(state);
    if planned.is_empty() {
        println!("no mirror copies recorded");
        return 0;
    }
    use sesync::mirror_cleanup::Verdict;
    for p in &planned {
        let (verb, why) = match &p.verdict {
            Verdict::Remove => ("remove", String::new()),
            Verdict::Gone => ("clear", " (already gone)".to_string()),
            Verdict::Keep(w) => ("keep", format!(": {w}")),
        };
        println!("{verb:<7} {:<8} {}{why}", p.target, p.path);
    }
    let removable = planned.iter().filter(|p| p.verdict == Verdict::Remove).count();
    if !apply {
        println!("\n{removable} can be removed. Dry run: nothing changed. Run `sesync mirrors --apply` to remove them.");
        return 0;
    }
    for line in sesync::mirror_cleanup::apply(state, &planned, &env.lock_dir(), &env.sesync_lock) {
        println!("{line}");
    }
    if let Err(e) = state.save(&env.state_file()) {
        eprintln!("sesync: cannot save state: {e}");
        return 1;
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Team admin commands do not touch the session state, so they run before it is loaded.
    if args.first().map(String::as_str) == Some("team") {
        std::process::exit(run_team(&args[1..]));
    }
    let env = Env::from_env();
    let _ = std::fs::create_dir_all(&env.state_dir);
    let mut state = match State::load(&env.state_file()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sesync: cannot read state {}: {e}", env.state_file().display());
            std::process::exit(1);
        }
    };
    if args.first().map(String::as_str) == Some("mirrors") {
        std::process::exit(run_mirrors(&env, &mut state, args.iter().any(|a| a == "--apply")));
    }
    if args.iter().any(|a| a == "--tui") {
        let roots = sesync::manage::Roots {
            omp: env.omp.join("sessions"),
            pi: env.pi.join("sessions"),
            claude: env.claude.join("projects"),
        };
        if let Err(e) = run_tui(roots, state, env.lock_dir(), env.sesync_lock.clone(), team_config_dir()) {
            eprintln!("sesync: tui failed: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.iter().any(|a| a == "--status") {
        print!("{}", format_status(&state));
        return;
    }
    if args.iter().any(|a| a == "--once") {
        let attempted = one_pass(&env, &mut state);
        println!("one pass done, {attempted} source(s) changed");
        return;
    }
    if args.iter().any(|a| a == "--watch") {
        // One daemon per state directory. The OS releases this lock when the process
        // exits, even on a crash, so a stale file never blocks a new start.
        let _instance = match acquire_instance(&env) {
            Ok(f) => f,
            Err(msg) => {
                eprintln!("sesync: {msg}");
                std::process::exit(3);
            }
        };
        // Clear locks left by dead processes before the first pass, as the Python does.
        sweep_stale_locks(&env);
        let limit: Option<u64> = std::env::var("SESYNC_PASSES").ok().and_then(|v| v.parse().ok());
        let poll = std::time::Duration::from_secs_f64(
            std::env::var("SESYNC_POLL").ok().and_then(|v| v.parse().ok()).unwrap_or(5.0));
        let mut passes = 0u64;
        // The team side is optional: configured only by the team environment (see team_watch).
        let team = sesync::team_watch::TeamWatch::from_env(&team_config_dir());
        // The team pull and push run on their own interval: SESYNC_POLL at first, doubling while the
        // team has nothing new, capped at SESYNC_TEAM_POLL_MAX (team_backoff). The local pass is not
        // affected and still runs on every SESYNC_POLL tick.
        let mut team_backoff = sesync::team_backoff::TeamBackoff::new(
            poll, duration_from_env("SESYNC_TEAM_POLL_MAX", 60.0));
        let mut next_team_at: Option<std::time::Instant> = None;
        // Conversations the last pull showed or skipped as unchanged; None before the first pull.
        let mut visible_before: Option<usize> = None;
        loop {
            one_pass(&env, &mut state);
            let team_due = next_team_at.map_or(true, |t| std::time::Instant::now() >= t);
            if team_due {
                // Set when a pull accepts a record or a push succeeds: the team interval resets.
                let mut activity = false;
                if let Some(t) = &team {
                    // Display-only: the pull verifies and counts. Nothing is written into a harness;
                    // importing a shown session stays a separate, confirmed action.
                    let out = t.pull(&state.team_seen);
                    eprintln!("sesync: team pull: {} shown, {} dropped, {} rolled back",
                        out.shown.len(), out.dropped, out.rolled_back);
                    let visible = out.shown.len() + out.unchanged;
                    activity |= !sesync::team_backoff::pull_is_idle(out.accepted.len(), visible, visible_before);
                    visible_before = Some(visible);
                    // The floor only rises: each conversation keeps the highest sequence accepted, so an
                    // older record replayed later is refused, including after a restart.
                    let before = state.team_seen.clone();
                    for (conv, seq) in &out.accepted {
                        let floor = state.team_seen.entry(conv.clone()).or_insert(0);
                        *floor = (*floor).max(*seq);
                    }
                    if state.team_seen != before {
                        if let Err(e) = state.save(&env.state_file()) {
                            eprintln!("sesync: cannot save state: {e}");
                        }
                    }
                }
                // Team push: marked sessions quiet for the team period, sealed and pushed. Nothing runs
                // unless a team is configured. O-penCode-origin sessions are skipped and reported.
                let now = now_secs();
                let conv = env.sesync_conv.clone();
                let opencode_bin = env.opencode_bin.clone();
                let scratch = env.state_dir.join("team-scratch");
                let paths: std::collections::HashMap<String, (String, f64)> = state.marked_sessions().into_iter()
                    .filter_map(|id| {
                        let key = state.sources.iter()
                            .find(|(_, s)| sesync::manage::session_id_of(&s.key) == id || s.key == id)
                            .map(|(k, _)| k.clone())?;
                        let fmt = state.sources[&key].fmt.clone();
                        let changed = state.sources[&key].last_seen_change;
                        Some((id, (format!("{fmt}\u{1f}{key}"), changed)))
                    })
                    .collect();
                let content_of = |id: &str| -> Result<Vec<u8>, String> {
                    let (tagged, _) = paths.get(id).ok_or("session is not on disk")?;
                    let (fmt, key) = tagged.split_once('\u{1f}').ok_or("bad source record")?;
                    sesync::team_sync::canonical_for_source(fmt, key, &conv, &opencode_bin, &scratch)
                };
                let last_change = |id: &str| paths.get(id).map(|(_, t)| *t);
                match sesync::team_pass::run(&team_config_dir(), &mut state, now, content_of, last_change) {
                    Ok(pass) => {
                        eprintln!("sesync: {}", sesync::team_pass::summary(&pass));
                        activity |= !pass.pushed.is_empty();
                    }
                    Err(sesync::team_pass::NotRun::NoTeam) => {}
                    Err(e) => eprintln!("sesync: team push not run: {e:?}"),
                }
                // Only a configured team backs off. Without one the team step returns at once, so it
                // keeps running on every tick, as it always has.
                if team.is_some() {
                    let before_interval = team_backoff.interval();
                    team_backoff = team_backoff.next(activity);
                    if team_backoff.interval() != before_interval {
                        eprintln!("sesync: team poll interval {}s", team_backoff.interval().as_secs_f64());
                    }
                }
                next_team_at = Some(std::time::Instant::now() + team_backoff.interval());
            }
            passes += 1;
            if limit.map_or(false, |l| passes >= l) {
                return;
            }
            std::thread::sleep(poll);
        }
    }
    eprintln!("usage: sesync --status | --once | --watch | --tui");
    std::process::exit(2);
}

/// Interactive manage screen. Raw mode and the alternate screen are restored on
/// every exit path, including a panic, so the terminal is never left unusable.
/// Ctrl+R: pull the team's shared sessions and keep only what the signed member list vouches
/// for. The team server is configured by SESYNC_TEAM_* (see team_watch); without them the
/// refresh says so, nothing else changes.
fn refresh_team(app: &mut sesync::app::App, config_dir: &std::path::Path) {
    match sesync::team_watch::TeamWatch::from_env(config_dir) {
        Some(watch) => {
            // The floors the daemon has persisted. The app only reads them; the daemon's loop raises them.
            // The app replaces its list with what this pull shows, so it asks for every conversation,
            // not only the ones that changed since the daemon's last pull.
            let out = watch.pull_all(&app.state().team_seen);
            app.set_pull_result(
                out.shown.into_iter().map(|r| (r.conversation, r.text)).collect(),
                out.dropped,
            );
        }
        None => {
            app.set_status(
                "team server not configured: set SESYNC_TEAM_URL, SESYNC_TEAM_TOKEN, SESYNC_TEAM_MACHINE and SESYNC_TEAM_SPACE",
            );
        }
    }
}

/// A team dialog was submitted: do the network step and report. Enrollment (join) saves the
/// credential itself; starting a team (create) leads into the one-time authenticator QR.
fn perform_setup(app: &mut sesync::app::App, req: sesync::app::SetupRequest, config_dir: &std::path::Path) {
    let v = |name: &str| req.value_of(name);
    match req.kind {
        sesync::app::Kind::Enroll => {
            let result = sesync::team_join::join(&v("server url"), &v("team"),
                &v("member"), &v("machine"), &v("code"), config_dir);
            match result {
                Ok(path) => {
                    app.finish_setup(Ok(format!(
                        "joined team '{}' as '{}' on '{}'; the credential is in {}",
                        v("team"), v("member"), v("machine"), path.display())));
                    // The new credential may be enough for pulls now.
                    app.set_team_configured(sesync::team_watch::TeamWatch::from_env(config_dir).is_some());
                }
                Err(e) => app.finish_setup(Err(format!("not joined: {e}"))),
            }
        }
        sesync::app::Kind::Register => {
            let base = match std::env::var("SESYNC_TEAM_URL") {
                Ok(u) => u,
                Err(_) => {
                    app.finish_setup(Err("the team server is not configured: set SESYNC_TEAM_URL".into()));
                    return;
                }
            };
            let token = match sesync::team_cli::read_admin_token(std::path::Path::new(&v("admin token file"))) {
                Ok(t) => t,
                Err(e) => {
                    app.finish_setup(Err(format!("not created: {e}")));
                    return;
                }
            };
            let members: Vec<String> = v("members").split(',')
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect();
            let cmd = sesync::team_cli::Command::Create { name: v("team"), members };
            match sesync::team_cli::run_detailed(&cmd, &base, &token, config_dir) {
                Ok((_, Some(uri))) => {
                    app.finish_setup(Ok(format!("team '{}' created", v("team"))));
                    app.show_qr(&v("team"), &uri);
                }
                Ok((_, None)) => app.finish_setup(Ok(format!("team '{}' created", v("team")))),
                Err(e) => app.finish_setup(Err(format!("not created: {e}"))),
            }
        }
    }
}

fn run_tui(
    roots: sesync::manage::Roots,
    state: State,
    lock_dir: PathBuf,
    sesync_lock: PathBuf,
    config_dir: PathBuf,
) -> std::io::Result<()> {
    use crossterm::event::{self, Event};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
    use crossterm::ExecutableCommand;
    use sesync::app::App;
    use ratatui::{backend::CrosstermBackend, Terminal};
    use std::time::{Duration, Instant};

    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ = std::io::stdout().execute(LeaveAlternateScreen);
        }
    }

    enable_raw_mode()?;
    let _restore = Restore;
    std::io::stdout().execute(EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    // Only harnesses installed on this machine are offered as mirror targets. O-penCode is not
    // offered yet: its writer is a bundle import that is not wired.
    let mut installed = Vec::new();
    if roots.omp.parent().map_or(false, |p| p.is_dir()) { installed.push("omp".to_string()); }
    if roots.pi.parent().map_or(false, |p| p.is_dir()) { installed.push("pi".to_string()); }
    if roots.claude.parent().map_or(false, |p| p.is_dir()) { installed.push("claude".to_string()); }
    let mut app = App::new(roots, state, lock_dir, sesync_lock);
    app.set_installed_targets(installed);
    // The Team tab and "Refresh team" exist before the first pull; without configuration they
    // say what is missing instead of failing silently.
    app.set_team_configured(sesync::team_watch::TeamWatch::from_env(&config_dir).is_some());

    loop {
        term.draw(|f| sesync::ui::draw(f, &app))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != crossterm::event::KeyEventKind::Press {
            continue;
        }
        // Keys that do nothing in the app do nothing to the terminal either.
        let Some(ev) = sesync::keys::key_to_ev(key.code, key.modifiers) else { continue };
        // Every key goes through the app, so every key cancels a pending delete the same way.
        // Ctrl+R only asks for a refresh; the refresh runs here, and only when the app allowed it.
        app.handle(ev, Instant::now());
        // The dialogs that ask for server inputs get the environment's defaults once, so
        // nothing secret is typed and the admin token stays on disk where it belongs.
        match app.mode() {
            sesync::app::Mode::Form => {
                if app.form_value("server url").is_empty() {
                    if let Ok(u) = std::env::var("SESYNC_TEAM_URL") {
                        app.set_form_value("server url", &u);
                    }
                }
                if app.form_value("admin token file").is_empty() {
                    if let Ok(p) = std::env::var("SESYNC_ADMIN_TOKEN_FILE") {
                        app.set_form_value("admin token file", &p);
                    }
                }
            }
            _ => {}
        }
        if let Some(req) = app.take_setup_request() {
            perform_setup(&mut app, req, &config_dir);
        }
        if app.take_refresh_request() {
            refresh_team(&mut app, &config_dir);
        }
        if app.should_quit() {
            return Ok(());
        }
    }
}

/// `sesync team ...`: parse, read the admin token from its file, and call the team server.
/// Exit 0 on success, 2 on a usage mistake, 1 on a server or token error.
fn run_team(args: &[String]) -> i32 {
    use sesync::team_cli::{parse, read_admin_token, run};
    if args.first().map(String::as_str) == Some("join") {
        return run_join(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("renew") {
        return run_renew(&args[1..]);
    }
    let cmd = match parse(args) {
        Ok(c) => c,
        Err(msg) => { eprintln!("{msg}"); return 2; }
    };
    let base = match std::env::var("SESYNC_TEAM_URL") {
        Ok(u) => u,
        Err(_) => { eprintln!("set SESYNC_TEAM_URL to the team server address"); return 2; }
    };
    let token_file = match std::env::var("SESYNC_ADMIN_TOKEN_FILE") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => { eprintln!("set SESYNC_ADMIN_TOKEN_FILE to the admin token file"); return 2; }
    };
    let token = match read_admin_token(&token_file) {
        Ok(t) => t,
        Err(msg) => { eprintln!("{msg}"); return 1; }
    };
    match run(&cmd, &base, &token, &team_config_dir()) {
        Ok(text) => { println!("{text}"); 0 }
        Err(msg) => { eprintln!("sesync team: {msg}"); 1 }
    }
}

/// `sesync team join <name> --member <m>`: asks for the 6-digit code, enrols this machine and
/// stores the token owner-only. Settings: SESYNC_TEAM_URL and SESYNC_CONFIG_DIR (optional).
fn run_join(args: &[String]) -> i32 {
    use sesync::team_join::join;
    let usage = "usage: sesync team join <name> --member <m>";
    let name = match args.first().filter(|n| !n.starts_with("--")) {
        Some(n) => n.clone(),
        None => { eprintln!("{usage}"); return 2; }
    };
    let member = match args.iter().position(|a| a == "--member").and_then(|i| args.get(i + 1)) {
        Some(m) => m.clone(),
        None => { eprintln!("{usage}"); return 2; }
    };
    let base = match std::env::var("SESYNC_TEAM_URL") {
        Ok(u) => u,
        Err(_) => { eprintln!("set SESYNC_TEAM_URL to the team server address"); return 2; }
    };
    let config = team_config_dir();
    let machine = std::env::var("SESYNC_MACHINE").unwrap_or_else(|_| {
        std::process::Command::new("hostname").output().ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_else(|| "machine".into())
    });
    eprint!("authenticator code (6 digits): ");
    let mut code = String::new();
    if std::io::stdin().read_line(&mut code).is_err() {
        eprintln!("no code read");
        return 1;
    }
    match join(&base, &name, &member, &machine, code.trim(), &config) {
        Ok(path) => { println!("joined team '{name}' as '{member}'. Credential saved to {}", path.display()); 0 }
        Err(msg) => { eprintln!("sesync team join: {msg}"); 1 }
    }
}

/// `sesync team renew <name>`: renews the stored member token while its enrolment is within
/// the 7-day window. After the window the code is needed again, via `sesync team join`.
fn run_renew(args: &[String]) -> i32 {
    use sesync::team_join::renew;
    let name = match args.first() {
        Some(n) => n.clone(),
        None => { eprintln!("usage: sesync team renew <name>"); return 2; }
    };
    let base = match std::env::var("SESYNC_TEAM_URL") {
        Ok(u) => u,
        Err(_) => { eprintln!("set SESYNC_TEAM_URL to the team server address"); return 2; }
    };
    let config = team_config_dir();
    match renew(&base, &config, &name) {
        Ok(()) => { println!("renewed team '{name}'."); 0 }
        Err(msg) => { eprintln!("sesync team renew: {msg}"); 1 }
    }
}