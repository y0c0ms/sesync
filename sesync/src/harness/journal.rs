//! What the journal-file harnesses (omp, Pi, Claude Code) share: one JSONL file per conversation,
//! records appended in order, one directory below the store. Listing and naming the files, reading
//! a title from a window of a large file and the newest prompt, installing a copy through
//! `sesync-conv to-<id>`, and rewriting a file in place for a rename.

use serde_json::Value;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

/// How much of a large transcript is read for its title, from each end.
const TITLE_WINDOW: u64 = 64 * 1024;
/// Files up to this size are read whole; above it only the windows are.
const TITLE_FULL_MAX: u64 = 256 * 1024;
/// How far back the tail is walked looking for a title record before giving up.
const TITLE_TAIL_MAX: u64 = 8 * 1024 * 1024;

/// Whether a chunk of transcript carries a record that can name the session. Used to stop the
/// backwards walk as soon as the rename is in hand.
fn carries_a_title(chunk: &str) -> bool {
    chunk.contains("\"title_change\"")
        || chunk.contains("\"session_info\"")
        || chunk.contains("\"custom-title\"")
}

/// The text a title is read from. Titles live in the first records (journal headers,
/// ai-title) or the last ones (Pi/omp renames, Claude custom-title), so
/// huge transcripts — Claude sessions reach hundreds of megabytes — are only windowed.
/// Reading every file whole on each TUI refresh is what made the interface lag.
pub fn read_title_text(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len <= TITLE_FULL_MAX {
        let mut text = String::new();
        f.read_to_string(&mut text).ok()?;
        return Some(text);
    }
    let mut head = vec![0u8; TITLE_WINDOW as usize];
    let n = f.read(&mut head).ok()?;
    head.truncate(n);
    // Keep whole lines: drop the partial last one (its JSON would not parse anyway).
    match head.iter().rposition(|&b| b == b'\n') {
        Some(i) => head.truncate(i + 1),
        None => head.push(b'\n'),
    }
    // Walk the tail backwards until a record that can name the session shows up. A rename made
    // inside the harness leaves only a `title_change` (the slot lags until it saves), and in a
    // long journal that record sits far from both ends: one 64 KiB window missed it, the caller
    // fell back to the stale slot, and a rename could be silently overwritten by the name shown.
    let mut tail = String::new();
    let mut end = len;
    let mut scanned: u64 = 0;
    loop {
        let span = TITLE_WINDOW.min(end);
        let start = end - span;
        let mut buf = vec![0u8; span as usize];
        f.seek(SeekFrom::Start(start)).ok()?;
        f.read_exact(&mut buf).ok()?;
        let mut chunk = String::from_utf8_lossy(&buf).into_owned();
        // Drop the partial first line unless this chunk reaches the start of the file: its JSON
        // would not parse, and the window before it carries that line whole.
        if start > 0 {
            if let Some(i) = chunk.find('\n') {
                chunk.drain(..=i);
            }
        }
        let found = carries_a_title(&chunk);
        tail = format!("{chunk}{tail}");
        scanned += span;
        if found || start == 0 || scanned >= TITLE_TAIL_MAX {
            break;
        }
        end = start;
    }
    let mut text = String::from_utf8_lossy(&head).into_owned();
    text.push_str(&tail);
    Some(text)
}

/// The title a journal shows, read from a window of the file (`read_title_text`) by the
/// harness's own rule. "" when it has none.
pub fn title(path: &Path, from_lines: fn(&[&str]) -> (String, bool)) -> String {
    read_title_text(path)
        .map(|text| {
            let lines: Vec<&str> = text.lines().collect();
            from_lines(&lines).0
        })
        .unwrap_or_default()
}

/// Text of a user message's content: a string, or the first text block of a list.
pub(super) fn user_text(msg: &Value) -> Option<String> {
    let content = msg.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    content.as_array()?.iter().find_map(|b| {
        (b.get("type")?.as_str()? == "text").then(|| b.get("text")?.as_str().map(str::to_string)).flatten()
    })
}

