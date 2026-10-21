//! The Antigravity CLI (`agy`) store. sesync never runs agy and never writes this store itself:
//! copies are written and retitled by `sesync-conv`. What lives here is what the daemon and the
//! TUI read on every pass (where the store is, its conversations, a conversation's title, and when
//! the user last typed into it) and the converter call that writes a copy.
//!
//! Layout (agy 1.4.4): `<root>/conversations/<id>.db` is one SQLite database per conversation, in
//! WAL mode, so the steps agy appends live in `<id>.db-wal` until agy exits; `<root>/annotations/
//! <id>.pbtxt` holds the title in protobuf text format; `<root>/conversation_summaries.db` is
//! agy's own cache and is only ever read here. Reading must never create `-wal`/`-shm` files next
//! to agy's: the daemon fingerprints `.db` plus `.db-wal`, and a read that leaves files behind
//! would look like a change.

use rusqlite::{Connection, OpenFlags};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// The store root: `ANTIGRAVITY_CLI_HOME` when set, else `<home>/.gemini/antigravity-cli`.
pub fn store_root(home: &Path) -> PathBuf {
    std::env::var_os("ANTIGRAVITY_CLI_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".gemini").join("antigravity-cli"))
}

/// The agy executable this machine would run, used only to decide whether the harness is
/// installed (sesync never starts it): `AGY_BIN`, else `%LOCALAPPDATA%\agy\bin\agy.exe` on
/// Windows when it exists, else the bare `agy` looked up on PATH.
pub fn agy_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("AGY_BIN").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            let p = PathBuf::from(local).join("agy").join("bin").join("agy.exe");
            if p.is_file() {
                return p;
            }
        }
    }
    PathBuf::from("agy")
}

/// Antigravity CLI conversations: `<root>/conversations/<id>.db`, one SQLite file each. agy's
/// own siblings are not conversations: the `-wal`/`-shm` files of a live database, and the
/// `.tmp` files a writer builds before it publishes (`<id>.db.tmp`, `<id>.tmp`).
pub fn antigravity_sessions(root: &Path) -> Vec<PathBuf> {
    let Ok(files) = fs::read_dir(root.join("conversations")) else { return Vec::new() };
    let mut out: Vec<PathBuf> = files
        .flatten()
        .map(|f| f.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            p.is_file() && name.ends_with(".db") && name.len() > 3
        })
        .collect();
    out.sort();
    out
}

/// Install an omp/Pi journal as an Antigravity CLI conversation. `sesync-conv to-antigravity`
/// builds `<store>/conversations/<id>.db` aside and publishes it atomically, replacing an existing
/// one in place, and writes the title to `annotations/<id>.pbtxt`. agy lists the new file at its
/// next start; sesync never touches agy's summary caches beyond what the converter does. When agy
/// holds the conversation open the converter refuses and leaves it intact: that error says "in
/// use" and is a normal retry. Returns the installed `.db`, taken from the last printed line.
pub fn install_antigravity(
    sesync_conv: &Path,
    src: &Path,
    store: &Path,
    mirror_id: &str,
    title: &str,
) -> Result<PathBuf, String> {
    let mut cmd = Command::new(sesync_conv);
    cmd.arg("to-antigravity").arg(src).arg("--store").arg(store).args(["--id", mirror_id]);
    // "untitled" is the scan's placeholder, not a name: without --title the converter takes the
    // journal's own title, and agy names an untitled conversation by its first prompt.
    let title = title.trim();
    if !title.is_empty() && title != "untitled" {
        cmd.args(["--title", title]);
    }
    let out = cmd.output().map_err(|e| format!("cannot run sesync-conv: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if err.contains("in use") {
            return Err(format!("Antigravity has this conversation open; will retry ({err})"));
        }
        return Err(format!("sesync-conv to-antigravity failed: {err}"));
    }
    let produced = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let produced = PathBuf::from(produced.lines().last().unwrap_or("").trim());
    if !produced.is_file() {
        return Err(format!("sesync-conv printed no output file: {}", produced.display()));
    }
    Ok(produced)
}

/// The store a conversation file belongs to: `<root>/conversations/<id>.db` -> `<root>`.
pub fn store_of(db: &Path) -> Option<PathBuf> {
    db.parent()?.parent().map(Path::to_path_buf)
}

