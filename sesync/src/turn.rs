//! Turn-taking: a mirror is a copy, but the user may answer in the copy's harness. When that
//! happens, that copy is the newest statement of the conversation and becomes the session's
//! source — the "main harness" moves — and the same pass rewrites every other configured copy
//! from it, including the old source. When more than one copy changed since the last pass,
//! the newest write wins; the losers are overwritten (operator decision).

use crate::divergence::fingerprint;
use crate::state::{Mirror, Source, State};
use std::path::Path;

/// One mirror whose content no longer matches what the daemon installed.
#[derive(Debug, Clone, PartialEq)]
pub struct MirrorEdit {
    /// The source entry the mirror hangs off.
    pub source_key: String,
    /// The harness the mirror lives in; it becomes the source's format.
    pub target: String,
    /// The mirror file. For OpenCode this is the conversation id (store-backed).
    pub path: String,
    /// The copy's last write, in seconds, as the tie-breaker.
    pub mtime: f64,
    /// The content hash at detection time.
    pub hash: String,
}

/// Whether a recorded mirror changed since the daemon last wrote or last saw it unchanged.
/// `SameContent` covers an mtime touch with identical bytes: it only refreshes the stamp.
#[derive(Debug)]
pub enum MirrorChange {
    /// Fingerprint matches the one recorded last pass.
    None,
    /// Content differs from what was installed; the harness wrote here.
    Edited(MirrorEdit),
    /// Same bytes as installed, newer stamp; the stamp is refreshed, no promotion.
    SameContent,
}

/// Check one recorded mirror against what the daemon installed. Cheap by design: a stat per
/// pass; the content is read only when the fingerprint moved.
pub fn check(mirror: &Mirror, source_key: &str) -> MirrorChange {
    let path = Path::new(&mirror.path);
    if mirror.target == "opencode" {
        return MirrorChange::None; // store-backed; the caller feeds row updates separately
    }
    let stamp = fingerprint(path);
    if stamp == "missing" {
        return MirrorChange::None; // a gone mirror is cleanup's business, not a promotion
    }
        if !mirror.edit_stamp.is_empty() && stamp == mirror.edit_stamp {
            return MirrorChange::None;
        }
        if debug_env() {
            eprintln!("turn: stamp moved for {} (recorded {:?})", mirror.path, mirror.edit_stamp);
        }
    match crate::mirror::file_hash(path) {
        Err(_) => MirrorChange::SameContent,
        Ok(h) if h == mirror.installed_hash => MirrorChange::SameContent,
        Ok(h) => MirrorChange::Edited(MirrorEdit {
            source_key: source_key.to_string(),
            target: mirror.target.clone(),
            path: mirror.path.clone(),
            mtime: crate::divergence::mtime_secs(path),
            hash: h,
        }),
    }
}

/// Find every edited mirror, promote the newest write per session, and report the new source
/// keys. Pure over the state: no file is written here, no harness is touched. The caller's
/// next pass writes the winner's mirrors through the normal pipeline.
fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn debug_env() -> bool {
    std::env::var_os("SESYNC_TURN_DEBUG").is_some()
}

