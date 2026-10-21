//! Antigravity CLI (`agy`, verified on 1.4.4 Windows) store: shared pieces of
//! `to_antigravity` and `from_antigravity`.
//!
//! Store root `%USERPROFILE%\.gemini\antigravity-cli\`:
//!   conversations\<id>.db        one SQLite file per conversation (schema below)
//!   annotations\<id>.pbtxt       protobuf text, `title:"..."` (source of the title)
//!   conversation_summaries.db    picker cache (table conversation_summaries)
//!   jetbox_summaries_proto.pb    primary summary cache: repeated 1 {1 id, 2 Summary{1 title}}
//!
//! Facts this module relies on (agy 1.4.4, measured):
//! * `PRAGMA user_version` must be 1, or agy's first load empties the
//!   trajectory metadata and the conversation loses its workspace.
//! * The workspace is TrajectoryMetadata field 1.1 or 7, a `file:///C:/...` URI
//!   with forward slashes, an uppercase drive and percent-encoded spaces.
//! * A title in annotations survives; a title written into the summary caches
//!   does not (agy rebuilds them), but a stale cache entry hides a new title.

use rusqlite::{Connection, OpenFlags};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const STEP_USER_INPUT: i64 = 14;
pub const STEP_PLANNER_RESPONSE: i64 = 15;
pub const STEP_ERROR_MESSAGE: i64 = 17;
pub const STEP_CHECKPOINT: i64 = 23;
pub const STEP_CONVERSATION_HISTORY: i64 = 98;
pub const STEP_SYSTEM_MESSAGE: i64 = 101;
pub const STEP_GENERIC: i64 = 132;

pub const STATUS_DONE: i64 = 3;
pub const STATUS_INVALID: i64 = 4;
pub const STATUS_ERROR: i64 = 7;

/// StepMetadata.source values.
pub const SOURCE_MODEL: u64 = 2;
pub const SOURCE_USER: u64 = 4;

pub const CLI_SOURCE: i64 = 17;
pub const APP_SOURCE: i64 = 1;
pub const TRAJECTORY_TYPE: i64 = 4;
pub const CLI_PROJECT: &str = "default-cli-project";

