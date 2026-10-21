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

/// (title, _) for an omp/Pi journal. The newest name record wins: omp's slot (`title`, stamped
/// `updatedAt`), a rename made inside omp (`title_change`; omp overlays it over the slot on read,
/// and the slot lags until omp saves again) or Pi's `session_info`. An old rename near the head
/// never overrides a slot rewritten since: the head window holds the first renames, and the last
/// one can lie beyond what the tail walk reads, so file order alone would revive a stale name.
/// Records without a time keep file order. Then the header, which can hold an older name, then
/// the first user message. Never the filename.
pub fn omp_title_from_lines(lines: &[&str]) -> (String, bool) {
    let (mut header, mut slot, mut fallback) = (String::new(), String::new(), String::new());
    let mut slot_at = 0.0;
    for line in lines {
        let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
        let kind = rec.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "session" => {
                if let Some(t) = rec.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    header = t.to_string();
                }
            }
            "title" | "title_change" | "session_info" => {
                let field = if kind == "session_info" { "name" } else { "title" };
                if let Some(t) = rec.get(field).and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) {
                    let at = rec.get("updatedAt").or_else(|| rec.get("timestamp"))
                        .and_then(Value::as_str).and_then(crate::prompts::parse_rfc3339).unwrap_or(0.0);
                    if at == 0.0 || slot_at == 0.0 || at >= slot_at {
                        slot = t.to_string();
                        slot_at = at;
                    }
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

/// The first window the preview reads from a conversation's end, and how far back it may grow.
const PREVIEW_WINDOW: u64 = 256 * 1024;
const PREVIEW_TAIL_MAX: u64 = 8 * 1024 * 1024;

/// The last whole lines of a file, in file order: the window from the end doubles until `want`
/// of its lines pass `counts`, it reaches the start, or it reaches `PREVIEW_TAIL_MAX` bytes.
pub(super) fn tail_lines(path: &Path, want: usize, counts: impl Fn(&str) -> bool) -> Result<Vec<String>, String> {
    let mut f = fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    let mut window = PREVIEW_WINDOW.min(len);
    loop {
        let start = len - window;
        let mut buf = Vec::with_capacity(window as usize);
        f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
        (&mut f).take(window).read_to_end(&mut buf).map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&buf);
        // A window that starts mid-file starts mid-line: that first line is not whole.
        let lines: Vec<&str> = text.lines().skip(usize::from(start > 0)).collect();
        if start == 0 || window >= PREVIEW_TAIL_MAX || lines.iter().filter(|l| counts(l)).count() >= want {
            return Ok(lines.into_iter().map(str::to_string).collect());
        }
        window = (window * 2).min(len).min(PREVIEW_TAIL_MAX);
    }
}

/// A message's content as the preview shows it: a string as it is; from a list of blocks, each
/// text block, a tool call as `[tool: name]` and an image as `[image]`, with thinking and tool
/// results left out. The journal and Claude's transcript share the block shapes under different
/// names (`toolCall`, `tool_use`).
pub(super) fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(blocks)) => blocks.iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str)? {
                "text" => b.get("text").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string),
                "toolCall" | "tool_use" => Some(format!("[tool: {}]", b.get("name").and_then(Value::as_str).unwrap_or("?"))),
                "image" => Some("[image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The last `max` turns of a canonical journal, oldest first, read from the file's end: user and
/// assistant messages, with tool results and sesync's own compaction note left out.
pub fn recent_messages(path: &Path, max: usize) -> Result<Vec<super::Message>, String> {
    use super::{Message, Role};
    // An assistant turn spans several records (one per tool round): read well past `max`.
    let lines = tail_lines(path, max * 4, |l| l.contains("\"message\""))?;
    let mut out = Vec::new();
    for line in &lines {
        let Ok(r) = serde_json::from_str::<Value>(line) else { continue };
        if r.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let role = match r.pointer("/message/role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let text = content_text(r.pointer("/message/content"));
        if text.is_empty() || (role == Role::User && text.starts_with(crate::prompts::COMPACTION_NOTE_PREFIX)) {
            continue;
        }
        out.push(Message { role, text, at: record_time(&r).unwrap_or(0.0) });
    }
    Ok(super::last_turns(out, max))
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

/// Each path separator becomes '-', the drive colon too; every other character is kept.
pub fn seg_join(path: &str) -> String {
    path.chars().map(|c| if matches!(c, '\\' | '/' | ':') { '-' } else { c }).collect()
}

/// Pi's project folder for a cwd, as pi-coding-agent 1.1 names it (`session-manager.ts`,
/// `getDefaultSessionDirPath`): the absolute path without one leading separator, each `/`, `\`
/// and `:` made '-', wrapped in "--" on both sides: `C:\Users\u\proj` is `--C--Users-u-proj--`,
/// `/home/u/proj` is `--home-u-proj--`. Pi lists, continues and resumes a project from that one
/// folder, so a copy anywhere else is never offered there. The converter
/// (`sesync-conv/src/journal.rs`) writes copies by the same rule; keep the two in step.
pub fn pi_bucket(cwd: &str) -> String {
    let path = resolved(cwd);
    let rest = path.strip_prefix(['/', '\\']).unwrap_or(&path);
    format!("--{}--", seg_join(rest))
}

/// omp's project folder for a cwd, as oh-my-pi 18.12 names it (`session/session-paths.ts`,
/// `getDefaultSessionDirName`). The temp root is checked first (on Windows it sits inside the
/// home): a cwd in it is `-tmp-<relative>`, the root itself `-tmp`. Then a cwd in the home is
/// `-<relative>`, the home itself `-`. Any other cwd takes Pi's absolute form. The converter
/// writes copies by the same rule; keep the two in step.
pub fn omp_bucket(cwd: &str, home: &str, temp: &str) -> String {
    let path = resolved(cwd);
    if let Some(rel) = relative_within(&path, temp) {
        return relative_bucket("-tmp", &rel);
    }
    if let Some(rel) = relative_within(&path, home) {
        return relative_bucket("-", &rel);
    }
    pi_bucket(&path)
}

/// The temp root as Node's `os.tmpdir()` finds it, the root omp files temp sessions under:
/// `TEMP`, else `TMP` on Windows; `TMPDIR`, `TMP`, `TEMP`, else `/tmp` elsewhere.
pub fn node_tmpdir() -> String {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if cfg!(windows) {
        var("TEMP").or_else(|| var("TMP"))
            .or_else(|| var("SystemRoot").or_else(|| var("windir")).map(|r| format!("{r}\\temp")))
            .unwrap_or_default()
    } else {
        var("TMPDIR").or_else(|| var("TMP")).or_else(|| var("TEMP")).unwrap_or_else(|| "/tmp".to_string())
    }
}

/// A cwd as `path.resolve` leaves it: no trailing separator, except on a root (`/`, `C:\`).
fn resolved(cwd: &str) -> String {
    let t = cwd.trim_end_matches(['\\', '/']);
    if t.is_empty() || t.ends_with(':') {
        return cwd[..(t.len() + 1).min(cwd.len())].to_string();
    }
    t.to_string()
}

/// `path` relative to `root` when it is the root or inside it, without a leading separator.
/// Either separator matches and ASCII case is ignored, as on Windows. None when it is outside,
/// or `root` is empty.
fn relative_within(path: &str, root: &str) -> Option<String> {
    let norm = |s: &str| s.replace('/', "\\");
    let (p, r) = (norm(path), norm(root.trim_end_matches(['\\', '/'])));
    if r.is_empty() || !p.get(..r.len())?.eq_ignore_ascii_case(&r) {
        return None;
    }
    match &p[r.len()..] {
        "" => Some(String::new()),
        rest => rest.strip_prefix('\\').map(str::to_string),
    }
}

/// omp's `encodeRelativeSessionDirName`: the prefix alone for the root itself, else the prefix
/// and the relative path with its separators made '-', joined by one '-'.
fn relative_bucket(prefix: &str, rel: &str) -> String {
    let enc = seg_join(rel);
    if enc.is_empty() {
        prefix.to_string()
    } else if prefix.ends_with('-') {
        format!("{prefix}{enc}")
    } else {
        format!("{prefix}-{enc}")
    }
}

/// The file name omp and Pi give a session: its start time with `:` and `.` made '-', then its
/// id (`2026-10-01T10-00-00-000Z_<uuid>.jsonl`).
pub fn journal_filename(timestamp_iso: &str, id: &str) -> String {
    format!("{}_{}.jsonl", timestamp_iso.replace([':', '.'], "-"), id)
}

/// Where and when a journal's conversation started, from its `session` record.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionHead {
    pub cwd: Option<String>,
    pub timestamp: Option<String>,
}

/// The `session` record of journal bytes, read from the first 64 KiB: past an omp title slot,
/// which is a line of its own. Empty fields when there is none.
pub fn session_head(bytes: &[u8]) -> SessionHead {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(64 * 1024)]);
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if v.get("type").and_then(Value::as_str) == Some("session") {
            let field = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
            return SessionHead { cwd: field("cwd"), timestamp: field("timestamp") };
        }
    }
    SessionHead::default()
}