pub fn promote_changed(state: &mut State, opencode_rows: &[(String, f64)]) -> Vec<String> {
    let debug = debug_env();
    // Collect the edits and the quiet stamps first (read-only), then resolve, then write.
    let mut edits: Vec<MirrorEdit> = Vec::new();
    let mut touched: Vec<(String, String, String)> = Vec::new();
    for (key, src) in state.sources.iter() {
        for m in src.mirrors.values() {
            if m.target == "opencode" {
                if m.target_updated > 0.0 {
                    if let Some((_, updated)) = opencode_rows.iter().find(|(id, _)| *id == m.path) {
                        if *updated > m.target_updated {
                            edits.push(MirrorEdit {
                                source_key: key.clone(),
                                target: m.target.clone(),
                                path: m.path.clone(),
                                mtime: *updated,
                                hash: format!("{:.3}", updated),
                            });
                        }
                    }
                }
                continue;
            }
            match check(m, key) {
                MirrorChange::None => {}
                MirrorChange::SameContent => {
                    touched.push((key.clone(), m.target.clone(), fingerprint(Path::new(&m.path))));
                }
                MirrorChange::Edited(e) => edits.push(e),
            }
        }
    }
    for (key, target, stamp) in touched {
        if let Some(src) = state.sources.get_mut(&key) {
            if let Some(m) = src.mirrors.get_mut(&target) {
                m.edit_stamp = stamp;
            }
        }
    }
    if debug {
        for e in &edits {
            eprintln!("turn: candidate {} of {} ({}) at {:.3}", e.path, e.source_key, e.target, e.mtime);
        }
    }
    // Components: entries whose keys are each other's mirror paths are copies of one
    // conversation (leftover tangles of an older binary). A component with a real edit turns
    // to the newest edit and keeps the winner's entry; one with none keeps its most recently
    // turned entry. Healthy states have one entry per component and are untouched here.
    let mut edits_sorted = edits;
    edits_sorted.sort_by(|a, b| {
        b.mtime.partial_cmp(&a.mtime).unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.target.cmp(&b.target))
    });
    let mut resolved: std::collections::BTreeSet<String> = Default::default();
    let mut promoted = Vec::new();
    for edit in &edits_sorted {
        if resolved.contains(&edit.source_key) {
            continue;
        }
        for member in component_of(state, &edit.source_key) {
            resolved.insert(member.clone());
            if member != edit.source_key {
                state.sources.remove(&member);
            }
        }
        if promote(state, edit).is_some() {
            promoted.push(edit.path.clone());
        }
    }
    // Components with no edit: collapse to the most recently turned entry.
    let keep: Vec<String> = {
        let mut roots: Vec<String> = Vec::new();
        let mut seen: std::collections::BTreeSet<String> = Default::default();
        for key in state.sources.keys().cloned().collect::<Vec<_>>() {
            if seen.contains(&key) {
                continue;
            }
            let members = component_of(state, &key);
            for m in &members {
                seen.insert(m.clone());
            }
            let best = members.iter()
                .map(|k| state.sources.get(k).unwrap())
                .max_by(|a, b| {
                    a.turn_at.partial_cmp(&b.turn_at).unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.last_seen_change.partial_cmp(&b.last_seen_change).unwrap_or(std::cmp::Ordering::Equal))
                })
                .map(|s| s.key.clone());
            if let Some(best) = best {
                for m in &members {
                    if *m != best {
                        state.sources.remove(m);
                    }
                }
                roots.push(best);
            }
        }
        roots
    };
    let _ = keep;
    promoted
}

/// The entries that are copies of the same conversation as `start`: transitively, two entries
/// are linked when one's key is recorded as a mirror path of the other.
fn component_of(state: &State, start: &str) -> Vec<String> {
    let mut out = vec![start.to_string()];
    let mut queue: Vec<String> = vec![start.to_string()];
    while let Some(k) = queue.pop() {
        let mut links: Vec<String> = state.sources.get(&k)
            .map(|s| s.mirrors.values().map(|m| m.path.clone()).collect())
            .unwrap_or_default();
        // And the entries that record `k` as their mirror.
        for (key, src) in &state.sources {
            if src.mirrors.values().any(|m| m.path == k) {
                links.push(key.clone());
            }
        }
        for l in links {
            if state.sources.contains_key(&l) && !out.contains(&l) {
                out.push(l.clone());
                queue.push(l);
            }
        }
    }
    out
}