/// The exact DDL agy 1.4.4 creates (backticks and all), from `sqlite_master` of a native file.
pub const SCHEMA: &str = "\
CREATE TABLE `trajectory_meta` (`trajectory_id` text,`cascade_id` text,`trajectory_type` integer,`source` integer,PRIMARY KEY (`trajectory_id`));
CREATE TABLE `steps` (`idx` integer,`step_type` integer NOT NULL DEFAULT 0,`status` integer NOT NULL DEFAULT 0,`has_subtrajectory` numeric NOT NULL DEFAULT false,`metadata` blob,`error_details` blob,`permissions` blob,`task_details` blob,`render_info` blob,`step_payload` blob,`step_format` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`));
CREATE INDEX `idx_steps_status` ON `steps`(`status`);
CREATE INDEX `idx_steps_step_type` ON `steps`(`step_type`);
CREATE TABLE `gen_metadata` (`idx` integer,`data` blob,`size` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`));
CREATE TABLE `executor_metadata` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE TABLE `parent_references` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE TABLE `trajectory_metadata_blob` (`id` text DEFAULT \"main\",`data` blob,PRIMARY KEY (`id`));
CREATE TABLE `battle_mode_infos` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
";

/// Columns a reader needs, per table. Extra columns are allowed: the format
/// already grew between 1.1.16 and 1.4.4.
pub const REQUIRED_COLUMNS: &[(&str, &[&str])] = &[
    ("trajectory_meta", &["trajectory_id", "cascade_id", "trajectory_type", "source"]),
    ("steps", &["idx", "step_type", "status", "metadata", "step_payload"]),
    ("trajectory_metadata_blob", &["id", "data"]),
];

/// The store root of a conversation file: `<root>/conversations/<id>.db`.
pub fn store_of(db: &Path) -> Option<PathBuf> {
    db.parent()?.parent().map(Path::to_path_buf)
}

pub fn annotation_path(store: &Path, id: &str) -> PathBuf {
    store.join("annotations").join(format!("{id}.pbtxt"))
}

// ---------------------------------------------------------------- file URIs

fn is_drive_path(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Percent-encode a path for a file URI: unreserved and path-safe ASCII stay,
/// everything else (space, `%`, `#`, `?`, non-ASCII as UTF-8) is encoded.
fn encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for &b in p.as_bytes() {
        let keep = b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&b);
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `C:\Users\me\proj one` -> `file:///C:/Users/me/proj%20one`, the form agy writes.
/// UNC `\\host\share\x` -> `file://host/share/x`; a POSIX path keeps its slashes.
pub fn path_to_file_uri(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = p.trim_end_matches('/');
    if let Some(unc) = p.strip_prefix("//") {
        return format!("file://{}", encode_path(unc));
    }
    if is_drive_path(p) {
        let mut s = p.to_string();
        s[..1].make_ascii_uppercase();
        let s = if s.len() == 2 { format!("{s}/") } else { s };
        return format!("file:///{}", encode_path(&s));
    }
    let p = p.strip_prefix('/').unwrap_or(p);
    format!("file:///{}", encode_path(p))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |i: usize| b.get(i).and_then(|c| (*c as char).to_digit(16));
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let (Some(h), Some(l)) = (hex(i + 1), hex(i + 2)) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Inverse of `path_to_file_uri`, also for the App's `file:///c%3A/...` form:
/// strip the scheme, percent-decode, and use backslashes on a drive or UNC path.
pub fn file_uri_to_path(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_string();
    };
    if let Some(local) = rest.strip_prefix('/') {
        let d = percent_decode(local);
        if is_drive_path(&d) {
            let mut s = d.replace('/', "\\");
            s[..1].make_ascii_uppercase();
            return s;
        }
        return format!("/{d}");
    }
    // file://host/share/... is a UNC path
    format!("\\\\{}", percent_decode(rest).replace('/', "\\"))
}

// --------------------------------------------------- protobuf text (titles)

/// Escape a string for a protobuf text-format double-quoted literal. UTF-8 stays
/// raw (agy reads it back exactly); control bytes become octal.
pub fn pbtxt_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Parse a protobuf text-format string literal starting at the opening quote.
/// Returns the decoded value (octal and hex escapes are bytes, joined as UTF-8).
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
        i += 1;
        let e = *b.get(i)?;
        i += 1;
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
                let mut n = 0;
                while n < 2 {
                    match b.get(i).and_then(|d| (*d as char).to_digit(16)) {
                        Some(d) => {
                            v = v * 16 + d;
                            i += 1;
                            n += 1;
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

/// The `title:` value of an annotations file, if it has a non-empty one.
pub fn parse_annotation_title(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(p) = rest.find("title") {
        let before_ok = p == 0 || !rest.as_bytes()[p - 1].is_ascii_alphanumeric() && rest.as_bytes()[p - 1] != b'_';
        let after = rest[p + 5..].trim_start();
        if before_ok {
            if let Some(v) = after.strip_prefix(':') {
                if let Some(t) = pbtxt_unquote(v.trim_start()) {
                    return Some(t).filter(|t| !t.is_empty());
                }
            }
        }
        rest = &rest[p + 5..];
    }
    None
}

/// New annotations content: our `title:` line plus any existing
/// `last_user_view_time:` lines verbatim. Nothing else: an unknown field
/// (e.g. `pinned:true`) makes agy ignore the whole file.
pub fn annotation_text(title: &str, existing: Option<&str>) -> String {
    let mut out = format!("title:\"{}\"", pbtxt_escape(title));
    for line in existing.unwrap_or("").lines() {
        if line.trim_start().starts_with("last_user_view_time") {
            out.push('\n');
            out.push_str(line);
        }
    }
    out
}

// ------------------------------------------------------------------ sqlite

fn backup_into_memory(src: &Connection) -> Result<Connection, String> {
    let mut dst = Connection::open_in_memory().map_err(|e| e.to_string())?;
    {
        let b = rusqlite::backup::Backup::new(src, &mut dst).map_err(|e| e.to_string())?;
        b.run_to_completion(1024, Duration::from_millis(50), None).map_err(|e| e.to_string())?;
    }
    Ok(dst)
}

/// SQLite URI for `path` (forward slashes, `%`, `?`, `#` and space escaped).
fn sqlite_uri(path: &Path, query: &str) -> String {
    let mut s = String::from("file:");
    let p = path.to_string_lossy().replace('\\', "/");
    if is_drive_path(&p) {
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

fn snapshot_from(open: impl FnOnce() -> rusqlite::Result<Connection>) -> Result<Connection, String> {
    let src = open().map_err(|e| e.to_string())?;
    src.busy_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())?;
    let snap = backup_into_memory(&src)?;
    src.close().map_err(|(_, e)| e.to_string())?;
    Ok(snap)
}

/// A consistent in-memory snapshot of a conversation DB, WAL included, that
/// never changes the store: no checkpoint, and no `-wal`/`-shm` created next to
/// the file (the daemon fingerprints `.db` and `.db-wal`).
/// * `-wal` and `-shm` present (agy running, or left them): read-only open, which
///   applies the WAL under SQLite's own locking.
/// * no `-wal`: the file is complete; read it with `immutable=1` (a plain read-only
///   open would create an empty `-wal` and `-shm`).
/// * `-wal` without `-shm`, or a failed open: copy the files to a scratch dir and read there.
pub fn snapshot(db: &Path) -> Result<Connection, String> {
    if !db.is_file() {
        return Err(format!("{}: no such conversation file", db.display()));
    }
    let ro = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let wal = with_suffix(db, "-wal");
    let shm = with_suffix(db, "-shm");
    let direct = if wal.exists() && shm.exists() {
        snapshot_from(|| Connection::open_with_flags(db, ro))
    } else if !wal.exists() {
        snapshot_from(|| Connection::open_with_flags(sqlite_uri(db, "immutable=1"), ro | OpenFlags::SQLITE_OPEN_URI))
    } else {
        Err("write-ahead log without its index".into())
    };
    let first = match direct {
        Ok(c) => return Ok(c),
        Err(e) => e,
    };
    let dir = std::env::temp_dir().join(format!("sesync-agy-snap-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let res = (|| -> Result<Connection, String> {
        let copy = dir.join("c.db");
        fs::write(&copy, fs::read(db).map_err(|e| format!("{}: {e}", db.display()))?).map_err(|e| e.to_string())?;
        if wal.is_file() {
            let w = fs::read(&wal).map_err(|e| format!("{}: {e}", wal.display()))?;
            fs::write(dir.join("c.db-wal"), w).map_err(|e| e.to_string())?;
        }
        snapshot_from(|| Connection::open(&copy))
    })();
    let _ = fs::remove_dir_all(&dir);
    res.map_err(|e| format!("{}: cannot read ({first}; from a copy: {e})", db.display()))
}

/// `<file><suffix>`, e.g. `x.db` + `-wal` = `x.db-wal`.
pub fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Required tables and columns present (extra columns allowed).
pub fn validate_schema(c: &Connection) -> Result<(), String> {
    for (table, cols) in REQUIRED_COLUMNS {
        let mut st = c.prepare(&format!("PRAGMA table_info(`{table}`)")).map_err(|e| e.to_string())?;
        let have: Vec<String> = st
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .collect();
        if have.is_empty() {
            return Err(format!("not an Antigravity conversation: table {table} is missing"));
        }
        for col in *cols {
            if !have.iter().any(|h| h == col) {
                return Err(format!("not an Antigravity conversation: {table}.{col} is missing"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_uri_matches_what_agy_writes() {
        assert_eq!(
            path_to_file_uri(r"C:\Users\manuesantos\Downloads\agy-research\sb\proj one"),
            "file:///C:/Users/manuesantos/Downloads/agy-research/sb/proj%20one",
            "the exact TM field 7 of a native 1.4.4 conversation"
        );
        assert_eq!(path_to_file_uri(r"c:\Proj\ação #1\"), "file:///C:/Proj/a%C3%A7%C3%A3o%20%231");
        assert_eq!(path_to_file_uri("C:\\"), "file:///C:/");
        assert_eq!(path_to_file_uri(r"\\srv\share\a b"), "file://srv/share/a%20b");
        assert_eq!(path_to_file_uri("/home/x y"), "file:///home/x%20y");
    }

    #[test]
    fn workspace_uri_decodes_cli_and_app_forms() {
        let p = r"C:\Users\me\proj one\ação 50%";
        assert_eq!(file_uri_to_path(&path_to_file_uri(p)), p);
        assert_eq!(file_uri_to_path("file:///c%3A/Users/me/mcp%20test"), r"C:\Users\me\mcp test", "App field 7");
        assert_eq!(file_uri_to_path("file:///c:/Users/me"), r"C:\Users\me", "App field 1");
        assert_eq!(file_uri_to_path("file://srv/share/x"), r"\\srv\share\x");
        assert_eq!(file_uri_to_path("file:///home/x%20y"), "/home/x y");
        assert_eq!(file_uri_to_path("file:///C:/a%2"), r"C:\a%2", "a dangling % is kept");
    }

    #[test]
    fn titles_escape_and_parse_back_like_agy() {
        let t = "Quote \"q\" utf8 acentuação\nline2 back\\slash\ttab";
        let text = annotation_text(t, None);
        assert_eq!(text, r#"title:"Quote \"q\" utf8 acentuação\nline2 back\\slash\ttab""#);
        assert_eq!(parse_annotation_title(&text).as_deref(), Some(t));
        // the file agy itself wrote with octal escapes
        assert_eq!(parse_annotation_title(r#"title:"octal acentua\303\247\303\243o""#).as_deref(), Some("octal acentuação"));
        assert_eq!(parse_annotation_title("last_user_view_time:{seconds:1 nanos:0}"), None);
        assert_eq!(parse_annotation_title(r#"subtitle:"no""#), None, "only the title field");
        assert_eq!(parse_annotation_title(r#"title:"""#), None, "empty title is no title");
    }

    #[test]
    fn retitling_keeps_only_the_view_time_line() {
        let old = "title:\"old\"\nlast_user_view_time:{seconds:1792500000 nanos:0}\npinned:true";
        assert_eq!(annotation_text("new", Some(old)), "title:\"new\"\nlast_user_view_time:{seconds:1792500000 nanos:0}");
    }
}
