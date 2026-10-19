//! O-penCode as a source. Ported from daemon/sesync.py opencode_sessions and
//! opencode_first_prompt. The store is opened read-only; the reader never writes.

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::Path;

pub struct Session {
    pub id: String,
    /// Last update, in seconds since the epoch (the store keeps milliseconds).
    pub updated: f64,
    pub directory: String,
    pub title: String,
}

/// Open read-only. Returns None when the database does not exist; it is never created.
fn open_ro(db: &Path) -> Option<Connection> {
    if !db.exists() {
        return None;
    }
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()
}

/// Live (not archived) root sessions. A session with a parent_id is a subagent run
/// that belongs to its parent, so it is not a conversation of its own and is left out.
/// Stores without a parent_id column (older OpenCode versions) are read as before.
pub fn sessions(db: &Path) -> Vec<Session> {
    let Some(con) = open_ro(db) else { return Vec::new() };
    let has_parent = con
        .prepare("select 1 from pragma_table_info('session') where name = 'parent_id'")
        .and_then(|mut s| s.exists([]))
        .unwrap_or(false);
    let sql = if has_parent {
        "select id, time_updated, directory, title from session \
         where time_archived is null and parent_id is null"
    } else {
        "select id, time_updated, directory, title from session where time_archived is null"
    };
    let Ok(mut stmt) = con.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok(Session {
            id: r.get::<_, String>(0)?,
            updated: r.get::<_, i64>(1)? as f64 / 1000.0,
            directory: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            title: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        })
    });
    match rows {
        Ok(it) => it.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// One row's last update, in seconds. 0.0 when the store or the row is unreadable.
pub fn row_updated(db: &Path, session_id: &str) -> Option<f64> {
    sessions(db).into_iter().find(|s| s.id == session_id).map(|s| s.updated)
}

/// The first real user prompt of a session: its first line, at most 80 characters.
/// Text that starts with '<' is a command wrapper, not a prompt. Empty when none.
pub fn first_prompt(db: &Path, session_id: &str) -> String {
    let Some(con) = open_ro(db) else { return String::new() };
    let Ok(mut stmt) = con.prepare(
        "select m.data, p.data from message m join part p on p.message_id = m.id \
         where m.session_id = ?1 order by m.time_created, p.time_created",
    ) else {
        return String::new();
    };
    let Ok(rows) = stmt.query_map([session_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) else {
        return String::new();
    };
    for (msg_json, part_json) in rows.flatten() {
        let (Ok(msg), Ok(part)) = (serde_json::from_str::<Value>(&msg_json), serde_json::from_str::<Value>(&part_json)) else {
            continue;
        };
        if msg.get("role").and_then(Value::as_str) != Some("user")
            || part.get("type").and_then(Value::as_str) != Some("text")
        {
            continue;
        }
        let text = part.get("text").and_then(Value::as_str).unwrap_or("").trim();
        if !text.is_empty() && !text.starts_with('<') {
            return text.split('\n').next().unwrap_or("").chars().take(80).collect();
        }
    }
    String::new()
}

/// Test hook: attempt a write through a connection opened the way the reader opens
/// its store. Returns the error the database gives, which must be an error.
pub fn write_probe(db: &Path) -> Result<(), String> {
    let con = open_ro(db).ok_or("no database")?;
    con.execute("insert into session (id, directory, title, time_updated) values ('x','/','x',0)", [])
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Export a session with the opencode CLI and convert it to an omp-format journal
/// with `sesync-conv from-opencode`, under `scratch`. The export is kept as-is because
/// the CLI can print a banner before the JSON; the converter skips anything before
/// the first '{'. A failed export or conversion is an error.
pub fn export_and_convert(
    opencode: &Path,
    sesync_conv: &Path,
    session_id: &str,
    scratch: &Path,
) -> Result<std::path::PathBuf, String> {
    use std::process::Command;
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let export = scratch.join(format!("{session_id}.export.json"));
    let journal = scratch.join(format!("{session_id}.journal.jsonl"));

    // Stdout goes straight to the export file, not a pipe. With a pipe the CLI
    // stopped after 196608 bytes and still exited 0, which truncated a 2.7 MB session.
    let file = std::fs::File::create(&export).map_err(|e| e.to_string())?;
    let out = Command::new(opencode)
        .args(["export", session_id])
        .stdout(file)
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("cannot run opencode export: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&export);
        return Err(format!("opencode export failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }

    let conv = Command::new(sesync_conv)
        .args(["from-opencode"])
        .arg(&export)
        .arg("--out")
        .arg(&journal)
        .output()
        .map_err(|e| format!("cannot run sesync-conv: {e}"))?;
    let _ = std::fs::remove_file(&export);
    if !conv.status.success() {
        let _ = std::fs::remove_file(&journal);
        return Err(format!("sesync_conv from-opencode failed: {}", String::from_utf8_lossy(&conv.stderr).trim()));
    }
    Ok(journal)
}

/// O-penCode names an untitled session "New session - <ISO timestamp>". That is not a
/// name, so it is treated as untitled and the caller falls back to the first prompt.
pub fn clean_title(title: &str) -> String {
    let t = title.trim();
    let is_default = t.strip_prefix("New session - ").map_or(false, |rest| {
        rest.len() >= 20 && rest.ends_with('Z') && rest.as_bytes()[4] == b'-' && rest.as_bytes()[10] == b'T'
    });
    if is_default { String::new() } else { t.to_string() }
}
