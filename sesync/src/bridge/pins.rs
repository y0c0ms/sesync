//! Pinned conversations: `worktree.set {worktree, isPinned}`. The pins live in the bridge's own state
//! file, `<bridge dir>/pins.json`, owner-only. sesync's store is never written, so a pin never changes
//! what the TUI or the daemon see. `worktree.ps` reports each row's `isPinned` from the same file.

use super::rpc::{self, Ctx, Failure};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const FILE: &str = "pins.json";

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// The pinned worktree ids. A missing file pins nothing; an unreadable one is an error, so a write
/// never replaces a file it could not read.
pub fn load(dir: &Path) -> Result<BTreeSet<String>, String> {
    let text = match std::fs::read_to_string(path(dir)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(format!("cannot read {FILE}: {e}")),
    };
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("{FILE} is not valid JSON: {e}"))?;
    let rows = value["pinned"].as_array().ok_or_else(|| format!("{FILE} has no pinned list"))?;
    rows.iter()
        .map(|row| row.as_str().map(str::to_string).ok_or_else(|| format!("{FILE} has a non-string pin")))
        .collect()
}

fn save(dir: &Path, pinned: &BTreeSet<String>) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(&json!({ "pinned": pinned }))
        .map_err(|e| format!("cannot encode {FILE}: {e}"))?;
    super::write_owner_only(&path(dir), &bytes)
}

/// `worktree.set {worktree, isPinned}`. Only `isPinned` is served: a request that names any other
/// field is refused, because answering success while ignoring it would be a false success.
pub fn set(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let object = params.as_object().ok_or(("bad_request", "params must be an object".to_string()))?;
    let other = object
        .iter()
        .find(|(key, value)| !matches!(key.as_str(), "worktree" | "isPinned") && !value.is_null());
    if let Some((key, _)) = other {
        return Err(("bad_request", format!("worktree.set serves isPinned only, not {key}")));
    }
    let pinned = params["isPinned"].as_bool().ok_or(("bad_request", "isPinned must be a boolean".to_string()))?;
    let worktree = rpc::conversation_of_params(ctx, params)?.worktree_id();
    let mut all = load(&ctx.dir).map_err(|m| ("unavailable", m))?;
    if pinned {
        all.insert(worktree.clone());
    } else {
        all.remove(&worktree);
    }
    save(&ctx.dir, &all).map_err(|m| ("unavailable", m))?;
    Ok(json!({ "worktree": worktree, "isPinned": pinned }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::testkit::{conversation, ctx_with, sandbox};

    fn ctx_for(name: &str) -> (Ctx, PathBuf) {
        let dir = sandbox(name);
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let ctx = ctx_with(vec![conversation("alpha", project.to_str().unwrap())], &dir.join("bridge"));
        (ctx, dir)
    }

    fn selector(ctx: &Ctx) -> String {
        format!("id:{}", ctx.catalog.conversations().unwrap()[0].worktree_id())
    }

    #[test]
    fn a_pin_is_stored_in_the_bridge_file_and_an_unpin_removes_it() {
        let (ctx, _dir) = ctx_for("pin");
        let worktree = selector(&ctx);
        let reply = set(&json!({ "worktree": worktree, "isPinned": true }), &ctx).unwrap();
        assert_eq!(reply["isPinned"], true);
        assert_eq!(load(&ctx.dir).unwrap().len(), 1);
        set(&json!({ "worktree": worktree, "isPinned": false }), &ctx).unwrap();
        assert!(load(&ctx.dir).unwrap().is_empty());
    }

    #[test]
    fn the_pin_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (ctx, _dir) = ctx_for("mode");
        set(&json!({ "worktree": selector(&ctx), "isPinned": true }), &ctx).unwrap();
        let mode = std::fs::metadata(path(&ctx.dir)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_missing_file_pins_nothing() {
        let dir = sandbox("missing-pins");
        assert!(load(&dir).unwrap().is_empty());
    }

    #[test]
    fn a_damaged_file_fails_the_write_and_is_left_alone() {
        let (ctx, _dir) = ctx_for("damaged");
        std::fs::create_dir_all(&ctx.dir).unwrap();
        std::fs::write(path(&ctx.dir), "{not json").unwrap();
        let err = set(&json!({ "worktree": selector(&ctx), "isPinned": true }), &ctx).unwrap_err();
        assert_eq!(err.0, "unavailable");
        assert_eq!(std::fs::read_to_string(path(&ctx.dir)).unwrap(), "{not json");
    }

    #[test]
    fn an_unknown_worktree_is_not_found() {
        let (ctx, _dir) = ctx_for("unknown");
        let err = set(&json!({ "worktree": "id:nope", "isPinned": true }), &ctx).unwrap_err();
        assert_eq!(err.0, "worktree_not_found");
    }

    #[test]
    fn is_pinned_must_be_a_boolean_and_no_other_field_is_accepted() {
        let (ctx, _dir) = ctx_for("shape");
        let worktree = selector(&ctx);
        assert_eq!(set(&json!({ "worktree": worktree }), &ctx).unwrap_err().0, "bad_request");
        assert_eq!(set(&json!({ "worktree": worktree, "isPinned": "yes" }), &ctx).unwrap_err().0, "bad_request");
        let comment = set(&json!({ "worktree": worktree, "isPinned": true, "comment": "x" }), &ctx).unwrap_err();
        assert_eq!(comment.0, "bad_request");
        assert!(comment.1.contains("comment"));
        assert!(set(&json!({ "worktree": worktree, "isPinned": true, "comment": null }), &ctx).is_ok());
    }
}
