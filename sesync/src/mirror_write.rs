//! Write a mirror of one session into one chosen harness. Only the daemon calls this, and only for
//! targets the user picked for that session (`Source.chosen_targets`). Nothing is written to a
//! harness the user did not choose.
//!
//! The file-backed targets (omp, Pi, Claude, Gemini) get a converted journal installed by
//! `sesync-conv`. The O-penCode target converts the same journal to an import bundle and
//! loads it with `opencode import`, delete-first so a refresh replaces the session. Every
//! write claims the conversation with `sesync-lock` first, so a live turn cannot start
//! while its mirror is being written.

use crate::install::{install_gemini, install_journal, install_opencode, InstallTarget};
use crate::lock::{with_claim, ClaimOutcome};
use crate::mirror::{file_hash, mirror_opencode_id, mirror_uuid};
use crate::state::{Mirror, State};
use std::path::{Path, PathBuf};

/// Where each harness keeps its store, and the tools needed to write into it.
pub struct Stores<'a> {
    /// The user's home. omp buckets are home-relative, so the converter needs the real
    /// home, never the store's parent (`~/.omp` would put every mirror in `--home-...-`).
    pub home: &'a Path,
    pub omp: &'a Path,
    pub pi: &'a Path,
    pub claude: &'a Path,
    pub gemini: &'a Path,
    pub opencode_bin: &'a Path,
    pub opencode_db: &'a Path,
    pub sesync_conv: &'a Path,
    pub sesync_lock: &'a Path,
}

/// Write (or refresh) one mirror of `source_key` into `target`, record it in state, and return the
/// installed path. `conv` is the conversation id the lock is claimed under.
pub fn write_mirror(
    stores: &Stores,
    state: &mut State,
    source_key: &str,
    source_fmt: &str,
    conv: &str,
    title: &str,
    target: &str,
) -> Result<PathBuf, String> {
    if target == source_fmt {
        return Err(format!("{target} is the source's own format; nothing to mirror"));
    }
    // The O-penCode and Gemini writers take a journal, and this entry point only has the
    // source file itself. omp/Pi sources are already journals; Claude and Gemini sources
    // are converted first and arrive through `write_mirror_from_journal` instead.
    if (target == "opencode" || target == "gemini") && source_fmt != "omp" && source_fmt != "pi" {
        return Err(format!("a {source_fmt} source must be converted to a journal before writing a {target} mirror"));
    }
    install_into(stores, state, source_key, conv, title, target, Path::new(source_key))
}

/// As `write_mirror`, but the content is already a converted omp journal: an O-penCode source
/// is exported and converted once, a Claude or Gemini source is converted once, then the same
/// journal is installed into every chosen target. The lock and the state record still belong to
/// the source's own key.
pub fn write_mirror_from_journal(
    stores: &Stores,
    state: &mut State,
    source_key: &str,
    conv: &str,
    title: &str,
    target: &str,
    journal: &Path,
) -> Result<PathBuf, String> {
    install_into(stores, state, source_key, conv, title, target, journal)
}

