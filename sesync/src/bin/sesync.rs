//! sesync: Rust replacement for `python daemon/sesync.py`.
//! Modes: `--status` prints the mirror view; `--once` runs one sync pass.
//! Continuous polling is the same pass repeated on an interval (not yet wired).

use sesync::harness::{self, Harness, Kind, Locations, Row, Tools};
use sesync::mirror::{copy_hash, copy_mtime, copy_stamp};
use sesync::pass::{format_status, PassSource};
use sesync::scan::{should_sync, ScanInput, MIN_BYTES};
use sesync::state::{Mirror, Source, State};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct Env {
    state_dir: PathBuf,
    home: PathBuf,
    /// Every harness's store and command, each read from that harness's own variables.
    locations: Locations,
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
        // Windows binaries carry `.exe`, so the bare name is not the file that exists.
        let beside_me = |name: &str| -> PathBuf {
            let candidates: Vec<PathBuf> = harness::executable_candidates(Path::new(name));
            std::env::current_exe().ok()
                .and_then(|e| e.parent().map(|d| candidates.iter().map(|c| d.join(c)).collect::<Vec<_>>()))
                .and_then(|ps| ps.into_iter().find(|p| p.exists()))
                .unwrap_or_else(|| home.join(".local/bin").join(name))
        };
        Env {
            state_dir: get("SESYNC_DIR", home.join(".local/share/sesync")),
            home: home.clone(),
            locations: Locations::from_env(&home),
            sesync_conv: get("SESYNC_CONV", beside_me("sesync-conv")),
            sesync_lock: get("SESYNC_LOCK", beside_me("sesync-lock")),
            quiet_secs: num("SESYNC_QUIET", 20.0),
            max_age_secs: num("SESYNC_MAX_AGE_DAYS", 14.0) * 86400.0,
        }
    }
    fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }
    fn tools(&self) -> Tools<'_> {
        Tools { home: &self.home, sesync_conv: &self.sesync_conv }
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

/// Mirrors the daemon created, so a change to them is never treated as a source.
fn is_mirror(state: &State, key: &str) -> bool {
    state.mirror_keys.contains(key)
}

/// What sesync lets go of at every load and after every merge with the TUI's file. The records of
/// harnesses it no longer syncs (`harness::RETIRED`): their sources move to their best surviving
/// copy (one its own harness still has), their targets are released, and none of their files is
/// ever touched. And the sessions the user deleted in the TUI (`State::forget_deleted`): the
/// record goes and the copies are released. Cheap and idempotent.
fn release_records(locations: &Locations, state: &mut State) -> Vec<String> {
    let exists = |target: &str, key: &str| harness::get(target).is_some_and(|h| h.copy_exists(locations.of(target), key));
    let mut log = Vec::new();
    for retired in harness::RETIRED {
        log.extend(state.retire_harness(retired, &exists));
    }
    log.extend(state.forget_deleted(now_secs()));
    log
}

/// `release_records`, reporting each change on stderr (the daemon and the CLI modes; the TUI's
/// screen would be garbled by it).
fn release_records_logged(locations: &Locations, state: &mut State) {
    for line in release_records(locations, state) {
        eprintln!("sesync: {line}");
    }
}

