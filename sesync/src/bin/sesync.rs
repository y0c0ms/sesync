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
    home: PathBuf,
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
            home: home.clone(),
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
    // One OpenCode store read per pass: the rows are scan_opencode's input, and the id set
    // tells whether a recorded OpenCode mirror still exists (its store moved to SQLite).
    let opencode_sessions = if env.opencode_db.exists() {
        sesync::opencode::sessions(&env.opencode_db)
    } else {
        Vec::new()
    };
    // Turn-taking first: a mirror the user answered in becomes the session's source, and the
    // promoted source is synced in this same pass, without the quiet window — the whole point
    // is that the other harnesses hold the updated copy by the time the user opens them.
    let rows: Vec<(String, f64)> = opencode_sessions.iter().map(|s| (s.id.clone(), s.updated)).collect();
    for key in sesync::turn::promote_changed(state, &rows) {
        let title = state.sources.get(&key).map(|s| s.title.clone()).unwrap_or_default();
        eprintln!("sesync: '{}' answered elsewhere; it becomes the source now", title);
        out.push(PassSource { key, title, urgent: true });
    }
    let opencode_ids: Option<std::collections::BTreeSet<String>> = if opencode_sessions.is_empty() {
        None
    } else {
        Some(opencode_sessions.iter().map(|s| s.id.clone()).collect())
    };
    let home = env.home.to_string_lossy().to_string();
    // Claude sessions are sources too (the Python daemon synced them; the Rust port had
    // dropped this path). Each format lists its own main sessions.
    let claude_root = env.claude.clone();
    let mut sources: Vec<(&str, Vec<PathBuf>)> = vec![
        ("omp", if env.omp.is_dir() { discovery::jsonl_sessions(&env.omp) } else { Vec::new() }),
        ("pi", if env.pi.is_dir() { discovery::jsonl_sessions(&env.pi) } else { Vec::new() }),
        ("claude", discovery::claude_sessions(&claude_root)),
        ("gemini", if env.gemini.is_dir() { discovery::gemini_sessions(&env.gemini) } else { Vec::new() }),
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
            if src.conv.is_empty() {
                src.conv = sesync::manage::conversation_id(&key);
            }
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
            // A chosen target with no mirror — or one recorded where its harness will never
            // list it (mirror_stale) — forces a pass. Repeated failures put the retries into
            // backoff (mirror_demand), so a transcript that can never convert is not
            // attempted every pass forever.
            let needs_cwd = src.chosen_targets.iter()
                .any(|t| (t == "omp" || t == "pi") && src.mirrors.contains_key(t));
            let cwd = if needs_cwd { sesync::buckets::cwd_from_head(fmt, &path) } else { None };
            let missing = src.chosen_targets.iter().any(|t| match src.mirrors.get(t) {
                None => true,
                Some(m) => sesync::scan::mirror_stale(t, &m.path, cwd.as_deref(), &home, opencode_ids.as_ref()),
            });
            let (wanted, needs_mirror) = sesync::scan::mirror_demand(
                missing, src.chosen_targets.len(), src.mirrors.len(),
                src.failures, src.last_failure, now, env.quiet_secs);
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
                needs_mirror,
            };
            if !should_sync(&input) || meta.len() < MIN_BYTES {
                continue;
            }
            let title = titles::read_title_text(&path)
                .map(|text| {
                    let lines: Vec<&str> = text.lines().collect();
                    if fmt == "claude" {
                        titles::claude_title_from_lines(&lines).0
                    } else if fmt == "gemini" {
                        titles::gemini_title_from_lines(&lines).0
                    } else {
                        titles::omp_title_from_lines(&lines).0
                    }
                })
                .unwrap_or_default();
            src.last_hash = hash;
            out.push(PassSource { key, title: if title.is_empty() { "untitled".into() } else { title }, urgent: false });
        }
    }
    out.extend(scan_opencode(env, state, now, &opencode_sessions));
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
fn scan_opencode(env: &Env, state: &mut State, now: f64, sessions: &[sesync::opencode::Session]) -> Vec<PassSource> {
    let mut out = Vec::new();
    let home = env.home.to_string_lossy().to_string();
    let ids: Option<std::collections::BTreeSet<String>> = if sessions.is_empty() {
        None
    } else {
        Some(sessions.iter().map(|s| s.id.clone()).collect())
    };
    for s in sessions {
        if state.mirror_keys.contains(&s.id) {
            continue;
        }
        // A row recorded as an O-penCode mirror of another session's copy — turn-taking moved
        // the source away — is a mirror, never a source.
        if state.sources.values().any(|v| v.mirrors.get("opencode").map_or(false, |m| m.path == s.id)) {
            continue;
        }
        // A choice whose mirror has not been written yet outranks the age window, exactly as
        // for the file-backed sources. A mirror recorded in the wrong bucket (or an OpenCode
        // id that no longer exists after its store migration) counts as unwritten.
        let pending = state.sources.get(&s.id)
            .map(|src| src.chosen_targets.iter().any(|t| match src.mirrors.get(t) {
                None => true,
                Some(m) => sesync::scan::mirror_stale(t, &m.path, Some(&s.directory), &home, ids.as_ref()),
            }))
            .unwrap_or(false);
        if !pending && now - s.updated > env.max_age_secs {
            continue;
        }
        let src = state.sources.entry(s.id.clone()).or_insert_with(|| Source::new("opencode", &s.id));
        if src.conv.is_empty() {
            src.conv = sesync::manage::conversation_id(&s.id);
        }
        if s.updated > src.last_seen_change {
            src.last_seen_change = s.updated;
        }
        if now - src.last_seen_change < env.quiet_secs {
            continue;
        }
        let stamp = format!("{:.3}", s.updated);
        if stamp == src.last_hash && !pending {
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
        out.push(PassSource { key: s.id.clone(), title, urgent: false });
    }
    out
}

/// One pass: scan the stores and record what changed in sesync's own state. Nothing is written
/// into any harness store, so a session stays exactly where its harness keeps it.
fn one_pass(env: &Env, state: &mut State) -> usize {
    // The TUI writes the same file while this loop runs. Adopt its choices before the scan
    // and once more before the save, so a keystroke is never rolled back by this pass.
    if let Err(e) = state.absorb_user_edits(&env.state_file()) {
        eprintln!("sesync: cannot read user choices from state: {e}");
    }
    let changed = scan(env, state);
    let attempted = changed.len();
    for s in &changed {
        sync_chosen(env, state, s);
    }
    if let Err(e) = state.absorb_user_edits(&env.state_file()) {
        eprintln!("sesync: cannot read user choices from state: {e}");
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
    if chosen.is_empty() {
        return;
    }
    // An O-penCode source has no file to hand the converter: export it with its own CLI and
    // convert once, then install the same journal into every chosen target. A Claude
    // transcript or a Gemini session log is not a journal either: convert it the same way,
    // or the target converter would pass the raw records through unchanged.
    let mut journal: Option<std::path::PathBuf> = None;
    if fmt == "opencode" || fmt == "claude" || fmt == "gemini" {
        let scratch = env.state_dir.join("mirror-scratch");
        let converted = if fmt == "opencode" {
            sesync::opencode::export_and_convert(&env.opencode_bin, &env.sesync_conv, &s.key, &scratch)
        } else if fmt == "gemini" {
            sesync::team_sync::canonical_bytes(
                sesync::team_sync::Canonical::Gemini(Path::new(&s.key)),
                &env.sesync_conv,
                &scratch,
            )
            .map(|_| scratch.join("canonical.jsonl"))
        } else {
            sesync::team_sync::canonical_bytes(
                sesync::team_sync::Canonical::Claude(Path::new(&s.key)),
                &env.sesync_conv,
                &scratch,
            )
            .map(|_| scratch.join("canonical.jsonl"))
        };
        match converted {
            Ok(j) => journal = Some(j),
            Err(e) => {
                if let Some(src) = state.sources.get_mut(&s.key) {
                    src.failures += 1;
                    src.last_failure = now_secs();
                }
                eprintln!("mirror {} failed: {e}", s.key);
                return;
            }
        }
    }
    let stores = sesync::mirror_write::Stores {
        home: &env.home,
        omp: &env.omp,
        pi: &env.pi,
        claude: &env.claude,
        gemini: &env.gemini,
        opencode_bin: &env.opencode_bin,
        opencode_db: &env.opencode_db,
        sesync_conv: &env.sesync_conv,
        sesync_lock: &env.sesync_lock,
    };
    let conv = sesync::discovery::parse_omp_name(
        Path::new(&s.key).file_name().and_then(|n| n.to_str()).unwrap_or(""),
    ).unwrap_or_else(|| s.key.clone());
    let mut failed = false;
    for target in chosen {
        if target == fmt {
            continue;
        }
        let written = match &journal {
            Some(j) => sesync::mirror_write::write_mirror_from_journal(
                &stores, state, &s.key, &conv, &s.title, &target, j),
            None => sesync::mirror_write::write_mirror(
                &stores, state, &s.key, &fmt, &conv, &s.title, &target),
        };
        if let Err(e) = written {
            failed = true;
            if let Some(src) = state.sources.get_mut(&s.key) {
                src.failures += 1;
                src.last_failure = now_secs();
            }
            eprintln!("mirror {} -> {target} failed: {e}", s.key);
        }
    }
    if !failed {
        // This pass wrote every chosen target: clear the failure record so the retry
        // backoff in scan does not hold a mirror that is already fine.
        if let Some(src) = state.sources.get_mut(&s.key) {
            src.failures = 0;
            src.last_failure = 0.0;
        }
    }
    if let Some(j) = journal {
        let _ = std::fs::remove_file(j);
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
    if let Err(e) = state.absorb_user_edits(&env.state_file()) {
        eprintln!("sesync: cannot read user choices from state: {e}");
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
            gemini: env.gemini.join("tmp"),
        };
        if let Err(e) = run_tui(roots, state, env.lock_dir(), env.sesync_lock.clone(), team_config_dir(), env.state_file(), env.opencode_bin.clone()) {
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
                        if let Err(e) = state.absorb_user_edits(&env.state_file()) {
                            eprintln!("sesync: cannot read user choices from state: {e}");
                        }
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
/// Ctrl+R prompt: pull the team's shared sessions on a worker thread, so the terminal stays
/// live. Keep only what the signed member list vouches for; the team server is configured by
/// SESYNC_TEAM_* (see team_watch); without them the refresh says so, nothing else changes.

/// The pull in flight, checked between key events.
enum TeamPull {
    /// The request was refused before starting (not configured, or one already running).
    None,
    /// A worker is pulling; the receiver returns its verified outcome.
    Running(std::sync::mpsc::Receiver<sesync::team_pull_verified::PullOutcome>),
}

/// Put the result on the app: the verified sessions and the refuse count. Runs on the main
/// thread after the worker sent its outcome.
fn apply_pull(app: &mut sesync::app::App, out: sesync::team_pull_verified::PullOutcome) {
    let sesync::team_pull_verified::PullOutcome { shown, dropped, accepted, .. } = out;
    app.set_pull_result(shown.into_iter().map(|r| (r.conversation, r.text)).collect(), dropped);
    app.absorb_pull_floors(&accepted);
}

/// Start a `TeamPull::Running` worker, or explain why not.
fn request_team_pull(app: &mut sesync::app::App, config_dir: &std::path::Path) -> TeamPull {
    let Some(watch) = sesync::team_watch::TeamWatch::from_env(config_dir) else {
        app.set_error(
            "team server not configured: set SESYNC_TEAM_URL, SESYNC_TEAM_TOKEN, SESYNC_TEAM_MACHINE and SESYNC_TEAM_SPACE",
        );
        return TeamPull::None;
    };
    if !app.begin_pull() {
        return TeamPull::None;
    }
    // The floors the daemon has persisted. The app only reads them; the daemon's loop raises them.
    // The app replaces its list with what this pull shows, so it asks for every conversation,
    // not only the ones that changed since the daemon's last pull.
    let seen = app.state().team_seen.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = watch.pull_all(&seen);
        let _ = tx.send(out);
    });
    TeamPull::Running(rx)
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

/// Whether a tool can be run: an explicit path is checked directly, a bare name is looked up
/// on PATH. Used for the installed-harness probe, which must not start the tool.
fn command_available(tool: &Path) -> bool {
    if tool.parent().map_or(false, |d| !d.as_os_str().is_empty()) {
        return tool.is_file();
    }
    std::env::var_os("PATH").map_or(false, |paths| {
        std::env::split_paths(&paths).any(|d| d.join(tool).is_file())
    })
}

/// (mtime, size) of the state file. The TUI reloads the daemon's fields and re-lists the
/// sessions only when this changes, so an idle keystroke and an idle second cost a stat.
fn state_stamp(path: &Path) -> Option<(u128, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos(), m.len()))
}

fn run_tui(
    roots: sesync::manage::Roots,
    state: State,
    lock_dir: PathBuf,
    sesync_lock: PathBuf,
    config_dir: PathBuf,
    state_file: PathBuf,
    opencode_bin: PathBuf,
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
    // Only harnesses installed on this machine are offered as mirror targets. O-penCode is
    // offered when its CLI can be run: the mirror is an `opencode import`, not a file.
    let mut installed = Vec::new();
    if roots.omp.parent().map_or(false, |p| p.is_dir()) { installed.push("omp".to_string()); }
    if roots.pi.parent().map_or(false, |p| p.is_dir()) { installed.push("pi".to_string()); }
    if roots.claude.parent().map_or(false, |p| p.is_dir()) { installed.push("claude".to_string()); }
    if roots.gemini.parent().map_or(false, |p| p.is_dir()) { installed.push("gemini".to_string()); }
    if command_available(&opencode_bin) { installed.push("opencode".to_string()); }
    let mut app = App::new(roots, state, lock_dir, sesync_lock);
    app.set_installed_targets(installed);
    // Restore last-run taste (theme, tab) from this user's configuration directory.
    let prefs = sesync::app::prefs::load(&config_dir);
    app.restore_prefs(prefs.theme, prefs.view);
    // The Team tab and "Refresh team" exist before the first pull; without configuration they
    // say what is missing instead of failing silently.
    app.set_team_configured(sesync::team_watch::TeamWatch::from_env(&config_dir).is_some());

    // The state file is read back only when it changed since the last look: the daemon
    // rewrites it on every pass, and a keystroke must not pay for a reload that shows
    // nothing new.
    let mut seen_state = state_stamp(&state_file);
    let mut last_merge = Instant::now();
    let mut pending_pull = TeamPull::None;
    loop {
        term.draw(|f| sesync::ui::draw(f, &app))?;
        if !event::poll(Duration::from_millis(200))? {
            // A running daemon writes mirrors and floors into the same file; take them in
            // about once a second so the table shows them without a keypress.
            if last_merge.elapsed() >= Duration::from_secs(1) {
                last_merge = Instant::now();
                let stamp = state_stamp(&state_file);
                if stamp != seen_state {
                    seen_state = stamp;
                    if let Ok(disk) = State::load(&state_file) {
                        app.absorb_daemon_state(&disk);
                        app.refresh_rows();
                    }
                }
            }
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != crossterm::event::KeyEventKind::Press {
            continue;
        }
        // Keys that do nothing in the app do nothing to the terminal either.
        let Some(ev) = sesync::keys::key_to_ev(key.code, key.modifiers) else { continue };
        // Every key goes through the app, so every key cancels a pending delete the same way.
        // Ctrl+R on the Team tab asks for a network refresh; the local list refreshes in the app.
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
        // A pull the app asked for (ctrl+r on the team tab): start it or collect the outcome of
        // the one already running. The terminal keeps ticking either way.
        if app.take_refresh_request() {
            pending_pull = request_team_pull(&mut app, &config_dir);
        }
        if let TeamPull::Running(rx) = &pending_pull {
            match rx.try_recv() {
                Ok(out) => {
                    apply_pull(&mut app, out);
                    pending_pull = TeamPull::None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    app.pull_failed();
                    pending_pull = TeamPull::None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        // The app owns choices (mirror targets, marks) that must survive the process. Take
        // the daemon's fields when it wrote them, then save only when this key changed a
        // user-owned field: a save from the app must never roll back a mirror record or a
        // team floor the daemon just wrote, and an unrelated key costs no disk I/O.
        let stamp = state_stamp(&state_file);
        if stamp != seen_state {
            if let Ok(disk) = State::load(&state_file) {
                app.absorb_daemon_state(&disk);
            }
            seen_state = stamp;
        }
        if app.take_user_dirty() {
            if let Err(e) = app.state().save_user(&state_file) {
                eprintln!("sesync: cannot save state: {e}");
            } else {
                seen_state = state_stamp(&state_file);
            }
        }
        if app.take_prefs_dirty() {
            if let Err(e) = sesync::app::prefs::save(&config_dir, &app.theme_name().to_string(), app.view()) {
                eprintln!("sesync: cannot save TUI preferences: {e}");
            }
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