/// The conversation id of a store file: its stem.
pub fn conversation_id_of(db: &Path) -> String {
    db.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// `<root>/annotations/<id>.pbtxt`, where agy keeps the title.
pub fn annotation_path(store: &Path, id: &str) -> PathBuf {
    store.join("annotations").join(format!("{id}.pbtxt"))
}

/// `<file><suffix>`, e.g. `x.db` + `-wal` = `x.db-wal`.
pub fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

// ------------------------------------------------------------------ titles

/// Decode a protobuf text-format string literal starting at its opening quote. Escapes
/// (`\n`, `\"`, `\\`, octal `\303`, hex `\xC3`) are bytes, joined as UTF-8.
fn pbtxt_unquote(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let q = *b.first()?;
    if q != b'"' && q != b'\'' {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < b.len() {
        let c = b[i];
        if c == q {
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        let e = *b.get(i + 1)?;
        i += 2;
        match e {
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'v' => out.push(11),
            b'0'..=b'7' => {
                let mut v = u32::from(e - b'0');
                for _ in 0..2 {
                    match b.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            v = v * 8 + u32::from(d - b'0');
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(v as u8);
            }
            b'x' => {
                let mut v = 0u32;
                for _ in 0..2 {
                    match b.get(i).and_then(|d| (*d as char).to_digit(16)) {
                        Some(d) => {
                            v = v * 16 + d;
                            i += 1;
                        }
                        None => break,
                    }
                }
                out.push(v as u8);
            }
            other => out.push(other),
        }
    }
    None
}

/// The non-empty `title:` value of an annotations file. Other fields (agy's App writes
/// `last_user_view_time:{...}`) are skipped; a field merely ending in `title` is not it.
pub fn annotation_title(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(p) = rest.find("title") {
        let prev = if p == 0 { None } else { Some(rest.as_bytes()[p - 1]) };
        let starts_field = prev.map_or(true, |c| !c.is_ascii_alphanumeric() && c != b'_');
        if starts_field {
            if let Some(v) = rest[p + 5..].trim_start().strip_prefix(':') {
                if let Some(t) = pbtxt_unquote(v.trim_start()) {
                    return Some(t).filter(|t| !t.trim().is_empty());
                }
            }
        }
        rest = &rest[p + 5..];
    }
    None
}

/// A conversation's title: the annotations file, else the title agy cached in
/// `conversation_summaries.db` (read only), else empty.
pub fn title(db: &Path) -> String {
    let id = conversation_id_of(db);
    let Some(store) = store_of(db) else { return String::new() };
    if let Some(t) = std::fs::read_to_string(annotation_path(&store, &id)).ok().as_deref().and_then(annotation_title) {
        return t;
    }
    let summaries = store.join("conversation_summaries.db");
    if !summaries.is_file() {
        return String::new();
    }
    read_only(&summaries, |c| {
        c.query_row("SELECT title FROM conversation_summaries WHERE conversation_id = ?1", [&id], |r| r.get::<_, String>(0))
            .ok()
    })
    .flatten()
    .map(|t| t.trim().to_string())
    .unwrap_or_default()
}

/// The newest time any file of a conversation's title changed: the database, its WAL and the
/// annotations file. The TUI re-reads a title only when this (or the size) moves.
pub fn title_stamp(db: &Path) -> Option<(u64, u128)> {
    let main = std::fs::metadata(db).ok()?;
    let mut len = main.len();
    let mut ns = nanos(&main);
    let mut others = vec![with_suffix(db, "-wal")];
    if let Some(store) = store_of(db) {
        others.push(annotation_path(&store, &conversation_id_of(db)));
    }
    for p in others {
        if let Ok(m) = std::fs::metadata(p) {
            len += m.len();
            ns = ns.max(nanos(&m));
        }
    }
    Some((len, ns))
}

fn nanos(m: &std::fs::Metadata) -> u128 {
    m.modified().ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// ------------------------------------------------------------------ sqlite

/// SQLite URI for a path (forward slashes; `%`, `?`, `#` and space escaped).
fn sqlite_uri(path: &Path, query: &str) -> String {
    let p = path.to_string_lossy().replace('\\', "/");
    let mut s = String::from("file:");
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        s.push_str("///");
    }
    for c in p.chars() {
        match c {
            '%' => s.push_str("%25"),
            '?' => s.push_str("%3f"),
            '#' => s.push_str("%23"),
            ' ' => s.push_str("%20"),
            c => s.push(c),
        }
    }
    s.push('?');
    s.push_str(query);
    s
}

/// Run `read` on a read-only view of an agy database without changing the store:
/// * `-wal` and `-shm` both present (agy running, or it left them): a read-only open, which
///   applies the WAL under SQLite's own locking;
/// * no `-wal`: the file is complete, so it is opened `immutable=1` (a plain read-only open
///   would create an empty `-wal` and `-shm` next to agy's file);
/// * a `-wal` without its `-shm`, or a failed open: the files are copied to scratch and read there.
/// None when the database cannot be read at all.
pub fn read_only<T>(db: &Path, read: impl Fn(&Connection) -> T) -> Option<T> {
    let ro = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let wal = with_suffix(db, "-wal");
    let shm = with_suffix(db, "-shm");
    let direct = if wal.exists() && shm.exists() {
        Connection::open_with_flags(db, ro).ok()
    } else if !wal.exists() {
        Connection::open_with_flags(sqlite_uri(db, "immutable=1"), ro | OpenFlags::SQLITE_OPEN_URI).ok()
    } else {
        None
    };
    if let Some(c) = direct {
        let _ = c.busy_timeout(Duration::from_secs(2));
        // A schema read proves the open is usable; a busy or broken file falls back to a copy.
        if c.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).is_ok() {
            let out = read(&c);
            let _ = c.close();
            return Some(out);
        }
    }
    static SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("sesync-agy-read-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let out = (|| {
        let copy = dir.join("c.db");
        std::fs::copy(db, &copy).ok()?;
        if wal.is_file() {
            std::fs::copy(&wal, dir.join("c.db-wal")).ok()?;
        }
        let c = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_WRITE).ok()?;
        let _ = c.busy_timeout(Duration::from_secs(2));
        let out = read(&c);
        let _ = c.close();
        Some(out)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

// --------------------------------------------------------------- protobuf

/// One field of a protobuf message: varints and fixed-width values as numbers, length-delimited
/// values as their bytes.
enum Value<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v: u64 = 0;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*i)?;
        *i += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// The fields of one message, in order. None for a malformed message (groups included: agy
/// writes none).
fn fields(b: &[u8]) -> Option<Vec<(u64, Value<'_>)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let key = varint(b, &mut i)?;
        let (field, wire) = (key >> 3, key & 7);
        let value = match wire {
            0 => Value::Int(varint(b, &mut i)?),
            1 => {
                let v = u64::from_le_bytes(b.get(i..i + 8)?.try_into().ok()?);
                i += 8;
                Value::Int(v)
            }
            2 => {
                let n = usize::try_from(varint(b, &mut i)?).ok()?;
                let v = b.get(i..i.checked_add(n)?)?;
                i += n;
                Value::Bytes(v)
            }
            5 => {
                let v = u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?);
                i += 4;
                Value::Int(u64::from(v))
            }
            _ => return None,
        };
        out.push((field, value));
    }
    Some(out)
}

/// The last occurrence of a length-delimited field (protobuf's rule for a repeated singular).
fn bytes_field<'a>(msg: &'a [u8], field: u64) -> Option<&'a [u8]> {
    fields(msg)?.into_iter().rev().find_map(|(f, v)| match v {
        Value::Bytes(b) if f == field => Some(b),
        _ => None,
    })
}

