//! OpenCode's store and CLI. Sessions are read from its SQLite database, which is opened
//! read-only and never written; a copy is imported, exported and removed only through the
//! `opencode` command.

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// The newest user prompt of a session, in seconds since the epoch. sesync's own compaction note
/// is a user message by shape and is skipped. None when the session has no prompt or the store
/// cannot be read.
pub fn newest_prompt(db: &Path, session_id: &str) -> Option<f64> {
    let con = open_ro(db)?;
    let like = format!("{}%", crate::prompts::COMPACTION_NOTE_PREFIX);
    con.query_row(
        "select max(m.time_created) from message m \
         where m.session_id = ?1 and json_extract(m.data, '$.role') = 'user' \
         and not exists (select 1 from part p where p.message_id = m.id \
             and json_extract(p.data, '$.type') = 'text' and json_extract(p.data, '$.text') like ?2)",
        rusqlite::params![session_id, like],
        |r| r.get::<_, Option<i64>>(0),
    )
    .ok()
    .flatten()
    .map(|ms| ms as f64 / 1000.0)
}

/// What a session holds, cheaply: message and part counts and the newest part update. Recorded
/// when sesync imports a copy, so a later mismatch shows somebody wrote there. None when the
/// session is not in the store.
pub fn content_fingerprint(db: &Path, session_id: &str) -> Option<String> {
    let con = open_ro(db)?;
    let exists: bool = con
        .query_row("select count(*) from session where id = ?1", [session_id], |r| r.get::<_, i64>(0))
        .map(|n| n > 0)
        .unwrap_or(false);
    if !exists {
        return None;
    }
    con.query_row(
        "select (select count(*) from message where session_id = ?1), \
                (select count(*) from part where session_id = ?1), \
                (select coalesce(max(time_updated), 0) from part where session_id = ?1)",
        [session_id],
        |r| Ok(format!("oc:{}:{}:{}", r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
    )
    .ok()
}

/// The directory the store keeps a session under. `opencode session delete` only finds a session
/// from inside its own project, so a delete must run there. None when the row is missing.
pub fn session_directory(db: &Path, session_id: &str) -> Option<String> {
    let con = open_ro(db)?;
    con.query_row("select directory from session where id = ?1", [session_id], |r| r.get::<_, Option<String>>(0))
        .ok()
        .flatten()
        .filter(|d| !d.is_empty())
}

/// Export a session with the CLI into `out`. Stdout goes straight to the file, not a pipe: with a
/// pipe the CLI stopped after 196608 bytes and still exited 0, which truncated a 2.7 MB session.
/// Runs in `dir` when it exists, the session's own project.
pub fn export(opencode: &Path, session_id: &str, dir: Option<&str>, out: &Path) -> Result<(), String> {
    let file = std::fs::File::create(out).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(opencode);
    cmd.args(["export", session_id]).stdout(file).stderr(std::process::Stdio::piped());
    if let Some(d) = dir.filter(|d| Path::new(d).is_dir()) {
        cmd.current_dir(d);
    }
    let run = cmd.output().map_err(|e| format!("cannot run opencode export: {e}"))?;
    if !run.status.success() {
        let _ = std::fs::remove_file(out);
        return Err(format!("opencode export failed: {}", String::from_utf8_lossy(&run.stderr).trim()));
    }
    Ok(())
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

/// The session's last `max` turns, oldest first: text parts as written and tool parts by name;
/// reasoning, synthetic text (OpenCode's own reminders) and sesync's compaction note left out.
/// Read-only, like every read of this store.
pub fn recent_messages(db: &Path, session_id: &str, max: usize) -> Result<Vec<crate::harness::Message>, String> {
    use crate::harness::{Message, Role};
    let con = open_ro(db).ok_or_else(|| format!("cannot open {}", db.display()))?;
    let newest: Vec<(String, i64, String)> = {
        let mut stmt = con.prepare(
            "select id, time_created, coalesce(json_extract(data, '$.role'), '') from message \
             where session_id = ?1 order by time_created desc, id desc limit ?2",
        ).map_err(|e| e.to_string())?;
        let rows = stmt.query_map(rusqlite::params![session_id, (max * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| e.to_string())?;
        rows.flatten().collect()
    };
    let mut parts = con.prepare("select data from part where message_id = ?1 order by time_created, id")
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (id, at, role) in newest.into_iter().rev() {
        let role = match role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        let texts: Vec<String> = parts.query_map([&id], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .flatten()
            .filter_map(|data| serde_json::from_str::<Value>(&data).ok())
            .filter_map(|p| match p.get("type").and_then(Value::as_str)? {
                "text" if p.get("synthetic").and_then(Value::as_bool) != Some(true) => {
                    p.get("text").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string)
                }
                "tool" => Some(format!("[tool: {}]", p.get("tool").and_then(Value::as_str).unwrap_or("?"))),
                "file" => Some("[file]".to_string()),
                _ => None,
            })
            .collect();
        let text = texts.join("\n");
        if text.is_empty() || (role == Role::User && text.starts_with(crate::prompts::COMPACTION_NOTE_PREFIX)) {
            continue;
        }
        out.push(Message { role, text, at: at as f64 / 1000.0 });
    }
    Ok(crate::harness::last_turns(out, max))
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
    let export_file = scratch.join(format!("{session_id}.export.json"));
    let journal = scratch.join(format!("{session_id}.journal.jsonl"));
    export(opencode, session_id, None, &export_file)?;

    let conv = Command::new(sesync_conv)
        .args(["from-opencode"])
        .arg(&export_file)
        .arg("--out")
        .arg(&journal)
        .output()
        .map_err(|e| format!("cannot run sesync-conv: {e}"))?;
    let _ = std::fs::remove_file(&export_file);
    if !conv.status.success() {
        let _ = std::fs::remove_file(&journal);
        return Err(format!("sesync_conv from-opencode failed: {}", String::from_utf8_lossy(&conv.stderr).trim()));
    }
    Ok(journal)
}

/// O-penCode names an untitled session "New session - <ISO timestamp>". That is not a name, so
/// it is treated as untitled ("") and the caller falls back to the first prompt.
pub fn clean_title(title: &str) -> String {
    let t = title.trim();
    if is_default_title(t) { String::new() } else { t.to_string() }
}

/// True for O-penCode's untouched default name, "New session - <ISO timestamp>".
/// Only that exact shape is stripped, so a user title that mentions a date survives.
fn is_default_title(t: &str) -> bool {
    let Some(rest) = t.strip_prefix("New session - ") else { return false };
    // YYYY-MM-DDTHH:MM:SS(.fff)Z
    let b = rest.as_bytes();
    if b.len() < 20 || !rest.ends_with('Z') {
        return false;
    }
    let digit = |i: usize| b.get(i).map_or(false, |c| c.is_ascii_digit());
    let shape = digit(0) && digit(1) && digit(2) && digit(3) && b[4] == b'-' && digit(5) && digit(6)
        && b[7] == b'-' && digit(8) && digit(9) && b[10] == b'T' && digit(11) && digit(12)
        && b[13] == b':' && digit(14) && digit(15) && b[16] == b':' && digit(17) && digit(18);
    if !shape {
        return false;
    }
    // Remaining characters between seconds and the Z: nothing, or .digits
    let mid = &rest[19..rest.len() - 1];
    mid.is_empty() || (mid.starts_with('.') && mid[1..].bytes().all(|c| c.is_ascii_digit()) && mid.len() > 1)
}

/// Remove an O-penCode session by id, through the CLI. The mirror's `path` IS its session id, so
/// this is how an OpenCode mirror is taken away - never by deleting a file. `env_xdg` points the
/// CLI's XDG_* variables at a sandbox; `None` inherits the real environment. A session that is
/// not there counts as removed: that is the normal state of a mirror never imported.
pub fn remove_opencode_session(
    opencode: &Path,
    mirror_id: &str,
    cwd: &str,
    env_xdg: Option<&Path>,
) -> Result<(), String> {
    let mut del = Command::new(opencode);
    del.args(["session", "delete", mirror_id]);
    if Path::new(cwd).is_dir() {
        del.current_dir(cwd);
    }
    if let Some(x) = env_xdg {
        del.env("XDG_DATA_HOME", x.join("data"))
            .env("XDG_CONFIG_HOME", x.join("cfg"))
            .env("XDG_STATE_HOME", x.join("state"))
            .env("XDG_CACHE_HOME", x.join("cache"));
    }
    let out = del.output().map_err(|e| format!("cannot run opencode session delete: {e}"))?;
    // Exit 0 is success, whatever it printed: this CLI writes its "Session … deleted" line to
    // stderr, so requiring an empty stderr turned a real deletion into a reported failure.
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // A session that is not there is the normal case for a mirror that was never imported.
    if err.contains("not found") || stdout.contains("not found") {
        return Ok(());
    }
    Err(format!("opencode session delete failed: {}", if err.is_empty() { stdout } else { err }))
}

/// Install an omp/Pi journal as an O-penCode session. Ported from install_opencode:
/// `sesync-conv to-opencode` writes a bundle, then `opencode import` loads it. Any previous
/// import of the same mirror id is deleted first, because `opencode import` only upserts the
/// session row and conflict-ignores message ids: without the delete a refresh would keep the
/// old turns and pile the new ones on top. The import runs in the conversation's directory
/// (from `cwd`, else the directory recorded in the bundle) so the session lands in the right
/// project. The delete runs in `existing_dir`, where the store keeps the session being replaced
/// (`opencode session delete` only finds a session from inside its own project); empty means
/// unknown, and the import directory is used. `env_xdg` points the CLI's XDG_* variables at a
/// sandbox; `None` inherits the real environment. The import must print "Imported session" or
/// it is not counted as installed.
#[allow(clippy::too_many_arguments)]
pub fn install_opencode(
    opencode: &Path,
    sesync_conv: &Path,
    src: &Path,
    cwd: &str,
    existing_dir: &str,
    mirror_id: &str,
    title: &str,
    env_xdg: Option<&Path>,
) -> Result<String, String> {
    let scratch = std::env::temp_dir().join(format!("hs-oc-install-{}-{mirror_id}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let bundle = scratch.join("bundle.json");

    let conv = Command::new(sesync_conv)
        .arg("to-opencode").arg(src)
        .arg("--out").arg(&bundle)
        .args(["--id", mirror_id, "--title", title])
        .output()
        .map_err(|e| format!("cannot run sesync-conv: {e}"))?;
    if !conv.status.success() {
        let _ = fs::remove_dir_all(&scratch);
        return Err(format!("sesync-conv to-opencode failed: {}", String::from_utf8_lossy(&conv.stderr).trim()));
    }

    // Where the import runs decides the project and directory OpenCode stores the session
    // under. Prefer the conversation's own directory when it still exists, then the bundle's.
    let bundle_dir = fs::read_to_string(&bundle).ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["info"]["directory"].as_str().map(str::to_string))
        .unwrap_or_default();
    let dir = if Path::new(cwd).is_dir() { cwd.to_string() } else { bundle_dir };
    let workdir = if Path::new(&dir).is_dir() { Some(PathBuf::from(&dir)) } else { None };

    // `opencode import` is not atomic: it writes the session row, then stops at the first message
    // it rejects. With the delete in front of it, a bundle the importer refuses used to cost the
    // whole existing session (6926 messages on 2026-10-21, left with none). Import into a
    // throwaway store first; only a bundle that imports there may replace the real session.
    let probe = scratch.join("probe");
    let mut check = Command::new(opencode);
    check.arg("import").arg(&bundle)
        .env("XDG_DATA_HOME", probe.join("data"))
        .env("XDG_CONFIG_HOME", probe.join("cfg"))
        .env("XDG_STATE_HOME", probe.join("state"))
        .env("XDG_CACHE_HOME", probe.join("cache"));
    if let Some(w) = &workdir {
        check.current_dir(w);
    }
    let probed = check.output().map_err(|e| format!("cannot run opencode import: {e}"))?;
    if !probed.status.success() || !String::from_utf8_lossy(&probed.stdout).contains("Imported session") {
        let _ = fs::remove_dir_all(&scratch);
        return Err(format!(
            "opencode refuses this copy, so the existing session was left as it is: {}",
            String::from_utf8_lossy(&probed.stderr).trim()
        ));
    }

    // Delete the previous import of this mirror, from where the store keeps it. A missing
    // session is the normal first write and is ignored; any other delete failure surfaces on
    // the import below.
    let delete_dir = if Path::new(existing_dir).is_dir() { existing_dir } else { dir.as_str() };
    let _ = remove_opencode_session(opencode, mirror_id, delete_dir, env_xdg);

    let mut cmd = Command::new(opencode);
    cmd.arg("import").arg(&bundle);
    if let Some(w) = &workdir {
        cmd.current_dir(w);
    }
    if let Some(x) = env_xdg {
        cmd.env("XDG_DATA_HOME", x.join("data"))
            .env("XDG_CONFIG_HOME", x.join("cfg"))
            .env("XDG_STATE_HOME", x.join("state"))
            .env("XDG_CACHE_HOME", x.join("cache"));
    }
    let imp = cmd.output().map_err(|e| format!("cannot run opencode import: {e}"))?;
    let _ = fs::remove_dir_all(&scratch);
    let stdout = String::from_utf8_lossy(&imp.stdout);
    if !imp.status.success() || !stdout.contains("Imported session") {
        return Err(format!("opencode import failed: {}", String::from_utf8_lossy(&imp.stderr).trim()));
    }
    Ok(mirror_id.to_string())
}
