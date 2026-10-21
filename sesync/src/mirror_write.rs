//! Write a mirror of one session into one chosen harness. Only the daemon calls this, and only for
//! targets the user picked for that session (`Source.chosen_targets`). Nothing is written to a
//! harness the user did not choose.
//!
//! The source arrives as a canonical journal (a harness whose files are journals hands its file
//! as it is; every other source is converted first), and the target's harness installs it
//! (`Harness::install`). Every write claims the conversation with `sesync-lock` first, so a live
//! turn cannot start while its mirror is being written.
//!
//! Bytes sesync did not write itself are never replaced without a verified archive copy: the old
//! source a promotion demoted (the user's original conversation), a copy changed in its harness
//! without an answer, a record left by an older binary. If the copy cannot be kept, the write
//! is refused and retried later.

use crate::archive_store::keep_before_replace;
use crate::harness::{self, Harness, Kind, Locations, Tools};
use crate::lock::{with_claim, ClaimOutcome};
use crate::mirror::{combine_hashes, copy_files, copy_hash, file_hash_bytes};
use crate::state::{Mirror, State};
use std::path::Path;

/// Where each harness keeps its store, and the tools needed to write into it.
pub struct Stores<'a> {
    pub locations: &'a Locations,
    /// The user's home. omp buckets are home-relative, so the converter needs the real home,
    /// never the store's parent.
    pub home: &'a Path,
    pub sesync_conv: &'a Path,
    pub sesync_lock: &'a Path,
    /// Where bytes sesync is about to replace are kept first (`<state dir>/archive`).
    pub archive: &'a Path,
    /// The claims' directory. The daemon claims there and writes the mirror-id aliases the lock
    /// binary follows, so a turn inside any copy and a write of that copy share one lock.
    pub lock_dir: &'a Path,
}

impl Stores<'_> {
    fn tools(&self) -> Tools<'_> {
        Tools { home: self.home, sesync_conv: self.sesync_conv }
    }
}

/// Write (or refresh) one mirror of `source_key` into `target` from `journal`, record it in state,
/// and return the copy's key: its path, or its session id in a store harness. `journal` is the
/// source itself when its harness keeps canonical journals, else the source converted. `conv` is
/// the conversation id the lock is claimed under.
pub fn write_mirror(
    stores: &Stores,
    state: &mut State,
    source_key: &str,
    conv: &str,
    title: &str,
    target: &str,
    journal: &Path,
) -> Result<String, String> {
    let source_fmt = state.sources.get(source_key).map(|s| s.fmt.clone()).unwrap_or_default();
    if target == source_fmt {
        return Err(format!("{target} is the source's own format; nothing to mirror"));
    }
    let h = harness::get(target).ok_or_else(|| format!("no harness named '{target}' to write a mirror into"))?;
    let native = harness::get(&source_fmt).is_some_and(|s| s.native_journal());
    if journal == Path::new(source_key) && !native {
        return Err(format!("a {source_fmt} source must be converted to a journal before writing a {target} mirror"));
    }
    install_into(stores, state, source_key, conv, title, h, journal)
}

/// A conversation id as an archive folder name.
fn archive_parent(conv: &str) -> String {
    let s: String = conv
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(120)
        .collect();
    if s.is_empty() { "unknown".into() } else { s }
}