fn install_into(
    stores: &Stores,
    state: &mut State,
    source_key: &str,
    conv: &str,
    title: &str,
    target: &str,
    src: &Path,
) -> Result<PathBuf, String> {
    // The mirror id is derived from the source key, so a turn-taking promotion (a new source
    // key) would mint a new id and leave the old copy behind. When a mirror is already
    // recorded for this target, keep its id: the same copy is replaced in place, wherever
    // the source currently lives. OpenCode mirrors are store conversations, not files, so
    // their id must not change at all.
    // Mirror ids are minted from the conversation's stable id, not from the moving source
    // key: a promotion changes the source key, and a key-derived id would mint a new copy in
    // another store and leave the old one behind. When a mirror is already recorded for this
    // target, keep its id: the same copy is replaced in place, wherever the source lives.
    let stable = state.sources.get(source_key)
        .map(|s| s.conv.clone())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| source_key.to_string());
    let mirror_id = if target == "opencode" {
        state.sources.get(source_key)
            .and_then(|s| s.mirrors.get(target))
            .map(|m| m.path.clone())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| mirror_opencode_id(&stable))
    } else {
        mirror_uuid(&stable, target)
    };
    if target == "opencode" {
        let outcome = with_claim(stores.sesync_lock, None, conv, "daemon", std::process::id(), source_key, || {
            install_opencode(stores.opencode_bin, stores.sesync_conv, src, "", &mirror_id, title, None)
        });
        let sid = match outcome {
            ClaimOutcome::Skipped(holder) => return Err(format!("held by {holder}; will retry")),
            ClaimOutcome::Ran(r) => r?,
        };
        record_opencode_mirror(state, source_key, &sid)?;
        // Record when the store last saw this conversation, so a later `updated` means the
        // user answered in the copy and turn-taking can promote it back.
        let updated = crate::opencode::row_updated(stores.opencode_db, &sid).unwrap_or(0.0);
        if let Some(src) = state.sources.get_mut(source_key) {
            if let Some(m) = src.mirrors.get_mut("opencode") {
                m.target_updated = updated;
            }
        }
        return Ok(PathBuf::from(sid));
    }
    if target == "gemini" {
        let outcome = with_claim(stores.sesync_lock, None, conv, "daemon", std::process::id(), source_key, || {
            install_gemini(stores.sesync_conv, src, stores.gemini, &mirror_id)
        });
        let installed = match outcome {
            ClaimOutcome::Skipped(holder) => return Err(format!("held by {holder}; will retry")),
            ClaimOutcome::Ran(r) => r?,
        };
        // The mirror is a file inside the Gemini store, and `discovery::gemini_sessions`
        // walks that store: recording the path in `mirror_keys` is what stops the scan
        // from treating the copy as a source.
        record_mirror(state, source_key, "gemini", &installed)?;
        return Ok(installed);
    }
    let target_store: &Path = match target {
        "omp" => stores.omp,
        "pi" => stores.pi,
        "claude" => stores.claude,
        other => return Err(format!("no file mirror writer for '{other}'")),
    };
    let install = match target {
        "pi" => InstallTarget::Pi { agent_dir: target_store.to_path_buf() },
        "omp" => InstallTarget::Omp {
            agent_dir: target_store.to_path_buf(),
            home: stores.home.to_path_buf(),
        },
        _ => InstallTarget::Claude { claude_dir: target_store.to_path_buf() },
    };
    let outcome = with_claim(stores.sesync_lock, None, conv, "daemon", std::process::id(), source_key, || {
        install_journal(stores.sesync_conv, src, install, "/", &mirror_id, title)
    });
    let installed = match outcome {
        ClaimOutcome::Skipped(holder) => return Err(format!("held by {holder}; will retry")),
        ClaimOutcome::Ran(r) => r?,
    };
    record_mirror(state, source_key, target, &installed)?;
    Ok(installed)
}

/// Record a written mirror on its source. The source must already be in state. When this
/// replaces a mirror at a different path (a rewrite into the right bucket, a new filename),
/// the old file is removed first — but only when it is still exactly what sesync wrote, so
/// nothing a user may have since edited is ever deleted. Anything kept stays in
/// `mirror_keys`, which is what stops the scan from treating it as a source.
pub fn record_mirror(state: &mut State, source_key: &str, target: &str, installed: &Path) -> Result<(), String> {
    let src = state.sources.get_mut(source_key).ok_or("unknown source")?;
    let path = installed.display().to_string();
    let replaced = src.mirrors.get(target).map(|m| (m.path.clone(), m.installed_hash.clone()));
    if let Some((old_path, old_hash)) = replaced {
        if old_path != path {
            let p = Path::new(&old_path);
            let ours_unchanged = old_path.starts_with('/')
                && !old_hash.is_empty()
                && file_hash(p).map(|h| h == old_hash).unwrap_or(false);
            if ours_unchanged {
                let _ = std::fs::remove_file(p);
                state.mirror_keys.remove(&old_path);
            }
        }
    }
    state.mirror_keys.insert(path.clone());
    src.mirrors.insert(target.to_string(), Mirror {
        target: target.to_string(),
        path,
        source_hash: src.last_hash.clone(),
        installed_hash: file_hash(installed).unwrap_or_default(),
        diverged: false,
        edit_stamp: String::new(),
        target_updated: 0.0,
    });
    Ok(())
}

/// Record a session imported into O-penCode. There is no file: the copy lives in OpenCode's
/// own store, and the mirror's path carries its session id so the UI can show it. The id is
/// not added to `mirror_keys`: that set names files sesync wrote into harness stores, and an
/// id there could hide a real OpenCode session from the scan.
pub fn record_opencode_mirror(state: &mut State, source_key: &str, sid: &str) -> Result<(), String> {
    let src = state.sources.get_mut(source_key).ok_or("unknown source")?;
    let mirror = Mirror {
        target: "opencode".to_string(),
        path: sid.to_string(),
        source_hash: src.last_hash.clone(),
        installed_hash: String::new(),
        diverged: false,
        edit_stamp: String::new(),
        target_updated: 0.0,
    };
    src.mirrors.insert("opencode".to_string(), mirror);
    Ok(())
}