/// (title, _) for an omp/Pi journal. The title record (omp's slot) or the last
/// `title_change` (a rename made inside omp; omp overlays it over the slot on
/// read, and the slot can lag behind the rename until omp saves again) wins over
/// the header, which can hold an older name. Falls back to the first user
/// message. Never the filename.
pub fn omp_title_from_lines(lines: &[&str]) -> (String, bool) {
    let (mut header, mut slot, mut fallback) = (String::new(), String::new(), String::new());
    for line in lines {
        let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
        match rec.get("type").and_then(Value::as_str).unwrap_or("") {
            "session" => {
                if let Some(t) = rec.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    header = t.to_string();
                }
            }
            "title" | "title_change" => {
                if let Some(t) = rec.get("title").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) {
                    slot = t.to_string();
                }
            }
            "session_info" => {
                if let Some(t) = rec.get("name").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) {
                    slot = t.to_string();
                }
            }
            "message" if fallback.is_empty() => {
                if let Some(m) = rec.get("message") {
                    if m.get("role").and_then(Value::as_str) == Some("user") {
                        if let Some(text) = user_text(m) {
                            fallback = text.trim().split('\n').next().unwrap_or("").chars().take(80).collect();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let title = [slot, header, fallback].into_iter().find(|t| !t.is_empty()).unwrap_or_default();
    (title, true)
}

/// The newest record `prompt_time` reads as a user prompt, in seconds since the epoch. Records
/// are appended in order, so the file is read from the end and the scan stops at the first
/// prompt. None when there is none or the file cannot be read.
pub fn newest_prompt(path: &Path, prompt_time: impl Fn(&serde_json::Value) -> Option<f64>) -> Option<f64> {
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // Cheap filter before parsing: every user record spells its role or type as "user".
    for line in text.lines().rev() {
        if !line.contains("\"user\"") {
            continue;
        }
        let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if let Some(t) = prompt_time(&r) {
            return Some(t);
        }
    }
    None
}

/// A user prompt in the omp/Pi journal shape: a `message` record whose role is `user` and whose
/// content someone typed. Its time, or None.
pub fn message_prompt_time(r: &serde_json::Value) -> Option<f64> {
    let is_prompt = r.get("type").and_then(serde_json::Value::as_str) == Some("message")
        && r.pointer("/message/role").and_then(serde_json::Value::as_str) == Some("user")
        && r.pointer("/message/content").is_some_and(crate::prompts::is_prompt_content);
    if is_prompt { record_time(r) } else { None }
}

/// A record's `timestamp`, in seconds since the epoch.
pub fn record_time(r: &serde_json::Value) -> Option<f64> {
    r.get("timestamp").and_then(serde_json::Value::as_str).and_then(crate::prompts::parse_rfc3339)
}

/// The folder a copy sits in: its project bucket.
pub fn parent_name(path: &str) -> Option<String> {
    Path::new(path).parent()?.file_name().map(|n| n.to_string_lossy().to_string())
}

/// `<timestamp>_<uuid>.jsonl`, the omp and Pi journal name.
/// Whether the harness will list a session file of this name. An import must only write names
/// the harness reads, or the file would be written and never shown.
pub fn is_listed_session_name(name: &str) -> bool {
    parse_omp_name(name).is_some()
}

pub fn parse_omp_name(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".jsonl")?;
    let (ts, id) = stem.split_once('_')?;
    if !is_timestamp(ts) || !is_uuid(id) {
        return None;
    }
    Some(id.to_string())
}

/// `2026-10-01T10-00-00-000Z`: digits and dashes in the exact places, then Z.
fn is_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 24
        && s.ends_with('Z')
        && b[10] == b'T'
        && [4, 7, 13, 16, 19].iter().all(|&i| b[i] == b'-')
        && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18, 20, 21, 22]
            .iter()
            .all(|&i| b[i].is_ascii_digit())
}

/// 8-4-4-4-12 lowercase hex.
pub(super) fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    parts.len() == 5
        && parts.iter().zip(lens).all(|(p, n)| p.len() == n && p.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()))
}

