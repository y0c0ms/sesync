//! Confirmed import of a team session into a local harness. This is the only place a team
//! session becomes a local file, and it needs an explicit confirmation from the user.

use std::io::Write;
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub enum ImportOutcome { Written }

#[derive(Debug, PartialEq, Eq)]
pub enum ImportError {
    /// The user did not confirm. Nothing is written.
    NotConfirmed,
    /// A local session already exists at the target. It is kept as it is.
    WouldOverwrite,
    /// The file could not be written.
    Io(String),
}

/// Write `bytes` to `target`, but only if the user confirmed and nothing is there yet. The file
/// is created exclusively, so a race with a local write cannot replace it either.
pub fn import_confirmed(target: &Path, bytes: &[u8], confirmed: bool) -> Result<ImportOutcome, ImportError> {
    if !confirmed {
        return Err(ImportError::NotConfirmed);
    }
    if target.exists() {
        return Err(ImportError::WouldOverwrite);
    }
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| ImportError::Io(e.to_string()))?;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists { ImportError::WouldOverwrite }
        else { ImportError::Io(e.to_string()) }
    })?;
    f.write_all(bytes).map_err(|e| ImportError::Io(e.to_string()))?;
    f.sync_all().ok();
    Ok(ImportOutcome::Written)
}

/// The journal a fork is written as: the same messages under a new session id, with `title` as the
/// newest name (a `title_change` record, which omp and Pi read as the rename). Every message is kept
/// as it was; nothing else in the header changes.
pub fn fork_journal(bytes: &[u8], new_id: &str, title: &str) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "the journal is not UTF-8".to_string())?;
    let mut out = String::new();
    let mut header = false;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let serde_json::Value::Object(mut record) = serde_json::from_str::<serde_json::Value>(line.trim())
            .map_err(|_| "the journal has a line that is not a record".to_string())?
        else {
            return Err("the journal has a line that is not a record".into());
        };
        if !header && record.get("type").and_then(serde_json::Value::as_str) == Some("session") {
            record.insert("id".into(), serde_json::Value::String(new_id.to_string()));
            header = true;
        }
        out.push_str(&serde_json::Value::Object(record).to_string());
        out.push('\n');
    }
    if !header {
        return Err("the journal has no session header".into());
    }
    let rename = serde_json::json!({
        "type": "title_change",
        "id": format!("fork-{new_id}"),
        "timestamp": crate::harness::journal::now_iso_ms(),
        "name": title,
    });
    out.push_str(&rename.to_string());
    out.push('\n');
    Ok(out.into_bytes())
}

/// Replace a local journal with `bytes`, after the bytes it holds are kept in the archive and read
/// back (`archive_store::keep_before_replace`). If the archive fails, nothing is replaced. The
/// replacement is a temp file renamed over the target, so a reader sees the old file or the new one.
pub fn replace_confirmed(archive: &Path, conversation: &str, target: &Path, bytes: &[u8], confirmed: bool)
    -> Result<ImportOutcome, ImportError>
{
    if !confirmed {
        return Err(ImportError::NotConfirmed);
    }
    let current = std::fs::read(target).map_err(|e| ImportError::Io(e.to_string()))?;
    if current == bytes {
        return Ok(ImportOutcome::Written);
    }
    let name = target.file_name().and_then(|n| n.to_str())
        .ok_or_else(|| ImportError::Io("the local copy has no file name".into()))?;
    crate::archive_store::keep_before_replace(archive, conversation, name, &current)
        .map_err(|e| ImportError::Io(format!("not replaced, the old copy could not be archived: {e}")))?;
    let dir = target.parent().ok_or_else(|| ImportError::Io("the local copy has no folder".into()))?;
    let tmp = dir.join(format!(".{name}.sesync-replace"));
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| ImportError::Io(e.to_string()))?;
        f.write_all(bytes).map_err(|e| ImportError::Io(e.to_string()))?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, target).map_err(|e| ImportError::Io(e.to_string()))?;
    Ok(ImportOutcome::Written)
}
