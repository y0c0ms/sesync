//! Write a mirror of one session into one chosen harness. Only the daemon calls this, and only for
//! targets the user picked for that session (`Source.chosen_targets`). Nothing is written to a
//! harness the user did not choose.
//!
//! The file-backed targets (omp, Pi, Claude) get a converted journal installed by `sesync-conv`.
//! The O-penCode target is installed by bundle import and is handled by the caller.
//! Every write claims the conversation with `sesync-lock` first, so a live turn cannot start
//! while its mirror is being written.

use crate::install::{install_journal, InstallTarget};
use crate::lock::{with_claim, ClaimOutcome};
use crate::mirror::{file_hash, mirror_uuid};
use crate::state::{Mirror, State};
use std::path::{Path, PathBuf};

/// Where each harness keeps its store, and the tools needed to write into it.
pub struct Stores<'a> {
    pub omp: &'a Path,
    pub pi: &'a Path,
    pub claude: &'a Path,
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
    let target_store: &Path = match target {
        "omp" => stores.omp,
        "pi" => stores.pi,
        "claude" => stores.claude,
        other => return Err(format!("no file mirror writer for '{other}'")),
    };
    let mirror_id = mirror_uuid(source_key, target);
    let src_path = PathBuf::from(source_key);
    let install = match target {
        "pi" => InstallTarget::Pi { agent_dir: target_store.to_path_buf() },
        "omp" => InstallTarget::Omp {
            agent_dir: target_store.to_path_buf(),
            home: target_store.parent().unwrap_or(target_store).to_path_buf(),
        },
        _ => InstallTarget::Claude { claude_dir: target_store.to_path_buf() },
    };
    let outcome = with_claim(stores.sesync_lock, None, conv, "daemon", std::process::id(), source_key, || {
        install_journal(stores.sesync_conv, &src_path, install, "/", &mirror_id, title)
    });
    let installed = match outcome {
        ClaimOutcome::Skipped(holder) => return Err(format!("held by {holder}; will retry")),
        ClaimOutcome::Ran(r) => r?,
    };
    record_mirror(state, source_key, target, &installed)?;
    Ok(installed)
}

/// Record a written mirror on its source. The source must already be in state.
pub fn record_mirror(state: &mut State, source_key: &str, target: &str, installed: &Path) -> Result<(), String> {
    let src = state.sources.get_mut(source_key).ok_or("unknown source")?;
    let path = installed.display().to_string();
    state.mirror_keys.insert(path.clone());
    src.mirrors.insert(target.to_string(), Mirror {
        target: target.to_string(),
        path,
        source_hash: src.last_hash.clone(),
        installed_hash: file_hash(installed).unwrap_or_default(),
        diverged: false,
    });
    Ok(())
}