/// UTC now as `2026-10-18T17:01:16.266Z`, the shape omp and Pi stamp on entries (days-from-civil
/// inverse; no timezone other than Z is ever written here).
pub fn now_iso_ms() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let (month, year) = if mp < 10 { (mp + 3, y) } else { (mp - 9, y + 1) };
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{mi:02}:{s:02}.{:03}Z", d.subsec_millis())
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
/// `<scratch>/canonical.jsonl`. The converter's output is captured, never inherited: the TUI's
/// preview converts too, and anything printed would land on its screen.
pub fn convert_from(sesync_conv: &Path, verb: &str, src: &Path, scratch: &Path, what: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let out = scratch.join("canonical.jsonl");
    let run = Command::new(sesync_conv)
        .arg(verb).arg(src).arg("--out").arg(&out)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run sesync-conv: {e}"))?;
    if !run.status.success() {
        let why = String::from_utf8_lossy(&run.stderr).trim().to_string();
        return Err(if why.is_empty() {
            format!("sesync-conv could not convert this {what}")
        } else {
            format!("sesync-conv could not convert this {what}: {why}")
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The folders Pi and omp list a project from, as their own sources compute them
    /// (pi-coding-agent 1.1 `getDefaultSessionDirPath`, oh-my-pi 18.12 `getDefaultSessionDirName`).
    /// A copy anywhere else is never offered for its project, so it counts as misplaced.
    #[test]
    fn project_folders_follow_the_harness_rules() {
        assert_eq!(pi_bucket("C:\\Users\\u\\proj"), "--C--Users-u-proj--");
        assert_eq!(pi_bucket("C:\\Users\\u\\proj\\"), "--C--Users-u-proj--");
        assert_eq!(pi_bucket("/home/u/proj"), "--home-u-proj--");
        assert_eq!(pi_bucket("/"), "----");
        assert_eq!(pi_bucket("C:\\"), "--C----");
        let (home, temp) = ("C:\\Users\\u", "C:\\Users\\u\\AppData\\Local\\Temp");
        assert_eq!(omp_bucket("C:\\Users\\u\\Projetos\\PC CONFIG", home, temp), "-Projetos-PC CONFIG");
        assert_eq!(omp_bucket("c:/users/u/projetos", home, temp), "-projetos");
        assert_eq!(omp_bucket(home, home, temp), "-");
        assert_eq!(omp_bucket("C:\\Users\\u\\AppData\\Local\\Temp\\x\\y", home, temp), "-tmp-x-y",
            "the temp root wins over the home it sits in");
        assert_eq!(omp_bucket(temp, home, temp), "-tmp");
        assert_eq!(omp_bucket("C:\\tmp", home, temp), "--C--tmp--");
        assert_eq!(omp_bucket("C:\\Users\\uv", home, temp), "--C--Users-uv--",
            "a name that only starts like the home is outside it");
        assert_eq!(omp_bucket("/home/u/proj", "/home/u", "/tmp"), "-proj");
        assert_eq!(omp_bucket("/tmp/x", "/home/u", "/tmp"), "-tmp-x");
        assert_eq!(omp_bucket("/srv/x", "/home/u", "/tmp"), "--srv-x--");
    }

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