/// Move the session to the winner: the winner's file becomes the source, the old source's
/// harness becomes a mirror target. The old source's other mirror records are carried over,
/// so a later re-write into a new path removes the old file (record_mirror's replace rule:
/// the stale copies' bytes are still what the daemon installed). Returns the new source key.
fn promote(state: &mut State, edit: &MirrorEdit) -> Option<String> {
    let old = state.sources.get(&edit.source_key)?.clone();
    if edit.path == edit.source_key {
        return None; // the winner is already the source; not a turn
    }
    if state.sources.contains_key(&edit.path) {
        // The destination copy already holds the source entry (a redundant candidate from a
        // tangled leftover): overwriting it would graft this leftover's stale mirror records
        // onto the live entry. Nothing to turn.
        return None;
    }
    let mut chosen = old.chosen_targets.clone();
    chosen.remove(&edit.target);
    chosen.insert(old.fmt.clone());
    let mut new = Source::new(&edit.target, &edit.path);
    new.title = old.title.clone();
    // The conversation's stable id moves with the turn: the team keys the mark, the pushed
    // hash and the server conversation on it, so the promoted copy keeps pushing as the SAME
    // conversation on the server, and the server keeps holding the newest version.
    new.conv = old.conv.clone();
    new.last_seen_change = edit.mtime;
    new.last_hash = edit.hash.clone();
    new.chosen_targets = chosen;
    new.turn_at = now_secs();
    let mut mirrors = old.mirrors.clone();
    mirrors.remove(&edit.target);
    // The old source's harness becomes a mirror of the winner. For a file that is the stale
    // bytes as they sit on disk; for OpenCode it is the conversation id, and the next write
    // replaces that conversation in place.
    let installed_hash = if edit.source_key.starts_with('/') {
        crate::mirror::file_hash(Path::new(&edit.source_key)).unwrap_or_default()
    } else {
        String::new()
    };
    mirrors.insert(old.fmt.clone(), Mirror {
        target: old.fmt.clone(),
        path: edit.source_key.clone(),
        source_hash: edit.hash.clone(),
        installed_hash,
        diverged: false,
        edit_stamp: String::new(),
        target_updated: 0.0,
    });
    new.mirrors = mirrors;
    state.sources.remove(&edit.source_key);
    state.sources.insert(edit.path.clone(), new);
    // The winner is no longer a mirror; the old source now is, when it is a file.
    state.mirror_keys.remove(&edit.path);
    if edit.source_key.starts_with('/') {
        state.mirror_keys.insert(edit.source_key.clone());
    }
    Some(edit.path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Source, State};
    use std::fs;
    use std::path::PathBuf;

    fn sb(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sesync-turn-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn omp_source(sb: &PathBuf, name: &str, body: &str) -> String {
        let p = sb.join("omp").join("sessions").join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        p.display().to_string()
    }

    fn mirror_file(sb: &PathBuf, body: &str) -> String {
        let p = sb.join("claude-mirror.jsonl");
        fs::write(&p, body).unwrap();
        p.display().to_string()
    }

    fn source_with_mirror(p_omp: String, c_path: String) -> (State, String) {
        let mut state = State::default();
        let mut src = Source::new("omp", &p_omp);
        src.chosen_targets.insert("claude".to_string());
        let c_hash = crate::mirror::file_hash(std::path::Path::new(&c_path)).unwrap();
        src.mirrors.insert("claude".to_string(), Mirror {
            target: "claude".to_string(),
            path: c_path.clone(),
            source_hash: "old".to_string(),
            installed_hash: c_hash,
            diverged: false,
            edit_stamp: String::new(),
            target_updated: 0.0,
        });
        state.sources.insert(p_omp.clone(), src);
        (state, p_omp)
    }

    const SRC_BODY: &str = "{\"type\":\"session\",\"id\":\"x\"}\n";

    #[test]
    fn an_edited_mirror_becomes_the_source() {
        let d = sb("basic");
        let p_omp = omp_source(&d, "a.jsonl", SRC_BODY);
        let c_path = mirror_file(&d, "{\"old\":true}\n");
        let (mut state, old_key) = source_with_mirror(p_omp, c_path.clone());
        // The user answers in claude: the mirror's content changes.
        fs::write(&c_path, "{\"old\":true}\n{\"user\":\"hi\"}\n").unwrap();
        let promoted = promote_changed(&mut state, &[]);
        assert_eq!(promoted, vec![c_path.clone()], "the edited copy wins");
        let new = &state.sources[&c_path];
        assert_eq!(new.fmt, "claude", "the winner's harness is the format");
        assert!(new.chosen_targets.contains("omp"), "the old source's harness becomes a target");
        assert!(!new.chosen_targets.contains("claude"), "the winner does not mirror to itself");
        assert!(state.sources.get(&old_key).is_none(), "the old source entry is gone");
        assert!(state.mirror_keys.contains(&old_key), "the old source file is now a mirror");
        assert!(!state.mirror_keys.contains(&c_path), "the winner is no longer a mirror");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_touched_mirror_with_the_same_content_promotes_nothing() {
        let d = sb("quiet");
        let p_omp = omp_source(&d, "b.jsonl", SRC_BODY);
        let c_path = mirror_file(&d, "{\"old\":true}\n");
        let (mut state, old_key) = source_with_mirror(p_omp, c_path.clone());
        assert!(promote_changed(&mut state, &[]).is_empty(), "nothing changed");
        // The fingerprint was recorded; a later identical pass costs nothing and stays quiet.
        assert_eq!(
            state.sources[&old_key].mirrors["claude"].edit_stamp,
            fingerprint(std::path::Path::new(&c_path)),
            "the stamp is recorded for the next pass");
        // A different stamp but identical content: SameContent, no promotion.
        let file = std::path::Path::new(&c_path);
        let stamp = fingerprint(file);
        state.sources.get_mut(&old_key).unwrap().mirrors.get_mut("claude").unwrap().edit_stamp = format!("x{}", stamp.len());
        let changed = check(&state.sources[&old_key].mirrors["claude"], &old_key);
        assert!(matches!(changed, MirrorChange::SameContent), "same bytes never promote");
        assert!(promote_changed(&mut state, &[]).is_empty(), "no promotion");
        assert_eq!(state.sources[&old_key].mirrors["claude"].edit_stamp, stamp,
            "the fresh stamp replaces the stale one");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_newest_of_two_edited_copies_wins() {
        let d = sb("conflict");
        let p_omp = omp_source(&d, "c.jsonl", SRC_BODY);
        let c_path = mirror_file(&d, "{\"a\":1}\n");
        let (mut state, _) = source_with_mirror(p_omp, c_path.clone());
        // Two edits: pi (older) and claude (newer). Newer wins.
        let older = d.join("older.jsonl");
        fs::write(&older, "{\"pi\":1}\n").unwrap();
        let src = state.sources.values_mut().next().unwrap();
        src.mirrors.insert("pi".to_string(), Mirror {
            target: "pi".to_string(),
            path: older.display().to_string(),
            source_hash: "old".to_string(),
            installed_hash: "stale".to_string(),
            diverged: false,
            edit_stamp: String::new(),
            target_updated: 0.0,
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&c_path, "{\"a\":1}\n{\"user\":\"later\"}\n").unwrap();
        let promoted = promote_changed(&mut state, &[]);
        assert_eq!(promoted, vec![c_path.clone()], "the newest write wins");
        assert_eq!(state.sources[&c_path].fmt, "claude");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_missing_mirror_is_not_a_promotion() {
        let d = sb("missing");
        let p_omp = omp_source(&d, "d.jsonl", SRC_BODY);
        let c_path = sb("missing").join("gone.jsonl").display().to_string();
        let mut state = State::default();
        let mut src = Source::new("omp", &p_omp);
        src.chosen_targets.insert("claude".to_string());
        src.mirrors.insert("claude".to_string(), Mirror {
            target: "claude".to_string(),
            path: c_path.clone(),
            source_hash: "old".to_string(),
            installed_hash: String::new(), // recorded before fingerprinting existed
            diverged: false,
            edit_stamp: String::new(),
            target_updated: 0.0,
        });
        state.sources.insert(p_omp.clone(), src);
        assert!(promote_changed(&mut state, &[]).is_empty(), "a gone mirror is cleanup's business");
        assert!(state.sources.contains_key(&p_omp), "the source survives");
        fs::remove_dir_all(&d).unwrap();
    }
    #[test]
    fn an_old_state_without_target_updated_never_promotes_on_upgrade() {
        // States written before turn-taking existed have target_updated = 0.0 on their
        // OpenCode mirrors: the first pass must adopt them quietly, not promote spuriously.
        let d = sb("upgrade");
        let p_omp = omp_source(&d, "u.jsonl", SRC_BODY);
        let mut state = State::default();
        let mut src = Source::new("omp", &p_omp);
        src.chosen_targets.insert("opencode".to_string());
        src.mirrors.insert("opencode".to_string(), Mirror {
            target: "opencode".to_string(),
            path: "ses_upgraded".to_string(),
            source_hash: "old".to_string(),
            installed_hash: String::new(),
            diverged: false,
            edit_stamp: String::new(),
            target_updated: 0.0,
        });
        state.sources.insert(p_omp.clone(), src);
        // Even a rising row update must not promote while nothing was recorded at install.
        let promoted = promote_changed(&mut state, &[("ses_upgraded".to_string(), 9999999.0)]);
        assert!(promoted.is_empty(), "target_updated = 0.0 means 'never installed', not 'edited'");
        assert!(state.sources.contains_key(&p_omp));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_exact_mtime_tie_breaks_by_harness_name() {
        let d = sb("tie");
        let p_omp = omp_source(&d, "t.jsonl", SRC_BODY);
        let mut state = State::default();
        let mut src = Source::new("opencode", "ses_tie");
        src.chosen_targets.insert("pi".to_string());
        src.chosen_targets.insert("gemini".to_string());
        let now = 2000.0;
        src.last_seen_change = now;
        for (t, path) in [("pi", "pi.jsonl"), ("gemini", "gemini.jsonl")] {
            let p = d.join(t).join(t);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, "{\"v\":1}\n").unwrap();
            src.mirrors.insert(t.to_string(), Mirror {
                target: t.to_string(),
                path: p.display().to_string(),
                source_hash: "old".to_string(),
                installed_hash: "stale".to_string(),
                diverged: false,
                edit_stamp: String::new(),
                target_updated: 0.0,
            });
        }
        state.sources.insert("ses_tie".to_string(), src);
        // Both copies report the same mtime; the tie must resolve by name (gemini < pi).
        let promoted = promote_changed(&mut state, &[]);
        let gpath = d.join("gemini").join("gemini").display().to_string();
        assert_eq!(promoted, vec![gpath.clone()],
            "the tie-break picks the alphabetically first harness");
        assert_eq!(state.sources[&gpath].fmt, "gemini");
        fs::remove_dir_all(&d).unwrap();
    }
    #[test]
    fn a_tangle_of_copies_collapses_to_one_entry() {
        // Two entries mirroring each other (an older bug's disk state): every pass would turn
        // the session again because each copy's rewrite invalidates the other's fingerprint.
        // The collapse keeps one entry and stops the machine.
        let d = sb("tangle");
        let p_omp = omp_source(&d, "x.jsonl", SRC_BODY);
        let c_path = mirror_file(&d, "{\"a\":1}\n");
        let mut state = State::default();
        let mut src = Source::new("omp", &p_omp);
        src.chosen_targets.insert("claude".to_string());
        src.mirrors.insert("claude".to_string(), Mirror {
            target: "claude".to_string(),
            path: c_path.clone(),
            source_hash: "old".to_string(),
            installed_hash: "stale".to_string(),
            diverged: false,
            edit_stamp: String::new(),
            target_updated: 0.0,
        });
        state.sources.insert(p_omp.clone(), src);
        let mut cl = Source::new("claude", &c_path);
        cl.chosen_targets.insert("omp".to_string());
        cl.mirrors.insert("omp".to_string(), Mirror {
            target: "omp".to_string(),
            path: p_omp.clone(),
            source_hash: "old".to_string(),
            installed_hash: "stale".to_string(),
            diverged: false,
            edit_stamp: String::new(),
            target_updated: 0.0,
        });
        state.sources.insert(c_path.clone(), cl);
        let promoted = promote_changed(&mut state, &[]);
        assert_eq!(promoted.len(), 1, "one winner, not a cycle");
        assert_eq!(state.sources.len(), 1, "the tangle collapsed to a single entry");
        // And a second pass with nothing touched turns nothing.
        assert!(promote_changed(&mut state, &[]).is_empty(), "the machine stopped");
        fs::remove_dir_all(&d).unwrap();
    }
}
