//! The omp / Pi session journal. One JSON record per line.
//!
//! omp and Pi share this format (Pi is omp's upstream engine). Measured facts
//! this module relies on, each verified against real files on 2026-10-12:
//!
//! * An omp file starts with a fixed 256-byte `title` slot (255 bytes of JSON
//!   padded with spaces + newline). A Pi file starts at the `session` header.
//!   Everything after that is identical in shape: records linked by
//!   `id` / `parentId`, the conversation under `type: "message"`.
//! * Pi resumes a raw omp journal unchanged (thinking blocks, omp-only keys,
//!   omp-only record types all tolerated) once images are inline base64
//!   instead of `blob:sha256:` references. omp resumes a raw Pi journal once
//!   the title slot is prepended. So omp<->Pi is a projection, not a
//!   conversion.
//! * omp writes two `bashExecution` records without `content` for `!cmd`
//!   turns; Pi >= 1.1 persists a `system` role record. Neither matters to
//!   the engines themselves; they matter to the OpenCode/Claude writers,
//!   which skip them.

use serde_json::{Map, Value};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub const TITLE_SLOT_BYTES: usize = 256;
pub const BLOB_PREFIX: &str = "blob:sha256:";

#[derive(Debug, Clone)]
pub struct Journal {
    /// The title slot record if the file had one (omp), else None (Pi).
    pub title: Option<Map<String, Value>>,
    /// Every record after the title slot, in file order.
    pub records: Vec<Map<String, Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Omp,
    Pi,
}

impl Journal {
    pub fn read(path: &Path) -> Result<Self, String> {
        let f = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut title = None;
        let mut records = Vec::new();
        for (n, line) in BufReader::new(f).lines().enumerate() {
            let line = line.map_err(|e| format!("{}:{}: {e}", path.display(), n + 1))?;
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(t).map_err(|e| format!("{}:{}: {e}", path.display(), n + 1))?;
            let Value::Object(m) = v else {
                return Err(format!("{}:{}: record is not an object", path.display(), n + 1));
            };
            if n == 0 && m.get("type").and_then(Value::as_str) == Some("title") {
                title = Some(m);
                continue;
            }
            records.push(m);
        }
        if records.is_empty() {
            return Err(format!("{}: empty journal", path.display()));
        }
        Ok(Journal { title, records })
    }

    pub fn flavor(&self) -> Flavor {
        if self.title.is_some() { Flavor::Omp } else { Flavor::Pi }
    }

    pub fn header(&self) -> Option<&Map<String, Value>> {
        self.records.iter().find(|r| r.get("type").and_then(Value::as_str) == Some("session"))
    }

    pub fn session_id(&self) -> Option<&str> {
        self.header().and_then(|h| h.get("id")).and_then(Value::as_str)
    }

    pub fn cwd(&self) -> Option<&str> {
        self.header().and_then(|h| h.get("cwd")).and_then(Value::as_str)
    }

    pub fn timestamp(&self) -> Option<&str> {
        self.header().and_then(|h| h.get("timestamp")).and_then(Value::as_str)
    }

