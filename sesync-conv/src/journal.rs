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
    /// is the last message record in the file (the engines append in order
    /// and a branch switch re-roots later records on the chosen parent), so
    /// walking parents from it and reversing gives exactly what the engine
    /// resumes. Inactive branches are excluded, matching session-migrate's
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
        let by_id: std::collections::HashMap<&str, &Map<String, Value>> =
            self.records.iter().filter_map(|r| r.get("id").and_then(Value::as_str).map(|id| (id, r))).collect();
        let leaf = self
            .records
            .iter()
            .rev()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("message"))
            .and_then(|r| r.get("id").and_then(Value::as_str));
        let mut chain: Vec<&Map<String, Value>> = Vec::new();
        let mut cur = leaf;
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
        chain
            .into_iter()
            .filter(|r| r.get("type").and_then(Value::as_str) == Some("message"))
            .filter_map(|r| Msg::from_record(r, fallback_ms))
            .collect()
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

/// Bucket directory name, same rule in both engines, verified against every
/// bucket on this box (2026-10-12):
///   <drive>:\<home>\<project> -> --C--<home>-<project>--   (pi: absolute)
///   same cwd                                -> -<project>                             (omp: home-relative)
///   C:\scratch (outside home)                   -> --C--tmp--                                       (omp falls back to absolute)
///   home itself                             -> -                                                (omp)
/// Rule: each path separator becomes '-', the drive colon becomes '-' as
/// well (so "C:\" is "C--"), every other character (spaces, hyphens,
/// underscores) is kept verbatim. Absolute form is wrapped in a leading "-"
/// and trailing "-". Home-relative form is the relative part with '-' for
/// separators and a leading '-'.
fn seg_join(path: &str) -> String {
    // "<drive>:\<home>\x" -> "C--<home>-x"; "/tmp/x" -> "-tmp-x"
    let mut out = String::new();
    let mut prev_sep = false;
    for ch in path.chars() {
        match ch {
            '\\' | '/' | ':' => {
                out.push('-');
                prev_sep = true;
            }
            c => {
                out.push(c);
                prev_sep = false;
            }
        }
    }
    let _ = prev_sep;
    out
}

pub fn pi_bucket(cwd: &str) -> String {
    let core = seg_join(cwd.trim_end_matches(['\\', '/']));
    format!("-{core}-")
}

pub fn omp_bucket(cwd: &str, home: &str) -> String {
    let norm = |s: &str| s.replace('/', "\\");
    let (c, h) = (norm(cwd.trim_end_matches(['\\', '/'])), norm(home.trim_end_matches(['\\', '/'])));
    if c.len() >= h.len() && c[..h.len()].eq_ignore_ascii_case(&h) && (c.len() == h.len() || c.as_bytes()[h.len()] == b'\\') {
        let rel = &c[h.len()..];
        if rel.is_empty() {
            return "-".to_string();
        }
        return seg_join(rel); // rel starts with '\' -> leading '-'
    }
    pi_bucket(&c)
}

pub fn journal_filename(timestamp_iso: &str, id: &str) -> String {
    format!("{}_{}.jsonl", timestamp_iso.replace(':', "-").replace('.', "-"), id)
}

pub fn blobs_dir_for(journal_path: &Path) -> Option<PathBuf> {
    // <agent>/sessions/<bucket>/<file> -> <agent>/blobs
    journal_path.parent()?.parent()?.parent().map(|a| a.join("blobs"))
}