/// Keep a verified copy of the recorded copy's bytes before this write replaces them, unless they
/// are provably what sesync itself last wrote (`installed_hash`). Once kept, the record takes the
/// current fingerprint, so the replace-in-place and moved-path rules treat the bytes as sesync's.
/// A copy that cannot be kept stops the write: nothing is replaced that cannot be restored.
fn preserve_foreign(stores: &Stores, state: &mut State, source_key: &str, h: &dyn Harness, conv: &str) -> Result<(), String> {
    let target = h.id();
    let Some(m) = state.sources.get(source_key).and_then(|s| s.mirrors.get(target)).cloned() else {
        return Ok(());
    };
    let parent = archive_parent(conv);
    let current = match h.kind() {
        Kind::Store => {
            let loc = stores.locations.of(target);
            let Some(now) = h.fingerprint(loc, &m.path) else {
                return Ok(()); // not in the store: nothing to lose
            };
            if !m.installed_hash.is_empty() && now == m.installed_hash {
                return Ok(());
            }
            let scratch = std::env::temp_dir().join(format!("sesync-keep-{}-{}", std::process::id(), m.path));
            let _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
            let out = scratch.join("export.json");
            let kept = h.export(loc, &m.path, &out)
                .and_then(|()| std::fs::read(&out).map_err(|e| e.to_string()))
                .and_then(|bytes| keep_before_replace(stores.archive, &parent, &format!("{}.export.json", m.path), &bytes));
            let _ = std::fs::remove_dir_all(&scratch);
            let kept = kept.map_err(|e| format!("cannot keep a copy of {target} session {} before replacing it: {e}", m.path))?;
            eprintln!("sesync: kept {} before replacing it ({})", m.path, kept.display());
            now
        }
        Kind::Files => {
            // Every file of the copy: an Antigravity conversation's turns may sit in its WAL alone.
            let p = Path::new(&m.path);
            let mut files: Vec<(String, Vec<u8>)> = Vec::new();
            for f in copy_files(target, p) {
                match std::fs::read(&f) {
                    Ok(b) => files.push((f.file_name().and_then(|n| n.to_str()).unwrap_or("copy").to_string(), b)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound && files.is_empty() => return Ok(()),
                    Err(e) => return Err(format!("cannot read {} to keep it before replacing it: {e}", f.display())),
                }
            }
            let h = combine_hashes(&files.iter().map(|(_, b)| file_hash_bytes(b)).collect::<Vec<_>>());
            if !m.installed_hash.is_empty() && h == m.installed_hash {
                return Ok(());
            }
            for (name, bytes) in &files {
                let kept = keep_before_replace(stores.archive, &parent, name, bytes)
                    .map_err(|e| format!("cannot keep a copy of {} before replacing it: {e}", m.path))?;
                eprintln!("sesync: kept {} before replacing it ({})", m.path, kept.display());
            }
            h
        }
    };
    if let Some(rec) = state.sources.get_mut(source_key).and_then(|s| s.mirrors.get_mut(target)) {
        rec.installed_hash = current;
    }
    Ok(())
}

/// Record the lock alias for a copy's id. Aliases are part of the lock layer: without the lock
/// binary nothing claims, so nothing is written either (a test or a machine without sesync-lock
/// never gets files in a lock directory). A failure costs only the mutual exclusion for that
/// copy, so it is reported, not fatal.
fn alias(stores: &Stores, id: &str, conv: &str) {
    if !stores.sesync_lock.exists() {
        return;
    }
    if let Err(e) = crate::lock::write_alias(stores.lock_dir, id, conv) {
        eprintln!("sesync: cannot record the lock alias {id} -> {conv}: {e}");
    }
}

fn install_into(
    stores: &Stores,
    state: &mut State,
    source_key: &str,
    conv: &str,
    title: &str,
    h: &dyn Harness,
    src: &Path,
) -> Result<String, String> {
    let target = h.id();
    let loc = stores.locations.of(target);
    // Mirror ids are minted from the conversation's stable id, not from the moving source key: a
    // promotion changes the source key, and a key-derived id would mint a new copy in another
    // store and leave the old one behind. A copy already recorded keeps the id it has, so it is
    // replaced in place wherever the source lives; for the old source a promotion demoted, that
    // is the id its harness has always known it by.
    let stable = state.team_id_of(source_key);
    let mirror_id = state.sources.get(source_key)
        .and_then(|s| s.mirrors.get(target))
        .and_then(|m| h.id_in(&m.path))
        .unwrap_or_else(|| h.mirror_id(&stable));
    preserve_foreign(stores, state, source_key, h, &stable)?;
    let outcome = with_claim(stores.sesync_lock, Some(stores.lock_dir), conv, "daemon", std::process::id(), source_key, || {
        h.install(loc, &stores.tools(), src, &mirror_id, title)
    });
    let key = match outcome {
        ClaimOutcome::Skipped(holder) => return Err(format!("held by {holder}; will retry")),
        ClaimOutcome::Ran(r) => r?,
    };
    match h.kind() {
        Kind::Store => {
            record_store_mirror(state, source_key, target, &key)?;
            // What the store now holds for this copy, and when: the fingerprint makes "provably
            // ours" decidable for a stored copy, the time is where turn-taking starts looking.
            let updated = harness::row_updated(h, loc, &key).unwrap_or(0.0);
            let fingerprint = h.fingerprint(loc, &key).unwrap_or_default();
            if let Some(m) = state.sources.get_mut(source_key).and_then(|s| s.mirrors.get_mut(target)) {
                m.target_updated = updated;
                m.installed_hash = fingerprint;
            }
            alias(stores, &key, conv);
        }
        Kind::Files => {
            // The copy is a file in a store the scan walks: recording it in `mirror_keys` is what
            // stops the scan from treating it as a source.
            record_mirror(state, source_key, target, Path::new(&key))?;
            alias(stores, &mirror_id, conv);
        }
    }
    Ok(key)
}

/// Record a written mirror on its source. The source must already be in state. When this
/// replaces a mirror at a different path (a rewrite into the right bucket, a new filename),
/// the old file is removed first, but only when it is still exactly what sesync wrote (or was
/// kept in the archive just before this write), so nothing a user may have since edited is ever
/// lost. Anything kept stays in `mirror_keys`, which is what stops the scan from treating it as
/// a source.
pub fn record_mirror(state: &mut State, source_key: &str, target: &str, installed: &Path) -> Result<(), String> {
    let src = state.sources.get_mut(source_key).ok_or("unknown source")?;
    let path = installed.display().to_string();
    let replaced = src.mirrors.get(target).map(|m| (m.path.clone(), m.installed_hash.clone()));
    if let Some((old_path, old_hash)) = replaced {
        if old_path != path {
            let p = Path::new(&old_path);
            let ours_unchanged = crate::mirror::is_file_key(&old_path)
                && !old_hash.is_empty()
                && copy_hash(target, p).map(|h| h == old_hash).unwrap_or(false);
            if ours_unchanged {
                let _ = crate::mirror::remove_copy(target, p);
                state.mirror_keys.remove(&old_path);
            }
        }
    }
    state.mirror_keys.insert(path.clone());
    src.mirrors.insert(target.to_string(), Mirror {
        target: target.to_string(),
        path,
        source_hash: src.last_hash.clone(),
        installed_hash: copy_hash(target, installed).unwrap_or_default(),
        diverged: false,
        edit_stamp: String::new(),
        target_updated: 0.0,
    });
    Ok(())
}

/// Record a copy written into a store harness. There is no file: the copy lives in the harness's
/// own database, and the mirror's path carries its session id so the UI can show it. The id is
/// not added to `mirror_keys`: that set names files sesync wrote into harness stores, and an id
/// there could hide a real session of that harness from the scan.
pub fn record_store_mirror(state: &mut State, source_key: &str, target: &str, id: &str) -> Result<(), String> {
    let src = state.sources.get_mut(source_key).ok_or("unknown source")?;
    let mirror = Mirror {
        target: target.to_string(),
        path: id.to_string(),
        source_hash: src.last_hash.clone(),
        installed_hash: String::new(),
        diverged: false,
        edit_stamp: String::new(),
        target_updated: 0.0,
    };
    src.mirrors.insert(target.to_string(), mirror);
    Ok(())
}