    /// Display title: the last `title_change` (a rename made inside omp; omp
    /// overlays it over the slot on read, and the slot can lag behind until omp
    /// saves again), else the slot (omp), else the last `session_info` name (Pi),
    /// else the header title, else the first user line.
    pub fn title_text(&self) -> String {
        for r in self.records.iter().rev() {
            if r.get("type").and_then(Value::as_str) == Some("title_change") {
                if let Some(n) = r.get("name").or(r.get("title")).and_then(Value::as_str) {
                    if !n.trim().is_empty() {
                        return n.trim().to_string();
                    }
                }
            }
        }
        if let Some(t) = self.title.as_ref().and_then(|t| t.get("title")).and_then(Value::as_str) {
            let t = t.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
        for r in self.records.iter().rev() {
            match r.get("type").and_then(Value::as_str) {
                Some("session_info") => {
                    if let Some(n) = r.get("name").or(r.get("title")).and_then(Value::as_str) {
                        if !n.trim().is_empty() {
                            return n.trim().to_string();
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(t) = self.header().and_then(|h| h.get("title")).and_then(Value::as_str) {
            if !t.trim().is_empty() {
                return t.trim().to_string();
            }
        }
        for m in self.messages() {
            if m.role == "user" {
                if let Some(t) = m.first_text() {
                    return t.lines().next().unwrap_or("").chars().take(80).collect();
                }
            }
        }
        String::new()
    }

    /// Conversation messages on the ACTIVE branch, in order. The active leaf
    /// is the one `leaf_id` names (see [`active_leaf`]), so walking parents
    /// from it and reversing gives exactly what the engine resumes. Inactive
    /// branches are excluded, matching session-migrate's
    /// "opaque:inactive_omp_branch_entry" accounting.
    pub fn messages(&self) -> Vec<Msg<'_>> {
        // A message without its own time takes the session header's time, and
        // only if that is missing too, the current time. Never the epoch: an
        // epoch timestamp shows as 1970 in every harness that displays it.
        let fallback_ms = self
            .timestamp()
            .and_then(parse_iso_ms)
            .unwrap_or_else(|| std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0));
        self.live_records()
            .into_iter()
            .filter(|r| r.get("type").and_then(Value::as_str) == Some("message"))
            .filter_map(|r| Msg::from_record(r, fallback_ms))
            .collect()
    }

    /// The id of the active leaf: the entry the engine resumes from. See [`active_leaf`].
    pub fn leaf_id(&self) -> Option<&str> {
        active_leaf(&self.records)
    }

    /// The records of the live window, oldest first: the active branch cut at the last
    /// compaction. Everything before that was archived into the compaction's summary, and the
    /// engines resume without it - an omp session compacted down to ~30k tokens was exported to
    /// OpenCode as 3.4M tokens because the walk crossed the compaction and kept the archive.
    ///
    /// The compaction record itself stays inside the window, so a target that drops the archive
    /// can still carry the summary that replaced it. Targets that read the journal directly
    /// (`to-pi`, `to-omp`) copy the whole file instead: the format is theirs, and they apply
    /// their own compaction.
    pub fn live_records(&self) -> Vec<&Map<String, Value>> {
        let by_id: std::collections::HashMap<&str, &Map<String, Value>> =
            self.records.iter().filter_map(|r| r.get("id").and_then(Value::as_str).map(|id| (id, r))).collect();
        let mut chain: Vec<&Map<String, Value>> = Vec::new();
        let mut cur = self.leaf_id();
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = cur {
            if !seen.insert(id) {
                break;
            }
            let Some(r) = by_id.get(id) else { break };
            chain.push(r);
            cur = r.get("parentId").and_then(Value::as_str);
        }
        chain.reverse();
        if let Some(cut) = chain.iter().rposition(|r| r.get("type").and_then(Value::as_str) == Some("compaction")) {
            let kept = chain[cut].get("firstKeptEntryId").and_then(Value::as_str);
            let start = kept
                .and_then(|id| chain[..cut].iter().rposition(|r| r.get("id").and_then(Value::as_str) == Some(id)))
                .unwrap_or(cut);
            chain.drain(..start);
        }
        chain
    }

    /// The last compaction in this journal, as `(summary, timestamp)`.
    ///
    /// The turns it archived are outside the live window, so the summary is the only record of
    /// them. The message-based writers carry it through `fit::Window::note`.
    pub fn live_compaction(&self) -> Option<(&str, &str)> {
        let r = self
            .records
            .iter()
            .rev()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("compaction"))?;
        let summary = r.get("summary").and_then(Value::as_str)?;
        let ts = r.get("timestamp").and_then(Value::as_str).unwrap_or("");
        Some((summary, ts))
    }

    /// omp title-slot line: exactly TITLE_SLOT_BYTES-1 bytes of JSON + "\n".
    pub fn title_slot_line(title: &str, updated_at: &str) -> String {
        let mut t = title.to_string();
        loop {
            let mut m = Map::new();
            m.insert("type".into(), "title".into());
            m.insert("v".into(), 1.into());
            m.insert("title".into(), t.clone().into());
            m.insert("source".into(), "auto".into());
            m.insert("updatedAt".into(), updated_at.into());
            m.insert("pad".into(), "".into());
            let base = serde_json::to_string(&m).unwrap();
            if base.len() + 1 <= TITLE_SLOT_BYTES {
                let pad = TITLE_SLOT_BYTES - 1 - base.len();
                m.insert("pad".into(), " ".repeat(pad).into());
                return serde_json::to_string(&m).unwrap();
            }
            // title too long for the slot: trim one char and retry
            t.pop();
        }
    }
}

/// The active leaf of a journal: the record the engine resumes from. Mirrors omp's resolution
/// (`session-entries.ts`: `vX` and `P0`) and Pi's (`SessionManager._buildIndex`, the last entry).
///
/// The leaf is `activeLeafId` when the last entry carries one, else its own `id`. omp writes
/// `activeLeafId` on a record (`model_usage`, for one) when the leaf had moved past the record's
/// `parentId`. If that id names no entry, the last entry whose own leaf is itself is used (`DTe`).
/// A journal whose last entry has no `parentId` key is not a chain, and falls back to the last
/// message record. A `null` `activeLeafId` means no leaf.
///
/// The same rule lives in `sesync/src/harness/journal.rs` (`active_leaf`): change both together.
pub fn active_leaf(records: &[Map<String, Value>]) -> Option<&str> {
    let entries: Vec<&Map<String, Value>> = records
        .iter()
        .filter(|r| r.get("type").and_then(Value::as_str) != Some("session") && r.contains_key("id"))
        .collect();
    let Some(last) = entries.last().copied() else { return last_message_id(records) };
    if !last.contains_key("parentId") {
        return last_message_id(records);
    }
    let u = own_leaf(last)?;
    if Some(u) == id_of(last) || entries.iter().any(|e| id_of(e) == Some(u)) {
        return Some(u);
    }
    entries.iter().rev().find(|e| own_leaf(e) == id_of(e)).copied().and_then(id_of)
}

fn id_of(r: &Map<String, Value>) -> Option<&str> {
    r.get("id").and_then(Value::as_str)
}

/// `activeLeafId` when the record has one (a null or non-string value gives no leaf), else its id.
fn own_leaf(r: &Map<String, Value>) -> Option<&str> {
    match r.get("activeLeafId") {
        Some(v) => v.as_str(),
        None => id_of(r),
    }
}

fn last_message_id(records: &[Map<String, Value>]) -> Option<&str> {
    records.iter().rev().find(|r| r.get("type").and_then(Value::as_str) == Some("message")).and_then(id_of)
}

/// One conversation message with its content blocks.
#[derive(Debug, Clone)]
pub struct Msg<'a> {
    pub record: &'a Map<String, Value>,
    pub id: &'a str,
    pub role: &'a str,
    pub timestamp_ms: i64,
    pub content: Vec<&'a Value>,
    pub message: &'a Map<String, Value>,
}

impl<'a> Msg<'a> {
    fn from_record(r: &'a Map<String, Value>, fallback_ms: i64) -> Option<Self> {
        let message = r.get("message")?.as_object()?;
        let role = message.get("role")?.as_str()?;
        let id = r.get("id")?.as_str()?;
        let ts = message
            .get("timestamp")
            .and_then(Value::as_i64)
            .or_else(|| r.get("timestamp").and_then(Value::as_str).and_then(parse_iso_ms))
            .unwrap_or(fallback_ms);
        let content = match message.get("content") {
            Some(Value::Array(a)) => a.iter().collect(),
            Some(v @ Value::String(_)) => vec![v],
            _ => Vec::new(),
        };
        Some(Msg { record: r, id, role, timestamp_ms: ts, content, message })
    }

    pub fn first_text(&self) -> Option<&'a str> {
        for b in &self.content {
            match b {
                Value::String(s) => return Some(s),
                Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("text") => {
                    if let Some(t) = o.get("text").and_then(Value::as_str) {
                        return Some(t);
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// Parse "2026-07-27T10:35:21.365Z" to epoch milliseconds. Good enough for
/// ordering and display; no timezone other than Z is ever written here.
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, l: usize| s.get(a..a + l)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (num(0, 4)?, num(5, 2)?, num(8, 2)?, num(11, 2)?, num(14, 2)?, num(17, 2)?);
    let ms = if b.len() >= 23 && b[19] == b'.' { num(20, 3).unwrap_or(0) } else { 0 };
    // days from civil (Howard Hinnant)
    let (y2, m2) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((((days * 24 + h) * 60 + mi) * 60 + sec) * 1000 + ms)
}

pub fn iso_from_ms(ms: i64) -> String {
    // inverse of the above, Z only
    let s = ms.div_euclid(1000);
    let msr = ms.rem_euclid(1000);
    let days = s.div_euclid(86400);
    let rem = s.rem_euclid(86400);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60, msr)
}

/// Resolve `blob:sha256:<hex>` image references to inline base64 using the
/// omp blob store next to the journal (`<agent>/blobs/<hex>`). Verifies the
/// digest. Returns the number of images inlined.
pub fn inline_blobs(records: &mut [Map<String, Value>], blobs_dir: &Path) -> Result<usize, String> {
    use sha2::{Digest, Sha256};
    let mut n = 0;
    for r in records.iter_mut() {
        let Some(Value::Object(msg)) = r.get_mut("message") else { continue };
        let Some(Value::Array(content)) = msg.get_mut("content") else { continue };
        for b in content.iter_mut() {
            let Value::Object(o) = b else { continue };
            if o.get("type").and_then(Value::as_str) != Some("image") {
                continue;
            }
            let Some(data) = o.get("data").and_then(Value::as_str) else { continue };
            let Some(hexd) = data.strip_prefix(BLOB_PREFIX) else { continue };
            if hexd.len() != 64 || !hexd.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!("invalid blob reference {data}"));
            }
            let p = blobs_dir.join(hexd);
            let bytes = fs::read(&p).map_err(|e| format!("blob {hexd} unavailable: {e}"))?;
            let got = hex::encode(Sha256::digest(&bytes));
            if got != hexd {
                return Err(format!("blob {hexd} hash mismatch"));
            }
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            o.insert("data".into(), Value::String(b64));
            n += 1;
        }
    }
    Ok(n)
}

/// Each path separator becomes '-', the drive colon too; every other character (spaces, hyphens,
/// underscores) is kept verbatim.
fn seg_join(path: &str) -> String {
    path.chars().map(|c| if matches!(c, '\\' | '/' | ':') { '-' } else { c }).collect()
}

/// Pi's project folder for a cwd, as pi-coding-agent 1.1 names it (`session-manager.ts`,
/// `getDefaultSessionDirPath`): the absolute path without one leading separator, each `/`, `\`
/// and `:` made '-', wrapped in "--" on both sides: `C:\Users\u\proj` is `--C--Users-u-proj--`,
/// `/home/u/proj` is `--home-u-proj--`. Pi lists, continues and resumes a project from that one
/// folder. sesync checks copies against the same rule (`sesync/src/harness/journal.rs`); keep
/// the two in step.
pub fn pi_bucket(cwd: &str) -> String {
    let path = resolved(cwd);
    let rest = path.strip_prefix(['/', '\\']).unwrap_or(&path);
    format!("--{}--", seg_join(rest))
}

/// omp's project folder for a cwd, as oh-my-pi 18.12 names it (`session/session-paths.ts`,
/// `getDefaultSessionDirName`). The temp root is checked first (on Windows it sits inside the
/// home): a cwd in it is `-tmp-<relative>`, the root itself `-tmp`. Then a cwd in the home is
/// `-<relative>`, the home itself `-`. Any other cwd takes Pi's absolute form.
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

pub fn journal_filename(timestamp_iso: &str, id: &str) -> String {
    format!("{}_{}.jsonl", timestamp_iso.replace(':', "-").replace('.', "-"), id)
}

pub fn blobs_dir_for(journal_path: &Path) -> Option<PathBuf> {
    // <agent>/sessions/<bucket>/<file> -> <agent>/blobs
    journal_path.parent()?.parent()?.parent().map(|a| a.join("blobs"))
}

#[cfg(test)]
pub mod fixture {
    use super::*;

    /// One compacted conversation for every target's test: two turns archived by the compaction,
    /// a kept window starting at `firstKeptEntryId`, the compaction with its summary, and one
    /// turn after it. Shared so the harnesses are tested on exactly the same input.
    pub const ARCHIVED_ONE: &str = "archived one";
    pub const ARCHIVED_TWO: &str = "archived two";
    pub const KEPT_ONE: &str = "kept one";
    pub const KEPT_TWO: &str = "kept two";
    pub const NEWEST: &str = "newest";
    pub const SUMMARY: &str = "WHAT CAME BEFORE";

    pub fn compacted_journal(name: &str) -> Journal {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("sesync-conv-{name}-{}-{n}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let msg = |id: &str, parent: &str, t: &str, text: &str| {
            let p = if parent.is_empty() { String::new() } else { format!(r#","parentId":"{parent}""#) };
            format!(
                r#"{{"type":"message","id":"{id}"{p},"timestamp":"{t}","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
            )
        };
        let lines = vec![
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z"}"#.to_string(),
            msg("a1", "", "2026-01-01T00:00:01.000Z", ARCHIVED_ONE),
            msg("a2", "a1", "2026-01-01T00:00:02.000Z", ARCHIVED_TWO),
            msg("k1", "a2", "2026-01-01T00:00:03.000Z", KEPT_ONE),
            msg("k2", "k1", "2026-01-01T00:00:04.000Z", KEPT_TWO),
            format!(
                r#"{{"type":"compaction","id":"c1","parentId":"k2","timestamp":"2026-01-01T00:00:05.000Z","summary":"{SUMMARY}","firstKeptEntryId":"k1"}}"#
            ),
            msg("n1", "c1", "2026-01-01T00:00:06.000Z", NEWEST),
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let j = Journal::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        j
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The folders Pi and omp list a project from, as their own sources compute them
    /// (pi-coding-agent 1.1 `getDefaultSessionDirPath`, oh-my-pi 18.12 `getDefaultSessionDirName`).
    /// A copy written anywhere else is never offered for its project.
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

    /// Everything before the last compaction's kept window was archived into that compaction's
    /// summary, and the engines resume without it. Exporting it anyway is what turned a session
    /// compacted to ~32k tokens into a 3.4M-token OpenCode copy.
    #[test]
    fn messages_stop_at_the_last_compactions_kept_window() {
        let dir = std::env::temp_dir().join(format!("sesync-conv-compact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("j.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let msg = |id: &str, parent: &str, t: &str, text: &str| {
            let p = if parent.is_empty() { String::new() } else { format!(r#","parentId":"{parent}""#) };
            format!(
                r#"{{"type":"message","id":"{id}"{p},"timestamp":"{t}","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
            )
        };
        writeln!(f, r#"{{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z"}}"#).unwrap();
        writeln!(f, "{}", msg("a1", "", "2026-01-01T00:00:01.000Z", "archived one")).unwrap();
        writeln!(f, "{}", msg("a2", "a1", "2026-01-01T00:00:02.000Z", "archived two")).unwrap();
        writeln!(f, "{}", msg("k1", "a2", "2026-01-01T00:00:03.000Z", "kept one")).unwrap();
        writeln!(f, "{}", msg("k2", "k1", "2026-01-01T00:00:04.000Z", "kept two")).unwrap();
        writeln!(
            f,
            r#"{{"type":"compaction","id":"c1","parentId":"k2","timestamp":"2026-01-01T00:00:05.000Z","summary":"what came before","firstKeptEntryId":"k1"}}"#
        )
        .unwrap();
        writeln!(f, "{}", msg("n1", "c1", "2026-01-01T00:00:06.000Z", "newest")).unwrap();
        drop(f);

        let j = Journal::read(&path).unwrap();
        let texts: Vec<&str> = j.messages().iter().map(|m| m.first_text().unwrap_or("")).collect();
        assert_eq!(texts, vec!["kept one", "kept two", "newest"], "archived turns must not be exported");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A journal with no compaction is unchanged: the whole chain is the live context.
    #[test]
    fn without_a_compaction_the_whole_chain_is_exported() {
        let dir = std::env::temp_dir().join(format!("sesync-conv-nocompact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("j.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z"}}"#).unwrap();
        writeln!(f, r#"{{"type":"message","id":"a1","timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"first"}}]}}}}"#).unwrap();
        writeln!(f, r#"{{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-01-01T00:00:02.000Z","message":{{"role":"user","content":[{{"type":"text","text":"second"}}]}}}}"#).unwrap();
        drop(f);
        let j = Journal::read(&path).unwrap();
        let texts: Vec<&str> = j.messages().iter().map(|m| m.first_text().unwrap_or("")).collect();
        assert_eq!(texts, vec!["first", "second"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod leaf_rules {
    use super::*;

    fn records(text: &str) -> Vec<Map<String, Value>> {
        text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        }).collect()
    }

    const PLAIN: &str = concat!(
        r#"{"type":"session","version":3,"id":"s","timestamp":"2026-11-22T10:00:00.000Z","cwd":"/home/u/proj"}"#, "\n",
        r#"{"type":"message","id":"m1","parentId":null,"message":{"role":"user","content":"q1"}}"#, "\n",
        r#"{"type":"message","id":"a1","parentId":"m1","message":{"role":"assistant","content":[{"type":"text","text":"a1"}]}}"#, "\n",
        r#"{"type":"message","id":"m2","parentId":"a1","message":{"role":"user","content":"q2"}}"#, "\n",
        r#"{"type":"message","id":"a2","parentId":"m2","message":{"role":"assistant","content":[{"type":"text","text":"a2"}]}}"#, "\n",
    );

    /// omp's model_usage: written with parentId m2 after the leaf moved to m4.
    const MODEL_USAGE: &str = concat!(
        r#"{"type":"session","version":3,"id":"s","timestamp":"2026-11-22T10:00:00.000Z","cwd":"/home/u/proj"}"#, "\n",
        r#"{"type":"message","id":"m1","parentId":null,"message":{"role":"user","content":"q1"}}"#, "\n",
        r#"{"type":"message","id":"m2","parentId":"m1","message":{"role":"assistant","content":[{"type":"text","text":"a1"}]}}"#, "\n",
        r#"{"type":"message","id":"m3","parentId":"m2","message":{"role":"user","content":"q2"}}"#, "\n",
        r#"{"type":"message","id":"m4","parentId":"m3","message":{"role":"assistant","content":[{"type":"text","text":"a2"}]}}"#, "\n",
        r#"{"type":"model_usage","id":"mu","parentId":"m2","activeLeafId":"m4"}"#, "\n",
    );

    #[test]
    fn the_last_entry_is_the_leaf_unless_it_names_an_active_leaf() {
        assert_eq!(active_leaf(&records(PLAIN)), Some("a2"));
        assert_eq!(active_leaf(&records(MODEL_USAGE)), Some("m4"), "activeLeafId wins, as omp resumes");
    }

    #[test]
    fn an_active_leaf_that_names_no_entry_falls_back_to_the_last_entry_with_its_own_leaf() {
        let text = concat!(
            r#"{"type":"message","id":"m1","parentId":null,"message":{"role":"user","content":"q1"}}"#, "\n",
            r#"{"type":"message","id":"a1","parentId":"m1","message":{"role":"assistant","content":[]}}"#, "\n",
            r#"{"type":"model_usage","id":"mu","parentId":"a1","activeLeafId":"gone"}"#, "\n",
        );
        assert_eq!(active_leaf(&records(text)), Some("a1"), "DTe: the last entry whose leaf is its own id");
    }

    #[test]
    fn a_null_active_leaf_means_no_leaf() {
        let text = concat!(
            r#"{"type":"message","id":"m1","parentId":null,"message":{"role":"user","content":"q1"}}"#, "\n",
            r#"{"type":"model_usage","id":"mu","parentId":"m1","activeLeafId":null}"#, "\n",
        );
        assert_eq!(active_leaf(&records(text)), None);
    }

    #[test]
    fn a_last_entry_without_a_parent_key_falls_back_to_the_last_message() {
        let text = concat!(
            r#"{"type":"message","id":"m1","parentId":null,"message":{"role":"user","content":"q1"}}"#, "\n",
            r#"{"type":"message","id":"a1","parentId":"m1","message":{"role":"assistant","content":[]}}"#, "\n",
            r#"{"type":"title","id":"t1"}"#, "\n",
        );
        assert_eq!(active_leaf(&records(text)), Some("a1"));
    }
}