fn int_field(msg: &[u8], field: u64) -> Option<u64> {
    fields(msg)?.into_iter().rev().find_map(|(f, v)| match v {
        Value::Int(n) if f == field => Some(n),
        _ => None,
    })
}

/// A `google.protobuf.Timestamp` {1 seconds, 2 nanos} in seconds since the epoch.
fn timestamp(msg: &[u8]) -> Option<f64> {
    let secs = int_field(msg, 1)? as i64;
    let nanos = int_field(msg, 2).unwrap_or(0) as i64;
    Some(secs as f64 + nanos as f64 / 1e9)
}

/// The text a USER_INPUT payload carries: field 19.2, else the 19.3 items' text, else 19.1.
fn user_text(payload: &[u8]) -> Option<String> {
    let input = bytes_field(payload, 19)?;
    if let Some(t) = bytes_field(input, 2).filter(|t| !t.is_empty()) {
        return Some(String::from_utf8_lossy(t).into_owned());
    }
    let items: Vec<String> = fields(input)?
        .into_iter()
        .filter_map(|(f, v)| match v {
            Value::Bytes(item) if f == 3 => bytes_field(item, 1).map(|t| String::from_utf8_lossy(t).into_owned()),
            _ => None,
        })
        .collect();
    if !items.is_empty() {
        return Some(items.join("\n"));
    }
    bytes_field(input, 1).map(|t| String::from_utf8_lossy(t).into_owned())
}

