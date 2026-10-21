//! Turn-taking: a mirror is a copy, but the user may answer in the copy's harness. When that
//! happens, that copy is the newest statement of the conversation and becomes the session's
//! source (the "main harness" moves), and the same pass rewrites every other configured copy
//! from it, including the old source. When more than one copy was answered since the last pass,
//! the newest answer wins; the losers are overwritten (operator decision), and bytes sesync did
//! not write are archived before that happens (mirror_write).
//!
//! An answer is a user prompt in the copy that is newer than every prompt of the source. Every
//! copy sesync writes carries the source's own timestamps, so a copy can only get ahead of its
//! source by somebody typing into it. Changed bytes alone never move the source: a rename, a
//! resume that appends bookkeeping, a file rewritten by an older binary or a state record that
//! fell behind all change bytes, and each of those used to turn the conversation over to a stale
//! copy, which then overwrote everything else.

use crate::harness;
use crate::mirror::{copy_hash, copy_stamp};
use crate::state::{Mirror, Source, State};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// When the user last typed into a conversation: (format, key) -> newest prompt, in seconds
/// since the epoch. The daemon asks each harness; tests pass fixed answers.
pub type PromptOf<'a> = &'a dyn Fn(&str, &str) -> Option<f64>;

/// Per store harness (by id), the (session id, last update) of every row its database lists.
pub type StoreRows = BTreeMap<String, Vec<(String, f64)>>;

/// A copy is ahead of its source only by a prompt typed at least this much later. Every format
/// keeps milliseconds, so this only absorbs rounding.
const PROMPT_SLACK_SECS: f64 = 1.0;

/// One copy somebody answered in.
#[derive(Debug, Clone, PartialEq)]
pub struct MirrorEdit {
    /// The source entry the mirror hangs off.
    pub source_key: String,
    /// The harness the mirror lives in; it becomes the source's format.
    pub target: String,
    /// The mirror file. For OpenCode this is the conversation id (store-backed).
    pub path: String,
    /// The copy's newest prompt, in seconds: the newest answer wins.
    pub mtime: f64,
    /// The content hash at detection time.
    pub hash: String,
}

/// What a pass learns about one recorded file mirror. Cheap by design: a stat per pass; the
/// content is read only when the fingerprint moved.
#[derive(Debug)]
pub enum MirrorChange {
    /// Fingerprint matches the one recorded last pass.
    None,
    /// The copy holds a prompt the source does not: somebody answered there.
    Edited(MirrorEdit),
    /// Same bytes as installed, newer stamp; the stamp is refreshed, no promotion.
    SameContent,
    /// The bytes changed but nobody answered (a rename, resume bookkeeping, an older write):
    /// the stamp is refreshed, no promotion.
    NoTurn,
}

/// The copy's newest prompt when it is newer than every prompt of its source. None when the copy
/// has no prompt, or none the source does not already have.
fn ahead_of_source(prompt_of: PromptOf, copy_fmt: &str, copy_key: &str, src_fmt: &str, src_key: &str) -> Option<f64> {
    let copy = prompt_of(copy_fmt, copy_key)?;
    match prompt_of(src_fmt, src_key) {
        Some(source) if copy <= source + PROMPT_SLACK_SECS => None,
        _ => Some(copy),
    }
}