/// Find sources that need a sync, and the title to give their mirror.
fn scan(env: &Env, state: &mut State) -> Vec<PassSource> {
    let now = now_secs();
    let mut out = Vec::new();
    // One database read per store harness per pass: the rows are scan_store's input, and the ids
    // tell whether a copy recorded there still exists.
    let stores: Vec<(&'static dyn Harness, Vec<Row>)> = harness::ALL.iter()
        .filter(|h| h.kind() == Kind::Store)
        .map(|h| (*h, h.rows(env.locations.of(h.id()))))
        .collect();
    // Turn-taking first: a mirror the user answered in becomes the session's source, and the
    // promoted source is synced in this same pass, without the quiet window: the whole point
    // is that the other harnesses hold the updated copy by the time the user opens them. An
    // answer is a user prompt newer than every prompt of the source, read by each harness from
    // its own records.
    let rows: sesync::turn::StoreRows = stores.iter()
        .map(|(h, rows)| (h.id().to_string(), rows.iter().map(|r| (r.id.clone(), r.updated)).collect()))
        .collect();
    let locations = &env.locations;
    let prompt_of = |fmt: &str, key: &str| -> Option<f64> { harness::get(fmt)?.newest_prompt(locations.of(fmt), key) };
    for key in sesync::turn::promote_changed(state, &rows, &prompt_of) {
        let title = state.sources.get(&key).map(|s| s.title.clone()).unwrap_or_default();
        eprintln!("sesync: '{}' answered elsewhere; it becomes the source now", title);
        out.push(PassSource { key, title, urgent: true });
    }
    // Every copy recorded on some source, files and store ids alike. A copy is never a source,
    // even when an older binary's record left it out of `mirror_keys`.
    let recorded = state.mirror_paths();
    // A store that listed nothing was not read: none of its copies is called stale.
    let store_ids: BTreeMap<String, BTreeSet<String>> = stores.iter()
        .filter(|(_, rows)| !rows.is_empty())
        .map(|(h, rows)| (h.id().to_string(), rows.iter().map(|r| r.id.clone()).collect()))
        .collect();
    let home = env.home.to_string_lossy().to_string();
    for h in harness::ALL.iter().filter(|h| h.kind() == Kind::Files) {
        let fmt = h.id();
        for path in h.sessions(env.locations.of(fmt)) {
            let key = path.display().to_string();
            if is_mirror(state, &key) || recorded.contains(&key) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            // A conversation of several files (a database and its WAL) changes in any of them:
            // its time, stamp and hash cover them together.
            let mtime = copy_mtime(fmt, &path);
            let src = state.sources.entry(key.clone()).or_insert_with(|| Source::new(fmt, &key));
            if src.conv.is_empty() {
                src.conv = sesync::manage::conversation_id(&key);
            }
            if mtime > src.last_seen_change {
                src.last_seen_change = mtime;
            }
            let stamp = copy_stamp(fmt, &path);
            let hash = if stamp == src.last_stamp && !src.last_hash.is_empty() {
                src.last_hash.clone()
            } else {
                let h = copy_hash(fmt, &path).unwrap_or_default();
                src.last_stamp = stamp;
                h
            };
            let hash_changed = hash != src.last_hash;
            let live: Vec<&Mirror> = src.mirrors.values().filter(|m| !m.diverged).collect();
            let live_current = live.iter().all(|m| m.source_hash == hash);
            // Mirrors are wanted only for targets the user chose for this session. A source
            // with no choice is "changed" only when its content changed, and writes nothing.
            // A chosen target with no mirror, or one recorded where its harness will never list
            // it (mirror_stale), forces a pass. Repeated failures put the retries into backoff
            // (mirror_demand), so a transcript that can never convert is not attempted every
            // pass forever.
            let needs_cwd = src.chosen_targets.iter()
                .any(|t| harness::get(t).is_some_and(|t| t.placed_by_cwd()) && src.mirrors.contains_key(t));
            let cwd = if needs_cwd { h.cwd(&path) } else { None };
            let missing = src.chosen_targets.iter().any(|t| match src.mirrors.get(t) {
                None => true,
                Some(m) => sesync::scan::mirror_stale(t, &m.path, cwd.as_deref(), &home, &store_ids),
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
            let title = h.title(&path);
            src.last_hash = hash;
            out.push(PassSource { key, title: if title.is_empty() { "untitled".into() } else { title }, urgent: false });
        }
    }
    for (h, rows) in &stores {
        out.extend(scan_store(env, state, now, *h, rows, &store_ids));
    }
    record_subagents(env, state);
    out
}

/// Record every subagent transcript under its parent session, so the TUI can show it there.
/// Harnesses that keep subagent files beside their parent list them (`Harness::subagents`).
fn record_subagents(env: &Env, state: &mut State) {
    let mut found: BTreeMap<String, Vec<String>> = Default::default();
    for h in harness::ALL {
        for (parent, path) in h.subagents(env.locations.of(h.id())) {
            found.entry(parent).or_default().push(path.display().to_string());
        }
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

/// The conversations of a store harness that changed. The same rules as for files: live sessions
/// only, none older than the window, wait for quiet, and re-sync only when the row's update time
/// moved. The title is the harness's (`Harness::row_title`).
fn scan_store(env: &Env, state: &mut State, now: f64, h: &dyn Harness, rows: &[Row],
              store_ids: &BTreeMap<String, BTreeSet<String>>) -> Vec<PassSource> {
    let mut out = Vec::new();
    let home = env.home.to_string_lossy().to_string();
    let loc = env.locations.of(h.id());
    for s in rows {
        if state.mirror_keys.contains(&s.id) {
            continue;
        }
        // A row recorded as a copy of another session's conversation (turn-taking moved the
        // source away) is a mirror, never a source.
        if state.sources.values().any(|v| v.mirrors.get(h.id()).map_or(false, |m| m.path == s.id)) {
            continue;
        }
        // A choice whose mirror has not been written yet outranks the age window, exactly as
        // for the file sources. A mirror recorded in the wrong folder (or a store id that no
        // longer exists) counts as unwritten.
        let pending = state.sources.get(&s.id)
            .map(|src| src.chosen_targets.iter().any(|t| match src.mirrors.get(t) {
                None => true,
                Some(m) => sesync::scan::mirror_stale(t, &m.path, Some(&s.directory), &home, store_ids),
            }))
            .unwrap_or(false);
        if !pending && now - s.updated > env.max_age_secs {
            continue;
        }
        let src = state.sources.entry(s.id.clone()).or_insert_with(|| Source::new(h.id(), &s.id));
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
        let title = h.row_title(loc, s);
        // Keep the name on the source, so the TUI can show it without reading the database.
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
    // A choice saved by an older TUI can still name a retired harness, and the TUI may have
    // deleted a session since the last pass.
    release_records_logged(&env.locations, state);
    let changed = scan(env, state);
    let attempted = changed.len();
    // Un-choosing a target has to drop the copy, and that is not a content change, so it runs on
    // every pass rather than inside the `changed` loop.
    for line in drop_unchosen(env, state) {
        println!("sesync: {line}");
    }
    for s in &changed {
        sync_chosen(env, state, s);
    }
    settle_sync_records(state);
    if let Err(e) = state.absorb_user_edits(&env.state_file()) {
        eprintln!("sesync: cannot read user choices from state: {e}");
    }
    release_records_logged(&env.locations, state);
    if let Err(e) = state.save(&env.state_file()) {
        eprintln!("sesync: cannot save state: {e}");
    }
    attempted
}

/// Keep the per-target records true for the TUI on every pass, not only for sources that
/// changed: a target the user no longer chose loses its record (an un-chosen failure stops being
/// reported at once), and a file copy written before the record existed gets its write time once
/// from its modified time. A copy in a store harness gets none: its row time is the imported
/// conversation's, not the moment sesync wrote it, so it waits for its next write. A copy that
/// does not hold what sesync wrote (diverged, or bytes sesync did not write) gets none either:
/// its time is not a sync.
fn settle_sync_records(state: &mut State) {
    for src in state.sources.values_mut() {
        let chosen = &src.chosen_targets;
        src.sync.retain(|t, _| chosen.contains(t));
        let missing: Vec<String> = src.chosen_targets.iter()
            .filter(|t| **t != src.fmt && !harness::is_store(t) && !src.sync.contains_key(*t))
            .cloned()
            .collect();
        for t in missing {
            let Some(m) = src.mirrors.get(&t) else { continue };
            if m.diverged || m.installed_hash.is_empty() {
                continue;
            }
            let at = copy_mtime(&t, Path::new(&m.path));
            if at > 0.0 {
                src.sync.insert(t, sesync::state::TargetSync { ok_at: at, ..Default::default() });
            }
        }
    }
}

/// Drop the mirrors the user no longer wants. Un-choosing a target is how a session stops being
/// mirrored there, and nothing else removes the copy: `sesync mirrors` only plans what is still
/// recorded, and it declines for store harnesses by design ("remove it through the harness"). File
/// mirrors go through the same safety rule and logging as that command; a copy in a store harness
/// is removed through the harness itself, never by touching a file.
fn drop_unchosen(env: &Env, state: &mut State) -> Vec<String> {
    let planned = sesync::mirror_cleanup::plan_unchosen(state);
    let (stored, rest): (Vec<_>, Vec<_>) = planned.into_iter().partition(|p| harness::is_store(&p.target));
    // Only the copies that provably lose nothing are deleted; the rest are released, so the record
    // stops claiming a mirror sesync will never update again while the copy itself stays.
    let (removable, releasable): (Vec<_>, Vec<_>) = rest
        .into_iter()
        .partition(|p| matches!(p.verdict, sesync::mirror_cleanup::Verdict::Remove));
    let mut log = sesync::mirror_cleanup::apply(state, &removable, &env.lock_dir(), &env.sesync_lock);
    log.extend(sesync::mirror_cleanup::release(state, &releasable));
    for p in &stored {
        let Some(h) = harness::get(&p.target) else { continue };
        let loc = env.locations.of(&p.target);
        // The same rule as for files: only a copy that still holds exactly what sesync imported
        // is deleted. A session somebody continued, or the old source a promotion demoted (the
        // user's own conversation), is released and kept.
        let Some(now) = h.fingerprint(loc, &p.path) else {
            sesync::mirror_cleanup::forget(state, p);
            log.push(format!("cleared {} {} (already gone)", p.target, p.path));
            continue;
        };
        if p.installed_hash.is_empty() || now != p.installed_hash {
            sesync::mirror_cleanup::forget(state, p);
            log.push(format!("released {} {} (kept the session: sesync did not write what it holds)", p.target, p.path));
            continue;
        }
        match h.remove(loc, &p.path) {
            Ok(()) if h.fingerprint(loc, &p.path).is_none() => {
                sesync::mirror_cleanup::forget(state, p);
                log.push(format!("removed {} {}", p.target, p.path));
            }
            Ok(()) => log.push(format!("kept    {} {}: the harness reported success but the session is still there", p.target, p.path)),
            Err(e) => log.push(format!("kept    {} {}: {e}", p.target, p.path)),
        }
    }
    log
}

/// A failure that stops the whole source (a conversion error, an empty export): count it on the
/// source and record the same message on every chosen target.
fn fail_source(state: &mut State, key: &str, targets: &[String], msg: &str) {
    let now = now_secs();
    if let Some(src) = state.sources.get_mut(key) {
        src.failures += 1;
        src.last_failure = now;
        for t in targets {
            src.record_sync(t, Err(msg), now);
        }
    }
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
    // A source whose files are not canonical journals (a store harness's session, a transcript,
    // a database) is converted once by its harness, then the same journal is installed into
    // every chosen target; otherwise the target converter would pass the raw records through.
    let scratch = env.state_dir.join("mirror-scratch");
    let converted = match harness::get(&fmt) {
        Some(h) => h.to_journal(env.locations.of(&fmt), &env.tools(), &s.key, &scratch),
        None => Err(format!("no harness named '{fmt}'")),
    };
    let journal: Option<PathBuf> = match converted {
        Ok(j) => j,
        Err(e) => {
            fail_source(state, &s.key, &chosen, &format!("conversion failed: {e}"));
            eprintln!("mirror {} failed: {e}", s.key);
            return;
        }
    };
    // Nothing replaces a copy with an empty conversation. An export with no message (today: an
    // OpenCode session a failed import had emptied) would otherwise overwrite every copy that
    // still holds the conversation. Counted as a failure, so the retry backs off.
    let content = journal.clone().unwrap_or_else(|| PathBuf::from(&s.key));
    if !sesync::scan::journal_has_messages(&content) {
        fail_source(state, &s.key, &chosen, "the source holds no messages");
        eprintln!("sesync: '{}' holds no messages; its copies were left as they are", s.title);
        if let Some(j) = journal {
            let _ = std::fs::remove_file(j);
        }
        return;
    }
    let archive = env.state_dir.join("archive");
    let lock_dir = env.lock_dir();
    let stores = sesync::mirror_write::Stores {
        locations: &env.locations,
        home: &env.home,
        sesync_conv: &env.sesync_conv,
        sesync_lock: &env.sesync_lock,
        archive: &archive,
        lock_dir: &lock_dir,
    };
    // Every write claims the conversation's stable id, the name every copy's alias points at, so
    // a turn in any harness and this write exclude each other. The source's own session id is
    // aliased too (an OpenCode id keeps its `ses_` prefix, the conversation id does not).
    let conv = state.team_id_of(&s.key);
    if env.sesync_lock.exists() {
        if let Err(e) = sesync::lock::write_alias(&lock_dir, &sesync::manage::session_id_of(&s.key), &conv) {
            eprintln!("sesync: cannot record the lock alias for {}: {e}", s.key);
        }
    }
    if let Some(src) = state.sources.get_mut(&s.key) {
        // Targets the user no longer chose keep no record.
        src.sync.retain(|t, _| chosen.contains(t));
    }
    let mut failed = false;
    for target in chosen {
        if target == fmt {
            continue;
        }
        let written = sesync::mirror_write::write_mirror(&stores, state, &s.key, &conv, &s.title, &target, &content);
        let now = now_secs();
        match written {
            Ok(_) => {
                if let Some(src) = state.sources.get_mut(&s.key) {
                    src.record_sync(&target, Ok(()), now);
                }
            }
            Err(e) => {
                failed = true;
                let msg = e.to_string();
                if let Some(src) = state.sources.get_mut(&s.key) {
                    src.failures += 1;
                    src.last_failure = now;
                    src.record_sync(&target, Err(msg.as_str()), now);
                }
                eprintln!("mirror {} -> {target} failed: {e}", s.key);
            }
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
    release_records_logged(&env.locations, state);
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
    // Released before any mode reads the records: the retired Gemini CLI (so neither the daemon,
    // the TUI nor `sesync mirrors` ever acts on a Gemini copy, never deleted, only released), and
    // the sessions the user deleted.
    release_records_logged(&env.locations, &mut state);
    if args.first().map(String::as_str) == Some("mirrors") {
        std::process::exit(run_mirrors(&env, &mut state, args.iter().any(|a| a == "--apply")));
    }
    if args.iter().any(|a| a == "--tui") {
        let roots = sesync::manage::Roots {
            locations: env.locations.clone(),
            home: env.home.clone(),
            sesync_conv: env.sesync_conv.clone(),
        };
        if let Err(e) = run_tui(roots, state, env.lock_dir(), env.sesync_lock.clone(), team_config_dir(), env.state_file()) {
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
        // The team side is optional: configured by a joined credential or the team environment (see
        // team_watch). A machine can belong to more than one team; every one of them is pulled and
        // pushed. The teams are read again at every team step, after renewing any token about to
        // run out, so a renewal, a new join or a `sesync team renew` is used without a restart.
        let mut teams = sesync::team_watch::teams_from_env(&team_config_dir());
        state.migrate_legacy_team_state(&teams.iter().map(|t| t.space.clone()).collect::<Vec<_>>());
        // The team pull and push run on their own interval: SESYNC_POLL at first, doubling while the
        // team has nothing new, capped at SESYNC_TEAM_POLL_MAX (team_backoff). The local pass is not
        // affected and still runs on every SESYNC_POLL tick.
        let mut team_backoff = sesync::team_backoff::TeamBackoff::new(
            poll, duration_from_env("SESYNC_TEAM_POLL_MAX", 60.0));
        let mut next_team_at: Option<std::time::Instant> = None;
        // Per team, the conversations its last pull showed or skipped as unchanged; absent before
        // that team's first pull. Per team, so one team's count is never compared with another's.
        let mut visible_before: std::collections::BTreeMap<String, usize> = Default::default();
        // The last renewal error reported per team, so a lasting refusal is logged once.
        let mut renew_errors: std::collections::BTreeMap<String, String> = Default::default();
        // The last line reported per waiting join, so "waiting for the admin" is logged once per
        // team per run, not on every team step.
        let mut join_notes: std::collections::BTreeMap<String, String> = Default::default();
        loop {
            one_pass(&env, &mut state);
            let team_due = next_team_at.map_or(true, |t| std::time::Instant::now() >= t);
            if team_due {
                // Set when a pull accepts a new record, a push succeeds or the admin approves a
                // waiting join: the team interval resets.
                let mut activity = false;
                let mut waiting = false;
                for (space, r) in sesync::team_join::check_all_waiting(&team_config_dir()) {
                    let note = match &r {
                        Ok(a) => a.describe(&space),
                        Err(e) => format!("team [{space}]: cannot check the join request: {e}"),
                    };
                    match r {
                        Ok(sesync::team_join::Approval::Approved { .. }) => activity = true,
                        _ => waiting = true,
                    }
                    if join_notes.get(&space) != Some(&note) {
                        eprintln!("sesync: {note}");
                        join_notes.insert(space, note);
                    }
                }
                for (space, r) in sesync::team_join::renew_all_due(&team_config_dir(), now_secs() as u64) {
                    match r {
                        Ok(true) => { renew_errors.remove(&space); eprintln!("sesync: team [{space}]: token renewed"); }
                        Ok(false) => { renew_errors.remove(&space); }
                        Err(e) => {
                            if renew_errors.get(&space) != Some(&e) {
                                eprintln!("sesync: team [{space}]: cannot renew the token: {e}");
                                renew_errors.insert(space, e);
                            }
                        }
                    }
                }
                teams = sesync::team_watch::teams_from_env(&team_config_dir());
                if !teams.is_empty() {
                    // Display-only: the pull verifies and counts. Nothing is written into a harness;
                    // importing a shown session stays a separate, confirmed action.
                    let pulls = sesync::team_watch::TeamWatch::pull_each(&teams, &state.floors_all());
                    let before = state.team_seen.clone();
                    // A newer member list a pull verified is kept, so the list held here only
                    // moves forward (a revoke stays in force whatever a server sends later).
                    for (space, kept) in sesync::team_watch::TeamWatch::keep_newer_lists(&team_config_dir(), &pulls) {
                        match kept {
                            Ok(v) => eprintln!("sesync: team [{space}]: member list v{v} kept"),
                            Err(e) => eprintln!("sesync: team [{space}]: cannot keep the newer member list: {e}"),
                        }
                    }
                    for p in &pulls {
                        match &p.out.error {
                            Some(e) => eprintln!("sesync: team pull [{}]: {e}", p.space),
                            None => eprintln!("sesync: team pull [{}]: {} shown, {} dropped, {} rolled back",
                                p.space, p.out.shown.len(), p.out.dropped, p.out.rolled_back),
                        }
                        let visible = p.out.shown.len() + p.out.unchanged;
                        // The pull fetches every record each time, so only a sequence above the
                        // floor already held is new; a record read again unchanged is not activity.
                        let new_records = p.out.accepted.iter()
                            .filter(|(conv, seq)| **seq > state.seen_floor(&p.space, conv))
                            .count();
                        activity |= !sesync::team_backoff::pull_is_idle(
                            new_records, visible, visible_before.get(&p.space).copied());
                        visible_before.insert(p.space.clone(), visible);
                        // The floor only rises, and it is per team: a record received under one
                        // team never raises another team's floor for the same conversation.
                        for (conv, seq) in &p.out.accepted {
                            state.raise_seen(&p.space, conv, *seq);
                        }
                    }
                    if state.team_seen != before {
                        if let Err(e) = state.absorb_user_edits(&env.state_file()) {
                            eprintln!("sesync: cannot read user choices from state: {e}");
                        }
                        release_records_logged(&env.locations, &mut state);
                        if let Err(e) = state.save(&env.state_file()) {
                            eprintln!("sesync: cannot save state: {e}");
                        }
                    }
                }
                // Team push: marked sessions quiet for the team period, sealed and pushed. Nothing runs
                // unless a team is configured. Each session is sent as a canonical journal, made by its
                // own harness.
                let now = now_secs();
                let scratch = env.state_dir.join("team-scratch");
                let paths: std::collections::HashMap<String, (String, f64)> = state.shared_sessions().into_iter()
                    .filter_map(|id| {
                        // The marked id is the conversation's stable id; the entry holding it
                        // can move between keys when a local turn promotes a copy.
                        let key = state.key_of_team_id(&id)?;
                        let fmt = state.sources[&key].fmt.clone();
                        let changed = state.sources[&key].last_seen_change;
                        Some((id, (format!("{fmt}\u{1f}{key}"), changed)))
                    })
                    .collect();
                let content_of = |id: &str| -> Result<Vec<u8>, String> {
                    let (tagged, _) = paths.get(id).ok_or("session is not on disk")?;
                    let (fmt, key) = tagged.split_once('\u{1f}').ok_or("bad source record")?;
                    sesync::team_sync::canonical_for_source(fmt, key, &env.locations, &env.tools(), &scratch)
                };
                let last_change = |id: &str| paths.get(id).map(|(_, t)| *t);
                match sesync::team_pass::run(&team_config_dir(), &mut state, now, content_of, last_change) {
                    Ok(passes) => {
                        for pass in &passes {
                            eprintln!("sesync: {}", sesync::team_pass::summary(pass));
                            activity |= !pass.pushed.is_empty();
                        }
                    }
                    Err(sesync::team_pass::NotRun::NoTeam) => {}
                    Err(e) => eprintln!("sesync: team push not run: {e:?}"),
                }
                // Only a configured team, or a join waiting for the admin, backs off. Without one the
                // team step returns at once, so it keeps running on every tick, as it always has.
                if !teams.is_empty() || waiting {
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

/// What a pull worker sends back: one verified outcome per team, and one line per join still
/// waiting or just approved. The teams to share with are read from disk when it is applied.
struct PullDone {
    pulls: Vec<sesync::team_watch::TeamPull>,
    notes: Vec<String>,
}

/// The pull in flight, collected on every loop turn.
enum TeamPull {
    /// The request was refused before starting (not configured, or one already running).
    None,
    /// A worker is pulling.
    Running(std::sync::mpsc::Receiver<PullDone>),
}

/// Put the result on the app: the verified sessions (one entry per conversation, the newest
/// across the teams), the refuse count and each team the server would not list, then any waiting
/// or approved join, which the status line shows instead of the pull count. A team left while
/// the pull ran is dropped from it, so it is neither shown nor offered again. Runs on the main
/// thread after the worker sent it.
fn apply_pull(app: &mut sesync::app::App, done: PullDone, config_dir: &std::path::Path) {
    let current: Vec<String> = sesync::team_watch::teams_from_env(config_dir).into_iter().map(|t| t.space).collect();
    let pulls: Vec<_> = done.pulls.into_iter().filter(|p| current.contains(&p.space)).collect();
    let copies = sesync::team_watch::TeamWatch::copies(&pulls);
    let refused: usize = pulls.iter().map(|p| p.out.dropped).sum();
    let floors: Vec<(String, String, u64)> = pulls.iter()
        .flat_map(|p| p.out.accepted.iter().map(|(c, s)| (p.space.clone(), c.clone(), *s)))
        .collect();
    let problems: std::collections::BTreeMap<String, String> = pulls.iter()
        .filter_map(|p| p.out.error.clone().map(|e| (p.space.clone(), e)))
        .collect();
    app.set_pull_result(copies, refused);
    app.absorb_pull_floors(&floors);
    app.set_teams(current);
    // The worker asked whether a waiting join was approved and stored the list when it was.
    app.set_team_cards(sesync::team_join::team_cards(config_dir));
    let mut notes = done.notes;
    // A team the server refused to list reads as empty otherwise; its card says it too.
    notes.extend(problems.iter().map(|(space, e)| format!("team [{space}]: {e}")));
    let failed = !problems.is_empty();
    app.set_team_problems(problems);
    if failed {
        app.set_error(&notes.join("; "));
    } else if !notes.is_empty() {
        app.set_status(&notes.join("; "));
    }
}

/// Apply the pull in flight once its worker has answered (or died); otherwise leave it running.
fn collect_pull(app: &mut sesync::app::App, pending: &mut TeamPull, config_dir: &std::path::Path) {
    let TeamPull::Running(rx) = &*pending else { return };
    match rx.try_recv() {
        Ok(out) => {
            apply_pull(app, out, config_dir);
            *pending = TeamPull::None;
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            app.pull_failed();
            *pending = TeamPull::None;
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
    }
}

/// Start a `TeamPull::Running` worker, or explain why not. The worker first asks whether any
/// waiting join was approved (storing the list when it was), renews any token about to run out,
/// and reads the credentials again before pulling, so a long-open TUI keeps working.
fn request_team_pull(app: &mut sesync::app::App, config_dir: &std::path::Path) -> TeamPull {
    if sesync::team_watch::teams_from_env(config_dir).is_empty() && sesync::team_join::waiting_teams(config_dir).is_empty() {
        app.set_error(
            "team server not configured: join a team (or set SESYNC_TEAM_URL for an older credential), or set SESYNC_TEAM_URL, SESYNC_TEAM_TOKEN, SESYNC_TEAM_MACHINE and SESYNC_TEAM_SPACE",
        );
        return TeamPull::None;
    }
    if !app.begin_pull() {
        return TeamPull::None;
    }
    // The floors the daemon has persisted, per team; the app only reads them. The app replaces
    // its list with what this pull shows, so every conversation is fetched, not only the ones
    // that changed since the daemon's last pull.
    let floors = app.state().floors_all();
    let config_dir = config_dir.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut notes: Vec<String> = sesync::team_join::check_all_waiting(&config_dir).into_iter()
            .map(|(space, r)| match r {
                Ok(a) => a.describe(&space),
                Err(e) => format!("team [{space}]: cannot check the join request: {e}"),
            })
            .collect();
        let _ = sesync::team_join::renew_all_due(&config_dir, now_secs() as u64);
        let teams = sesync::team_watch::teams_from_env(&config_dir);
        let pulls = sesync::team_watch::TeamWatch::pull_each(&teams, &floors);
        // A newer member list is kept here too, so the Team card shows the version in force.
        for (space, kept) in sesync::team_watch::TeamWatch::keep_newer_lists(&config_dir, &pulls) {
            if let Err(e) = kept {
                notes.push(format!("team [{space}]: cannot keep the newer member list: {e}"));
            }
        }
        let _ = tx.send(PullDone { pulls, notes });
    });
    TeamPull::Running(rx)
}

/// A dialog's network step or an admin job, run on a worker thread so the terminal keeps drawing.
/// `run_tui` ticks these on every loop turn, so a result shows without a keypress.
type SetupRx = std::sync::mpsc::Receiver<sesync::team_actions::SetupDone>;
type AdminRx = std::sync::mpsc::Receiver<Result<sesync::team_actions::AdminDone, String>>;

#[derive(Default)]
struct TeamJobs {
    setup: Option<SetupRx>,
    admin: Option<AdminRx>,
}

impl TeamJobs {
    /// Start the job the app asked for, and collect a finished one.
    fn tick(&mut self, app: &mut sesync::app::App, config_dir: &std::path::Path) {
        if let Some(req) = app.take_setup_request() {
            let (tx, rx) = std::sync::mpsc::channel();
            let dir = config_dir.to_path_buf();
            std::thread::spawn(move || {
                let _ = tx.send(sesync::team_actions::setup(&req, &dir));
            });
            self.setup = Some(rx);
        }
        if let Some(job) = app.take_admin_request() {
            let (tx, rx) = std::sync::mpsc::channel();
            let dir = config_dir.to_path_buf();
            std::thread::spawn(move || {
                let _ = tx.send(sesync::team_actions::admin(&job, &dir));
            });
            self.admin = Some(rx);
        }
        if let Some(outcome) = self.setup.as_ref().map(|rx| rx.try_recv()) {
            match outcome {
                Ok(done) => {
                    self.setup = None;
                    apply_setup(app, done, config_dir);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.setup = None;
                    app.finish_setup(sesync::team_actions::SetupDone {
                        result: Err("the team step stopped unexpectedly".into()),
                        created: None,
                        joined: false,
                    });
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(outcome) = self.admin.as_ref().map(|rx| rx.try_recv()) {
            match outcome {
                Ok(result) => {
                    self.admin = None;
                    app.finish_admin(result);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.admin = None;
                    app.finish_admin(Err("the admin step stopped unexpectedly".into()));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
    }
}

/// Report a finished dialog, then read the team records again: a join adds a team to share into
/// (a waiting join is listed for its refresh but not offered for sharing yet), and the admin flag
/// follows the key files, which a create may have just written.
fn apply_setup(app: &mut sesync::app::App, done: sesync::team_actions::SetupDone, config_dir: &std::path::Path) {
    app.finish_setup(done);
    reload_team_records(app, config_dir);
}

/// Read this machine's team records again (memberships, waiting joins, teams managed here)
/// after a dialog or a leave changed them.
fn reload_team_records(app: &mut sesync::app::App, config_dir: &std::path::Path) {
    let teams = sesync::team_watch::teams_from_env(config_dir);
    let waiting = sesync::team_join::waiting_teams(config_dir);
    app.set_team_configured(!teams.is_empty() || !waiting.is_empty());
    app.set_teams(teams.into_iter().map(|t| t.space).collect());
    app.set_admin(sesync::team_cli::has_admin_key(config_dir));
    app.set_admin_teams(sesync::team_cli::administered(config_dir));
    app.set_team_cards(sesync::team_join::team_cards(config_dir));
}


/// (mtime, size) of the state file. The TUI reloads the daemon's fields and re-lists the
/// sessions only when this changes, so an idle keystroke and an idle second cost a stat.
fn state_stamp(path: &Path) -> Option<(u128, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos(), m.len()))
}

/// The modification time of the state file, in seconds since the epoch, for the tab bar.
fn file_mtime_secs(path: &Path) -> Option<f64> {
    let m = std::fs::metadata(path).ok()?;
    Some(m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs_f64())
}

/// Mouse on the sessions list: the wheel moves the selection; a left click selects the row, and a
/// second click on the same row within half a second opens its session menu (the Enter key).
/// Clicks while a dialog is open do nothing.
fn handle_mouse(
    app: &mut sesync::app::App,
    m: crossterm::event::MouseEvent,
    screen: ratatui::layout::Rect,
    last_click: &mut Option<(usize, std::time::Instant)>,
    now: std::time::Instant,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    use sesync::app::{Ev, Mode};
    if !matches!(app.mode(), Mode::Browse) {
        return;
    }
    match m.kind {
        // The wheel over the details pane scrolls the preview; anywhere else it moves the list.
        MouseEventKind::ScrollUp if sesync::ui::details_pane_at(app, screen, m.column, m.row) => app.handle(Ev::PreviewUp, now),
        MouseEventKind::ScrollDown if sesync::ui::details_pane_at(app, screen, m.column, m.row) => app.handle(Ev::PreviewDown, now),
        MouseEventKind::ScrollUp => app.handle(Ev::Up, now),
        MouseEventKind::ScrollDown => app.handle(Ev::Down, now),
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(idx) = sesync::ui::session_row_at(app, screen, m.column, m.row) else { return };
            let double = matches!(last_click, Some((i, at)) if *i == idx && now.duration_since(*at) < std::time::Duration::from_millis(500));
            app.select_row(idx);
            *last_click = if double { None } else { Some((idx, now)) };
            if double {
                app.handle(Ev::Enter, now);
            }
        }
        _ => {}
    }
}

fn run_tui(
    roots: sesync::manage::Roots,
    state: State,
    lock_dir: PathBuf,
    sesync_lock: PathBuf,
    config_dir: PathBuf,
    state_file: PathBuf,
) -> std::io::Result<()> {
    use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
    use crossterm::ExecutableCommand;
    use sesync::app::App;
    use ratatui::{backend::CrosstermBackend, Terminal};
    use std::time::{Duration, Instant};

    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ = std::io::stdout().execute(DisableMouseCapture);
            let _ = std::io::stdout().execute(LeaveAlternateScreen);
        }
    }

    enable_raw_mode()?;
    let _restore = Restore;
    std::io::stdout().execute(EnterAlternateScreen)?;
    std::io::stdout().execute(EnableMouseCapture)?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    // Optional startup/event trace for CI debugging: SESYNC_TRACE names a file this process
    // appends to. Off in normal use; the pty test prints it when a wait fails.
    let mut trace: Option<std::fs::File> = std::env::var_os("SESYNC_TRACE").map(|p| {
        std::fs::OpenOptions::new().create(true).append(true).open(p).ok()
    }).flatten();
    let mut tr = |msg: &str, trace: &mut Option<std::fs::File>| {
        use std::io::Write;
        if let Some(f) = trace.as_mut() {
            let _ = writeln!(f, "{} {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0), msg);
        }
    };
    tr("tui start", &mut trace);
    // Only harnesses installed on this machine are offered as mirror targets; each harness says
    // what installed means for it (its folder exists, or its command can be run).
    let installed: Vec<String> = harness::ALL.iter()
        .filter(|h| h.present(roots.locations.of(h.id())))
        .map(|h| h.id().to_string())
        .collect();
    // The daemon's file is merged in while the TUI runs; retired harnesses are released from it
    // with the same locations the TUI was started with.
    let locations = roots.locations.clone();
    // The details pane previews the highlighted session; a worker reads its last turns, so the
    // list never waits on a large transcript or a conversion.
    let preview = sesync::preview::Worker::start(roots.locations.clone(), roots.home.clone(), roots.sesync_conv.clone());
    // The Team tab and "Refresh team" exist before the first pull; without configuration they
    // say what is missing instead of failing silently. A machine can belong to several teams.
    let teams: Vec<String> = sesync::team_watch::teams_from_env(&config_dir).iter()
        .map(|t| t.space.clone()).collect();
    let mut app = App::new(roots, state, lock_dir, sesync_lock);
    app.migrate_legacy_team_state(&teams);
    app.set_installed_targets(installed);
    // A join still waiting for the admin is configured too: ctrl+r asks whether it was approved.
    app.set_team_configured(!teams.is_empty() || !sesync::team_join::waiting_teams(&config_dir).is_empty());
    let have_teams = !teams.is_empty();
    app.set_teams(teams);
    app.set_team_cards(sesync::team_join::team_cards(&config_dir));
    // The team server and the admin token file are the dialogs' defaults; the admin flag says
    // whether this machine can run the admin actions at all.
    app.set_team_defaults(&std::env::var("SESYNC_TEAM_URL").unwrap_or_default(),
        &std::env::var("SESYNC_ADMIN_TOKEN_FILE").unwrap_or_default());
    app.set_admin(sesync::team_cli::has_admin_key(&config_dir));
    app.set_admin_teams(sesync::team_cli::administered(&config_dir));
    // crossterm drops every colour when NO_COLOR is set, so the QR cannot be black on white.
    app.set_monochrome(std::env::var_os("NO_COLOR").map_or(false, |v| !v.is_empty()));
    // Restore last-run taste (theme, tab) from this user's configuration directory.
    let prefs = sesync::app::prefs::load(&config_dir);
    app.restore_prefs(prefs.theme, prefs.view);

    // The state file is read back only when it changed since the last look: the daemon
    // rewrites it on every pass, and a keystroke must not pay for a reload that shows
    // nothing new.
    let mut seen_state = state_stamp(&state_file);
    app.set_state_written(file_mtime_secs(&state_file));
    let mut last_click: Option<(usize, Instant)> = None;
    let mut last_merge = Instant::now();
    let mut last_team_check = Instant::now();
    // A team this machine already belongs to is pulled once at start, so the Team tab shows
    // what the team holds instead of an empty list that waits for ctrl+r.
    let mut pending_pull = if have_teams { request_team_pull(&mut app, &config_dir) } else { TeamPull::None };
    let mut jobs = TeamJobs::default();
    loop {
        jobs.tick(&mut app, &config_dir);
        // A pull that finished while the terminal sat idle is applied now, not at the next key.
        collect_pull(&mut app, &mut pending_pull, &config_dir);
        // A newly highlighted session goes to the worker; whatever it has read since is shown.
        if let Some((fmt, key)) = app.preview_wanted() {
            if app.preview().key != key {
                preview.select(&fmt, &key);
                app.preview_requested(&key);
            }
        }
        if let Some(loaded) = preview.take() {
            app.apply_preview(loaded);
        }
        term.draw(|f| sesync::ui::draw(f, &app))?;
        if !event::poll(Duration::from_millis(200))? {
            tr("poll timeout", &mut trace);
            // A running daemon writes mirrors and floors into the same file; take them in
            // about once a second so the table shows them without a keypress.
            if last_merge.elapsed() >= Duration::from_secs(1) {
                last_merge = Instant::now();
                let stamp = state_stamp(&state_file);
                if stamp != seen_state {
                    seen_state = stamp;
                    if let Ok(mut disk) = State::load(&state_file) {
                        let _ = release_records(&locations, &mut disk);
                        app.absorb_daemon_state(&disk);
                        app.set_state_written(file_mtime_secs(&state_file));
                        app.refresh_rows();
                        tr("absorbed daemon state", &mut trace);
                    }
                }
                // The daemon may have seen the admin approve a waiting join; the cards are a
                // few small local files, read only while the Team tab is on screen.
                if app.view() == sesync::app::View::Team {
                    app.set_team_cards(sesync::team_join::team_cards(&config_dir));
                }
                // A join approved (or a team left) while this screen is open must reach the
                // share picker and the Team tab without a restart: re-read the teams, and pull
                // once when the set changed, so the tab shows what the team holds.
                if last_team_check.elapsed() >= Duration::from_secs(5) {
                    last_team_check = Instant::now();
                    let before = app.teams().to_vec();
                    reload_team_records(&mut app, &config_dir);
                    if app.teams() != before.as_slice() {
                        let names: Vec<String> = app.teams().to_vec();
                        app.set_status(&format!("teams: {}",
                            if names.is_empty() { "none".to_string() } else { names.join(", ") }));
                        if !names.is_empty() {
                            if let started @ TeamPull::Running(_) = request_team_pull(&mut app, &config_dir) {
                                pending_pull = started;
                            }
                        }
                        tr("teams re-resolved", &mut trace);
                    }
                }
            }
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Mouse(m) => {
                let size = term.size()?;
                let screen = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                handle_mouse(&mut app, m, screen, &mut last_click, Instant::now());
                continue;
            }
            _ => {
                tr("poll non-key event", &mut trace);
                continue;
            }
        };
        tr(&format!("key {} {:?}", key.code, key.modifiers), &mut trace);
        if key.kind != crossterm::event::KeyEventKind::Press {
            continue;
        }
        // Keys that do nothing in the app do nothing to the terminal either.
        let Some(ev) = sesync::keys::key_to_ev(key.code, key.modifiers) else { continue };
        // Every key goes through the app, so every key cancels a pending delete the same way.
        // Ctrl+R on the Team tab asks for a network refresh; the local list refreshes in the app.
        app.handle(ev, Instant::now());
        // A team the user chose to leave goes now (a few local files), and the team records are
        // read again, so neither the share picker nor the Team tab offers it any more.
        if let Some(req) = app.take_leave_request() {
            let outcome = sesync::team_actions::leave(&req, &config_dir);
            app.left_team(&req, outcome);
            reload_team_records(&mut app, &config_dir);
        }
        // A pull the app asked for (ctrl+r on the team tab). A request refused because one is
        // still running must not drop that one: its result would never arrive, and every later
        // ctrl+r would be refused for the rest of the run.
        if app.take_refresh_request() {
            if let started @ TeamPull::Running(_) = request_team_pull(&mut app, &config_dir) {
                pending_pull = started;
            }
        }
        collect_pull(&mut app, &mut pending_pull, &config_dir);
        // The app owns choices (mirror targets, marks) that must survive the process. Take
        // the daemon's fields when it wrote them, then save only when this key changed a
        // user-owned field: a save from the app must never roll back a mirror record or a
        // team floor the daemon just wrote, and an unrelated key costs no disk I/O.
        let stamp = state_stamp(&state_file);
        if stamp != seen_state {
            if let Ok(mut disk) = State::load(&state_file) {
                let _ = release_records(&locations, &mut disk);
                app.absorb_daemon_state(&disk);
                app.set_state_written(file_mtime_secs(&state_file));
            }
            seen_state = stamp;
        }
        if app.take_user_dirty() {
            if let Err(e) = app.save_user(&state_file) {
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
            tr("quit", &mut trace);
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
        Ok(j) if j.pending => {
            println!("asked to join team '{name}' as '{member}' on '{machine}'. Credential saved to {}.", j.path.display());
            println!("The admin must approve this machine before it syncs. Give them this fingerprint, out of band:");
            println!("  {}", j.fingerprint);
            println!("The daemon starts syncing on its own once the admin has approved it.");
            0
        }
        Ok(j) => { println!("joined team '{name}' as '{member}'. Credential saved to {}", j.path.display()); 0 }
        Err(msg) => { eprintln!("sesync team join: {msg}"); 1 }
    }
}

/// `sesync team renew <name>`: renews the stored member token while its enrolment is within
/// the 7-day window. After the window the code is needed again, via `sesync team join`. The
/// request goes to the server the team was joined on; SESYNC_TEAM_URL serves older credentials.
fn run_renew(args: &[String]) -> i32 {
    use sesync::team_join::renew;
    let name = match args.first() {
        Some(n) => n.clone(),
        None => { eprintln!("usage: sesync team renew <name>"); return 2; }
    };
    let fallback = std::env::var("SESYNC_TEAM_URL").ok();
    let config = team_config_dir();
    match renew(fallback.as_deref(), &config, &name) {
        Ok(()) => { println!("renewed team '{name}'."); 0 }
        Err(msg) => { eprintln!("sesync team renew: {msg}"); 1 }
    }
}