/// Files one directory below `root`, i.e. the Python glob `*/*.jsonl`.
pub(super) fn one_level_down(root: &Path, keep: fn(&str) -> bool) -> Vec<PathBuf> {
    let Ok(dirs) = fs::read_dir(root) else { return Vec::new() };
    let mut out = Vec::new();
    for d in dirs.flatten().filter(|d| d.path().is_dir()) {
        let Ok(files) = fs::read_dir(d.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if p.is_file() && keep(name) {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

/// omp or Pi session files under `<agent_dir>/sessions/*/`.
pub fn jsonl_sessions(agent_dir: &Path) -> Vec<PathBuf> {
    one_level_down(&agent_dir.join("sessions"), |n| parse_omp_name(n).is_some())
}

/// Each path separator becomes '-', the drive colon too; every other character is kept. The
/// converter (`sesync-conv/src/journal.rs`) is the single writer of copies, and names their
/// project folders by this rule: this copy exists only so a copy in the wrong folder is noticed.
pub fn seg_join(path: &str) -> String {
    path.chars().map(|c| if matches!(c, '\\' | '/' | ':') { '-' } else { c }).collect()
}

/// The first cwd a journal records, read from a 64 KiB head window: omp and Pi journals carry it
/// on the `session` record, Claude transcripts on most records. None for an unreadable file.
pub fn cwd_from_head(path: &Path) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 64 * 1024];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|v| v.get("cwd").and_then(|c| c.as_str()).map(str::to_string))
}

/// Convert a canonical journal with `sesync-conv <verb>`, with a deterministic copy id, and place
/// the result atomically under `<store>/<sub>/<bucket>/`, replacing an older copy with the same
/// id whatever its timestamp prefix. Returns the installed path. Any converter failure is an
/// error, and nothing is left in the store.
#[allow(clippy::too_many_arguments)]
pub fn install(
    sesync_conv: &Path,
    src: &Path,
    verb: &str,
    sub: &str,
    store: &Path,
    extra: &[String],
    mirror_id: &str,
    title: &str,
) -> Result<PathBuf, String> {
    let scratch = std::env::temp_dir().join(format!("hs-install-scratch-{}-{}", std::process::id(), mirror_id));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let out_dir = scratch.join(sub);
    let mut cmd = Command::new(sesync_conv);
    cmd.arg(verb).arg(src).arg("--out-dir").arg(&out_dir).args(extra);
    // The mirror id names the file; --title carries the conversation name (to-claude writes it
    // as the custom-title record Claude labels the session by).
    cmd.args(["--id", mirror_id, "--title", title]);
    let out = cmd.output().map_err(|e| format!("cannot run sesync-conv: {e}"))?;
    if !out.status.success() {
        let _ = fs::remove_dir_all(&scratch);
        return Err(format!("sesync-conv {verb} failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let produced = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let produced = PathBuf::from(produced.lines().last().unwrap_or("").trim());
    if !produced.is_file() {
        let _ = fs::remove_dir_all(&scratch);
        return Err(format!("sesync-conv printed no output file: {}", produced.display()));
    }
    let name = produced.file_name().ok_or("no file name")?.to_owned();
    let bucket = produced.parent().and_then(Path::file_name).ok_or("no bucket")?.to_owned();
    let dest_dir = store.join(sub).join(&bucket);
    fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
    if let Ok(rd) = fs::read_dir(&dest_dir) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().ends_with(&format!("{mirror_id}.jsonl")) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    let dest = dest_dir.join(&name);
    let tmp = dest.with_extension("jsonl.tmp");
    fs::copy(&produced, &tmp).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(&scratch);
    Ok(dest)
}

/// Rewrite a journal in place through a temporary file, then check the new title is in it: a
/// rename that leaves the name out must not report success.
pub fn rewrite(path: &Path, title: &str, edit: impl FnOnce(&str) -> String) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("rename.tmp");
    fs::write(&tmp, edit(&text)).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    let check = fs::read_to_string(path).map_err(|e| e.to_string())?;
    if !check.contains(title) {
        return Err(format!("rename did not write the new title into {}", path.display()));
    }
    Ok(())
}

/// A journal's text with one record appended on a line of its own.
pub fn append_record(text: &str, rec: serde_json::Value) -> String {
    let mut out = text.to_string();
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&rec.to_string());
    out.push('\n');
    out
}

/// Convert a harness's own file into a canonical journal with `sesync-conv from-<id>`, written to
/// `<scratch>/canonical.jsonl`.
pub fn convert_from(sesync_conv: &Path, verb: &str, src: &Path, scratch: &Path, what: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let out = scratch.join("canonical.jsonl");
    let status = Command::new(sesync_conv)
        .arg(verb).arg(src).arg("--out").arg(&out)
        .status()
        .map_err(|e| format!("cannot run sesync-conv: {e}"))?;
    if !status.success() {
        return Err(format!("sesync-conv could not convert this {what}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head_file(name: &str, text: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("sesync-journal-{}-{name}.jsonl", std::process::id()));
        fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn the_cwd_is_found_in_a_journal_header_and_a_claude_record() {
        let omp = head_file("omp", "{\"type\":\"title\",\"title\":\"T\"}\n{\"type\":\"session\",\"cwd\":\"/home/u/proj\"}\n");
        assert_eq!(cwd_from_head(&omp).as_deref(), Some("/home/u/proj"));
        let claude = head_file("claude", "{\"type\":\"mode\"}\n{\"type\":\"system\",\"cwd\":\"/home/u/projects\"}\n");
        assert_eq!(cwd_from_head(&claude).as_deref(), Some("/home/u/projects"));
        let none = head_file("none", "{\"type\":\"mode\"}\n");
        assert_eq!(cwd_from_head(&none), None);
        for p in [omp, claude, none] {
            let _ = fs::remove_file(p);
        }
    }
}