/// Check one recorded file mirror of `src` (keyed `source_key`).
pub fn check(mirror: &Mirror, src: &Source, source_key: &str, prompt_of: PromptOf) -> MirrorChange {
    let path = Path::new(&mirror.path);
    if harness::is_store(&mirror.target) {
        return MirrorChange::None; // in a store harness; promote_changed reads its rows
    }
    // An Antigravity copy changes in its WAL while agy runs: the stamp and the hash cover both.
    let stamp = copy_stamp(&mirror.target, path);
    if stamp == "missing" {
        return MirrorChange::None; // a gone mirror is cleanup's business, not a promotion
    }
    if !mirror.edit_stamp.is_empty() && stamp == mirror.edit_stamp {
        return MirrorChange::None;
    }
    if debug_env() {
        eprintln!("turn: stamp moved for {} (recorded {:?})", mirror.path, mirror.edit_stamp);
    }
    match copy_hash(&mirror.target, path) {
        Err(_) => MirrorChange::SameContent,
        Ok(h) if h == mirror.installed_hash => MirrorChange::SameContent,
        Ok(h) => match ahead_of_source(prompt_of, &mirror.target, &mirror.path, &src.fmt, source_key) {
            Some(at) => MirrorChange::Edited(MirrorEdit {
                source_key: source_key.to_string(),
                target: mirror.target.clone(),
                path: mirror.path.clone(),
                mtime: at,
                hash: h,
            }),
            None => MirrorChange::NoTurn,
        },
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn debug_env() -> bool {
    std::env::var_os("SESYNC_TURN_DEBUG").is_some()
}

/// Collapse leftover tangles, find every copy somebody answered in, promote the newest answer per
/// conversation, and report the new source keys. Pure over the state: no file is written here,
/// no harness is touched. The caller writes the winner's mirrors through the normal pipeline.
/// `store_rows` holds the rows of each store harness.
pub fn promote_changed(state: &mut State, store_rows: &StoreRows, prompt_of: PromptOf) -> Vec<String> {
    let debug = debug_env();
    collapse_tangles(state);
    let mut edits: Vec<MirrorEdit> = Vec::new();
    let mut stamps: Vec<(String, String, String)> = Vec::new();
    let mut rows_seen: Vec<(String, String, f64)> = Vec::new();
    for (key, src) in state.sources.iter() {
        for m in src.mirrors.values() {
            if harness::is_store(&m.target) {
                // The row's update time filters out the untouched copies; the prompts decide.
                let row = store_rows.get(&m.target).and_then(|rows| rows.iter().find(|(id, _)| *id == m.path));
                let Some((_, updated)) = row else { continue };
                if *updated <= m.target_updated {
                    continue;
                }
                match ahead_of_source(prompt_of, &m.target, &m.path, &src.fmt, key) {
                    Some(at) => edits.push(MirrorEdit {
                        source_key: key.clone(),
                        target: m.target.clone(),
                        path: m.path.clone(),
                        mtime: at,
                        hash: format!("{updated:.3}"),
                    }),
                    None => rows_seen.push((key.clone(), m.target.clone(), *updated)),
                }
                continue;
            }
            match check(m, src, key, prompt_of) {
                MirrorChange::None => {}
                MirrorChange::SameContent | MirrorChange::NoTurn => {
                    stamps.push((key.clone(), m.target.clone(), copy_stamp(&m.target, Path::new(&m.path))));
                }
                MirrorChange::Edited(e) => edits.push(e),
            }
        }
    }
    for (key, target, stamp) in stamps {
        if let Some(m) = state.sources.get_mut(&key).and_then(|s| s.mirrors.get_mut(&target)) {
            m.edit_stamp = stamp;
        }
    }
    for (key, target, updated) in rows_seen {
        if let Some(m) = state.sources.get_mut(&key).and_then(|s| s.mirrors.get_mut(&target)) {
            m.target_updated = updated;
        }
    }
    if debug {
        for e in &edits {
            eprintln!("turn: answered in {} of {} ({}) at {:.3}", e.path, e.source_key, e.target, e.mtime);
        }
    }
    // The newest answer wins; an exact tie breaks by harness name, so the outcome is stable.
    edits.sort_by(|a, b| b.mtime.total_cmp(&a.mtime).then_with(|| a.target.cmp(&b.target)));
    let mut turned: BTreeSet<String> = BTreeSet::new();
    let mut promoted = Vec::new();
    for edit in &edits {
        if !turned.insert(edit.source_key.clone()) {
            continue;
        }
        if let Some(key) = promote(state, edit) {
            promoted.push(key);
        }
    }
    promoted
}

/// Entries whose keys are each other's mirror paths are copies of one conversation, left by an
/// older binary (or a merge that re-adopted a demoted source). Each such group keeps its most
/// recently turned entry and drops the rest before any copy is checked, so a stale entry's
/// records can never turn the conversation back. A dropped file stays a mirror key, so the scan
/// does not adopt it again; when its harness is still chosen, the survivor records it as a copy
/// whose bytes sesync did not write.
fn collapse_tangles(state: &mut State) {
    let keys: Vec<String> = state.sources.keys().cloned().collect();
    let mut done: BTreeSet<String> = BTreeSet::new();
    for key in keys {
        if done.contains(&key) || !state.sources.contains_key(&key) {
            continue;
        }
        let members = component_of(state, &key);
        done.extend(members.iter().cloned());
        if members.len() < 2 {
            continue;
        }
        let Some(best) = members.iter()
            .max_by(|a, b| {
                let (a, b) = (&state.sources[*a], &state.sources[*b]);
                a.turn_at.total_cmp(&b.turn_at).then_with(|| a.last_seen_change.total_cmp(&b.last_seen_change))
            })
            .cloned() else { continue };
        for member in members.iter().filter(|m| **m != best) {
            let Some(gone) = state.sources.remove(member) else { continue };
            let keep = state.sources.get_mut(&best).expect("the survivor stays");
            if keep.chosen_targets.contains(&gone.fmt) && !keep.mirrors.contains_key(&gone.fmt) && gone.fmt != keep.fmt {
                keep.mirrors.insert(gone.fmt.clone(), Mirror {
                    target: gone.fmt.clone(),
                    path: member.clone(),
                    ..Default::default()
                });
            }
            for (target, rec) in gone.mirrors {
                if target != keep.fmt && rec.path != best && !keep.mirrors.contains_key(&target) {
                    keep.mirrors.insert(target, rec);
                }
            }
            for (target, rec) in gone.sync {
                if target != keep.fmt && !keep.sync.contains_key(&target) {
                    keep.sync.insert(target, rec);
                }
            }
            if crate::mirror::is_file_key(member) {
                state.mirror_keys.insert(member.clone());
            }
        }
    }
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

/// Move the session to the winner: the winner's copy becomes the source, the old source becomes
/// the copy in its own harness. The old source's other mirror records are carried over, so the
/// next write replaces each copy in place. Returns the new source key.
fn promote(state: &mut State, edit: &MirrorEdit) -> Option<String> {
    if edit.path == edit.source_key || state.sources.contains_key(&edit.path) {
        // The winner already holds a source entry: nothing to turn, and grafting this entry's
        // records onto it would overwrite the live ones.
        return None;
    }
    let conv = state.team_id_of(&edit.source_key);
    let old = state.sources.get(&edit.source_key)?.clone();
    let now = now_secs();
    let mut chosen = old.chosen_targets.clone();
    chosen.remove(&edit.target);
    chosen.insert(old.fmt.clone());
    let mut new = Source::new(&edit.target, &edit.path);
    new.title = old.title.clone();
    // Sync records follow the targets they describe. The winner's own target is no longer a
    // mirror, and the old source's target now holds bytes sesync did not write: neither keeps one.
    new.sync = old.sync.clone();
    new.sync.remove(&edit.target);
    new.sync.remove(&old.fmt);
    // The conversation's stable id moves with the turn: mirror ids are minted from it and the
    // team keys on it, so every copy keeps its id and the server keeps one conversation.
    new.conv = conv;
    new.last_seen_change = edit.mtime;
    new.last_hash = edit.hash.clone();
    new.chosen_targets = chosen;
    new.turn_at = now;
    let mut mirrors = old.mirrors.clone();
    mirrors.remove(&edit.target);
    // The old source is now the copy in its own harness. sesync did not write those bytes, and
    // the record says so: an empty installed_hash is never "ours, unchanged", so nothing deletes
    // them, and the next write keeps a verified archive of them before replacing them in place.
    let file = crate::mirror::is_file_key(&edit.source_key);
    mirrors.insert(old.fmt.clone(), Mirror {
        target: old.fmt.clone(),
        path: edit.source_key.clone(),
        source_hash: String::new(),
        installed_hash: String::new(),
        diverged: false,
        edit_stamp: if file { copy_stamp(&old.fmt, Path::new(&edit.source_key)) } else { String::new() },
        target_updated: now,
    });
    new.mirrors = mirrors;
    state.sources.remove(&edit.source_key);
    state.sources.insert(edit.path.clone(), new);
    state.seen_keys.insert(edit.path.clone());
    // The winner is no longer a mirror; the old source now is, when it is a file.
    state.mirror_keys.remove(&edit.path);
    if file {
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

    /// Files are read for real; store answers come from the map.
    fn files_and(stored: Vec<(&'static str, f64)>) -> impl Fn(&str, &str) -> Option<f64> {
        move |fmt: &str, key: &str| match harness::get(fmt) {
            Some(h) if h.kind() == harness::Kind::Files => h.newest_prompt(&harness::Location::default(), key),
            _ => stored.iter().find(|(id, _)| *id == key).map(|(_, t)| *t),
        }
    }

    fn no_rows() -> StoreRows {
        StoreRows::new()
    }

    fn omp_prompt(id: &str, at: &str, text: &str) -> String {
        format!(r#"{{"type":"message","id":"{id}","timestamp":"{at}","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#)
    }

    fn claude_prompt(at: &str, text: &str) -> String {
        format!(r#"{{"type":"user","uuid":"u-{at}","timestamp":"{at}","message":{{"role":"user","content":"{text}"}}}}"#)
    }

    const T1: &str = "2026-10-01T10:00:00.000Z";
    const T2: &str = "2026-10-01T10:05:00.000Z";
    const T3: &str = "2026-10-01T10:09:00.000Z";

    /// An omp source with one prompt at T1 and its Claude copy as sesync installed it.
    fn source_and_copy(d: &Path) -> (State, String, String) {
        let src = d.join("omp").join("a.jsonl");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::write(&src, format!("{{\"type\":\"session\",\"id\":\"a\",\"cwd\":\"/p\"}}\n{}\n", omp_prompt("m1", T1, "first question"))).unwrap();
        let copy = d.join("claude").join("c.jsonl");
        fs::create_dir_all(copy.parent().unwrap()).unwrap();
        fs::write(&copy, format!("{}\n", claude_prompt(T1, "first question"))).unwrap();
        let (src, copy) = (src.display().to_string(), copy.display().to_string());
        let mut state = State::default();
        let mut s = Source::new("omp", &src);
        s.chosen_targets.insert("claude".into());
        s.mirrors.insert("claude".into(), Mirror {
            target: "claude".into(),
            path: copy.clone(),
            source_hash: "h".into(),
            installed_hash: crate::mirror::file_hash(Path::new(&copy)).unwrap(),
            ..Default::default()
        });
        state.sources.insert(src.clone(), s);
        (state, src, copy)
    }

    fn append(path: &str, line: &str) {
        let mut body = fs::read_to_string(path).unwrap();
        body.push_str(line);
        body.push('\n');
        fs::write(path, body).unwrap();
    }

    #[test]
    fn an_answer_in_a_copy_moves_the_source_there() {
        let d = sb("answer");
        let (mut state, src, copy) = source_and_copy(&d);
        append(&copy, &claude_prompt(T2, "a new question asked in claude"));
        let promoted = promote_changed(&mut state, &no_rows(), &files_and(vec![]));
        assert_eq!(promoted, vec![copy.clone()], "the answered copy wins");
        let new = &state.sources[&copy];
        assert_eq!(new.fmt, "claude");
        assert!(new.chosen_targets.contains("omp") && !new.chosen_targets.contains("claude"));
        assert!(state.sources.get(&src).is_none(), "the old source entry is gone");
        assert!(state.mirror_keys.contains(&src), "the old source file is now a mirror");
        assert_eq!(new.mirrors["omp"].installed_hash, "",
            "the old source's bytes are not sesync's: nothing may treat them as a disposable copy");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn changed_bytes_without_a_new_prompt_never_move_the_source() {
        // The bug this guards: a copy whose bytes no longer matched the recorded hash (a rename,
        // resume bookkeeping, a write the state record missed) was promoted, and its stale
        // content then overwrote the real conversation everywhere.
        let d = sb("noturn");
        let (mut state, src, copy) = source_and_copy(&d);
        append(&copy, r#"{"type":"custom-title","customTitle":"renamed in claude","sessionId":"c"}"#);
        append(&copy, r#"{"type":"user","timestamp":"2026-10-01T10:30:00.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}}"#);
        assert!(promote_changed(&mut state, &no_rows(), &files_and(vec![])).is_empty(), "no prompt, no turn");
        assert!(state.sources.contains_key(&src), "the source stays where it is");
        assert_eq!(state.sources[&src].mirrors["claude"].edit_stamp, copy_stamp("claude", Path::new(&copy)),
            "the new stamp is recorded, so the next pass costs a stat");
        // A record that fell behind: the copy is a full rewrite with the source's own prompts.
        fs::write(&copy, format!("{}\n{}\n", claude_prompt(T1, "first question"), claude_prompt(T1, "first question"))).unwrap();
        assert!(promote_changed(&mut state, &no_rows(), &files_and(vec![])).is_empty(), "the source's own prompts are no answer");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_compaction_note_is_not_an_answer() {
        let d = sb("note");
        let (mut state, src, copy) = source_and_copy(&d);
        append(&copy, &claude_prompt(T3, "Earlier turns of this conversation were compacted. Their summary: x"));
        assert!(promote_changed(&mut state, &no_rows(), &files_and(vec![])).is_empty(),
            "sesync's own note is stamped with the compaction time and must not read as a turn");
        assert!(state.sources.contains_key(&src));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_answer_in_the_source_after_the_copy_keeps_the_source() {
        let d = sb("source-newer");
        let (mut state, src, copy) = source_and_copy(&d);
        append(&copy, &claude_prompt(T2, "asked in claude"));
        append(&src, &omp_prompt("m2", T3, "asked later in omp"));
        assert!(promote_changed(&mut state, &no_rows(), &files_and(vec![])).is_empty(),
            "the newest answer wins, and it is the source's");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_newest_of_two_answered_copies_wins() {
        let d = sb("two");
        let (mut state, src, copy) = source_and_copy(&d);
        let pi = d.join("pi").join("p.jsonl");
        fs::create_dir_all(pi.parent().unwrap()).unwrap();
        fs::write(&pi, format!("{}\n{}\n", omp_prompt("m1", T1, "first question"), omp_prompt("m9", T2, "asked in pi"))).unwrap();
        let pi = pi.display().to_string();
        let s = state.sources.get_mut(&src).unwrap();
        s.chosen_targets.insert("pi".into());
        s.mirrors.insert("pi".into(), Mirror { target: "pi".into(), path: pi.clone(), installed_hash: "older".into(), ..Default::default() });
        append(&copy, &claude_prompt(T3, "asked later in claude"));
        let promoted = promote_changed(&mut state, &no_rows(), &files_and(vec![]));
        assert_eq!(promoted, vec![copy.clone()], "claude answered last");
        assert_eq!(state.sources[&copy].mirrors["pi"].path, pi, "the other copy stays recorded, to be rewritten");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_opencode_copy_turns_only_on_a_newer_prompt() {
        let d = sb("opencode");
        let (mut state, src, _copy) = source_and_copy(&d);
        let s = state.sources.get_mut(&src).unwrap();
        s.chosen_targets.insert("opencode".into());
        s.mirrors.insert("opencode".into(), Mirror { target: "opencode".into(), path: "ses_x".into(), target_updated: 100.0, ..Default::default() });
        let t1 = crate::prompts::parse_rfc3339(T1).unwrap();
        // The row moved (a rename, an open) but the copy's newest prompt is the source's own.
        let rows: StoreRows = [("opencode".to_string(), vec![("ses_x".to_string(), 500.0)])].into_iter().collect();
        assert!(promote_changed(&mut state, &rows, &files_and(vec![("ses_x", t1)])).is_empty());
        assert_eq!(state.sources[&src].mirrors["opencode"].target_updated, 500.0, "the row time is taken as seen");
        // Somebody typed in OpenCode.
        let rows: StoreRows = [("opencode".to_string(), vec![("ses_x".to_string(), 900.0)])].into_iter().collect();
        let promoted = promote_changed(&mut state, &rows, &files_and(vec![("ses_x", t1 + 60.0)]));
        assert_eq!(promoted, vec!["ses_x".to_string()]);
        assert_eq!(state.sources["ses_x"].fmt, "opencode");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_missing_mirror_is_not_a_promotion() {
        let d = sb("missing");
        let (mut state, src, copy) = source_and_copy(&d);
        fs::remove_file(&copy).unwrap();
        assert!(promote_changed(&mut state, &no_rows(), &files_and(vec![])).is_empty(), "a gone mirror is cleanup's business");
        assert!(state.sources.contains_key(&src));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn two_answers_at_the_same_instant_break_by_harness_name() {
        let d = sb("tie");
        let (mut state, src, copy) = source_and_copy(&d);
        let pi = d.join("pi").join("p.jsonl");
        fs::create_dir_all(pi.parent().unwrap()).unwrap();
        fs::write(&pi, format!("{}\n", omp_prompt("m9", T2, "asked in pi"))).unwrap();
        let pi = pi.display().to_string();
        let s = state.sources.get_mut(&src).unwrap();
        s.chosen_targets.insert("pi".into());
        s.mirrors.insert("pi".into(), Mirror { target: "pi".into(), path: pi, installed_hash: "older".into(), ..Default::default() });
        append(&copy, &claude_prompt(T2, "asked in claude"));
        let promoted = promote_changed(&mut state, &no_rows(), &files_and(vec![]));
        assert_eq!(promoted, vec![copy], "claude sorts before pi, so the outcome never depends on map order");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_tangle_collapses_to_its_latest_turn_and_does_not_turn_again() {
        // The loop seen on 2026-10-21: the demoted OpenCode source was adopted back from disk, so
        // it and the promoted copy recorded each other as mirrors. The stale side's record
        // of the copy no longer matched, and the copy was promoted (and the OpenCode session
        // deleted and re-imported) on every pass.
        let d = sb("tangle");
        let (mut state, src, copy) = source_and_copy(&d);
        state.sources.get_mut(&src).unwrap().mirrors.get_mut("claude").unwrap().installed_hash = "stale".into();
        let mut cl = Source::new("claude", &copy);
        cl.turn_at = 5000.0;
        cl.chosen_targets.insert("omp".into());
        cl.mirrors.insert("omp".into(), Mirror { target: "omp".into(), path: src.clone(), installed_hash: "stale".into(), ..Default::default() });
        state.sources.insert(copy.clone(), cl);
        let promoted = promote_changed(&mut state, &no_rows(), &files_and(vec![]));
        assert!(promoted.is_empty(), "collapsing a tangle is not a turn");
        assert_eq!(state.sources.len(), 1, "one entry per conversation");
        assert!(state.sources.contains_key(&copy), "the most recently turned entry survives");
        assert!(state.mirror_keys.contains(&src), "the dropped file stays a mirror, never a source again");
        assert!(promote_changed(&mut state, &no_rows(), &files_and(vec![])).is_empty(), "the machine stopped");
        fs::remove_dir_all(&d).unwrap();
    }
}