/// USER_INPUT, agy's step type for what the user typed.
const STEP_USER_INPUT: i64 = 14;

/// The newest user prompt of a conversation, in seconds since the epoch: the newest USER_INPUT
/// step whose text is typed (not empty, not a wrapper, not sesync's compaction note), timed by
/// its StepMetadata field 1 (the `metadata` column, else payload field 5). None when there is
/// none or the database cannot be read.
pub fn newest_prompt(db: &Path) -> Option<f64> {
    read_only(db, |c| {
        let mut st = c.prepare("SELECT metadata, step_payload FROM steps WHERE step_type = ?1 ORDER BY idx DESC").ok()?;
        let rows = st.query_map([STEP_USER_INPUT], |r| {
            Ok((r.get::<_, Option<Vec<u8>>>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
        }).ok()?;
        for row in rows.flatten() {
            let (meta, payload) = row;
            let Some(payload) = payload else { continue };
            if !user_text(&payload).is_some_and(|t| crate::prompts::is_typed_text(&t)) {
                continue;
            }
            let meta = meta.filter(|m| !m.is_empty()).or_else(|| bytes_field(&payload, 5).map(<[u8]>::to_vec));
            if let Some(at) = meta.as_deref().and_then(|m| bytes_field(m, 1)).and_then(timestamp) {
                return Some(at);
            }
        }
        None
    })
    .flatten()
}

/// What `sesync-conv antigravity-retitle` reported.
#[derive(Debug, PartialEq, Eq)]
pub enum Retitled {
    /// The title is in place and agy's caches were cleared.
    Done,
    /// The title is written, but a running agy holds its cache: agy shows it after a restart.
    AfterRestart,
}

/// Rename a conversation through the converter, which writes `annotations/<id>.pbtxt` and drops
/// agy's cached title. sesync never edits agy's caches itself.
pub fn retitle(sesync_conv: &Path, store: &Path, id: &str, title: &str) -> Result<Retitled, String> {
    let out = std::process::Command::new(sesync_conv)
        .arg("antigravity-retitle")
        .arg("--store").arg(store)
        .args(["--id", id, "--title", title])
        .output()
        .map_err(|e| format!("cannot run sesync-conv: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(Retitled::Done),
        Some(2) => Ok(Retitled::AfterRestart),
        _ => Err(format!("sesync-conv antigravity-retitle failed: {}", String::from_utf8_lossy(&out.stderr).trim())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotation_titles_are_unescaped_and_other_fields_ignored() {
        assert_eq!(annotation_title(r#"title:"Quote \"q\" utf8 acentuação\nline2""#).as_deref(),
            Some("Quote \"q\" utf8 acentuação\nline2"));
        assert_eq!(annotation_title("title:\"a\\303\\247b\"").as_deref(), Some("açb"), "octal escapes are UTF-8 bytes");
        assert_eq!(annotation_title("last_user_view_time:{seconds:1 nanos:2}\ntitle:\"Named\"").as_deref(), Some("Named"));
        assert_eq!(annotation_title("subtitle:\"not it\"").as_deref(), None, "a field ending in title is another field");
        assert_eq!(annotation_title("title:\"\"").as_deref(), None, "an empty title names nothing");
    }

    #[test]
    fn wire_reader_reads_nested_timestamps_and_refuses_truncation() {
        // StepMetadata {1: Timestamp {1: 1_792_368_000, 2: 500_000_000}}
        let mut ts = vec![0x08];
        let mut n: u64 = 1_792_368_000;
        while n >= 0x80 { ts.push((n as u8) | 0x80); n >>= 7; }
        ts.push(n as u8);
        ts.extend([0x10, 0x80, 0xCA, 0xB5, 0xEE, 0x01]);
        let mut meta = vec![0x0A, ts.len() as u8];
        meta.extend(&ts);
        assert_eq!(bytes_field(&meta, 1).and_then(timestamp), Some(1_792_368_000.5));
        assert!(fields(&meta[..meta.len() - 1]).is_none(), "a cut message is malformed, not short");
    }
}
