//! The RPC subset. Three methods are served:
//!   * `status.get`: what this host is, for the app's compatibility gate;
//!   * `worktree.ps`: the home screen's list, one row per session project directory;
//!   * `repo.list`: the same directories as repos, for grouping.
//! Every other method answers `method_not_found`. The app reads that as "not available here" and
//! skips the read, so the rest of its screens degrade rather than fail.
//!
//! Rows describe sesync's sessions, not live terminals or agents. Those fields are sent empty.

use super::catalog::{Catalog, Project};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const RUNTIME_ID: &str = "sesync-bridge";
/// Orca's `MOBILE_PROTOCOL_VERSION` (mobile/src/transport/protocol-version.ts). The phone refuses a
/// host whose `minCompatibleMobileVersion` is above that, so this is the version whose subset is
/// served here.
pub const PROTOCOL_VERSION: u64 = 3;
/// The oldest phone protocol this host supports. The app enforces its own floor of 2 on hosts.
pub const MIN_COMPATIBLE_MOBILE_VERSION: u64 = 2;

/// This host's identity, as `status.get` reports it.
pub struct Host {
    pub machine_name: String,
    /// Orca's platform names: `linux`, `darwin`, `win32`.
    pub platform: &'static str,
}

/// Answer one decrypted request frame. The reply carries the request's `id`, as the app matches on it.
pub fn dispatch(request: &Value, catalog: &dyn Catalog, host: &Host) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "status.get" => success(&id, status(host)),
        "worktree.ps" => match catalog.projects() {
            Ok(projects) => {
                let worktrees: Vec<Value> = projects.iter().map(worktree_row).collect();
                let total = worktrees.len();
                success(&id, json!({ "worktrees": worktrees, "totalCount": total, "truncated": false }))
            }
            Err(message) => error(&id, "unavailable", &message),
        },
        "repo.list" => match catalog.projects() {
            Ok(projects) => {
                let repos: Vec<Value> = projects.iter().map(repo_row).collect();
                success(&id, json!({ "repos": repos }))
            }
            Err(message) => error(&id, "unavailable", &message),
        },
        other => error(&id, "method_not_found", &format!("Unknown method: {other}")),
    }
}

pub fn success(id: &Value, result: Value) -> Value {
    json!({ "id": id, "ok": true, "result": result, "_meta": { "runtimeId": RUNTIME_ID } })
}

pub fn error(id: &Value, code: &str, message: &str) -> Value {
    json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message },
        "_meta": { "runtimeId": RUNTIME_ID },
    })
}

fn status(host: &Host) -> Value {
    json!({
        "runtimeId": RUNTIME_ID,
        "appVersion": env!("CARGO_PKG_VERSION"),
        "protocolVersion": PROTOCOL_VERSION,
        "minCompatibleMobileVersion": MIN_COMPATIBLE_MOBILE_VERSION,
        "capabilities": [],
        "floatingWorkspaceEnabled": false,
        "hostPlatform": host.platform,
        "machineName": host.machine_name,
    })
}

/// A stable id for a project directory. Hashed, so an id never carries path characters a route
/// or a URL would have to escape.
fn repo_id(dir: &str) -> String {
    let digest = Sha256::digest(dir.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("sesync-{hex}")
}

/// The name shown for a project: its last path component, or a plain label for sessions whose
/// harness records no directory.
fn project_name(dir: &str) -> String {
    if dir.is_empty() {
        return "no project directory".into();
    }
    Path::new(dir).file_name().map_or_else(|| dir.to_string(), |n| n.to_string_lossy().into_owned())
}

fn worktree_row(p: &Project) -> Value {
    let repo = repo_id(&p.dir);
    let name = project_name(&p.dir);
    let newest_ms = (p.newest() * 1000.0).round();
    let count = p.sessions.len();
    let latest = p.sessions.first().map_or("", |s| s.title.as_str());
    json!({
        "worktreeId": format!("{repo}::{}", p.dir),
        "repoId": repo,
        "repo": name,
        "path": p.dir,
        "branch": "",
        "workspaceKind": "folder-workspace",
        "isArchived": false,
        "isMainWorktree": false,
        "hasHostSidebarActivity": false,
        "parentWorktreeId": null,
        "childWorktreeIds": [],
        "displayName": name,
        "workspaceStatus": "in-progress",
        "sortOrder": newest_ms,
        "lastActivityAt": newest_ms,
        "linkedIssue": null,
        "linkedPR": null,
        "linkedLinearIssue": null,
        "linkedGitLabMR": null,
        "linkedGitLabIssue": null,
        "comment": "",
        "isPinned": false,
        "isActive": false,
        "unread": false,
        "liveTerminalCount": 0,
        "hasAttachedPty": false,
        "lastOutputAt": null,
        "preview": format!("{count} session{} · latest: {latest}", if count == 1 { "" } else { "s" }),
        "status": "inactive",
        "agents": [],
    })
}

fn repo_row(p: &Project) -> Value {
    json!({
        "id": repo_id(&p.dir),
        "displayName": project_name(&p.dir),
        "path": p.dir,
        "badgeColor": "#64748b",
        "connectionId": null,
    })
}

#[cfg(test)]
mod tests {
    use super::super::catalog::Session;
    use super::*;

    struct Fixed(Vec<Project>);
    impl Catalog for Fixed {
        fn projects(&self) -> Result<Vec<Project>, String> {
            Ok(self.0.clone())
        }
    }

    struct Broken;
    impl Catalog for Broken {
        fn projects(&self) -> Result<Vec<Project>, String> {
            Err("cannot read the sesync state".into())
        }
    }

    fn host() -> Host {
        Host { machine_name: "box".into(), platform: "linux" }
    }

    fn req(id: &str, method: &str) -> Value {
        json!({ "id": id, "deviceToken": "t", "method": method })
    }

    #[test]
    fn unknown_methods_answer_method_not_found_with_the_request_id() {
        let reply = dispatch(&req("rpc-7", "terminal.list"), &Fixed(vec![]), &host());
        assert_eq!(reply["id"], "rpc-7");
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], "method_not_found");
        assert_eq!(reply["error"]["message"], "Unknown method: terminal.list");
        assert_eq!(reply["_meta"]["runtimeId"], RUNTIME_ID);
    }

    #[test]
    fn status_reports_the_protocol_the_app_gates_on() {
        let reply = dispatch(&req("s", "status.get"), &Fixed(vec![]), &host());
        assert_eq!(reply["ok"], true);
        let result = &reply["result"];
        assert_eq!(result["protocolVersion"], 3);
        assert_eq!(result["minCompatibleMobileVersion"], 2);
        assert_eq!(result["hostPlatform"], "linux");
        assert!(result["capabilities"].is_array());
        assert!(result["floatingWorkspaceEnabled"].is_boolean());
        assert!(result["appVersion"].is_string());
    }

    #[test]
    fn worktree_rows_carry_every_field_the_app_reads_and_group_by_directory() {
        let projects = vec![Project {
            dir: "/home/u/work/alpha".into(),
            sessions: vec![
                Session { harness: "omp".into(), title: "fix the login".into(), updated: 200.0 },
                Session { harness: "claude".into(), title: "older".into(), updated: 100.0 },
            ],
        }];
        let reply = dispatch(&req("w", "worktree.ps"), &Fixed(projects), &host());
        let result = &reply["result"];
        assert_eq!(result["totalCount"], 1);
        assert_eq!(result["truncated"], false);
        let row = &result["worktrees"][0];
        // The keys of the desktop's RuntimeWorktreePsSummary that the app's Worktree row requires.
        for key in ["worktreeId", "repoId", "repo", "path", "branch", "displayName", "workspaceStatus",
                    "sortOrder", "linkedIssue", "linkedPR", "comment", "isPinned", "isActive", "unread",
                    "liveTerminalCount", "hasAttachedPty", "lastOutputAt", "preview", "status", "agents"] {
            assert!(row.get(key).is_some(), "row is missing {key}");
        }
        assert_eq!(row["repo"], "alpha");
        assert_eq!(row["path"], "/home/u/work/alpha");
        assert_eq!(row["status"], "inactive");
        assert_eq!(row["agents"], json!([]));
        assert_eq!(row["preview"], "2 sessions · latest: fix the login");
        assert!(row["worktreeId"].as_str().unwrap().ends_with("::/home/u/work/alpha"));
        assert_eq!(row["repoId"], repo_row(&Project { dir: "/home/u/work/alpha".into(), sessions: vec![] })["id"]);
    }

    #[test]
    fn an_empty_catalog_is_a_valid_empty_list() {
        let reply = dispatch(&req("w", "worktree.ps"), &Fixed(vec![]), &host());
        assert_eq!(reply["result"]["worktrees"], json!([]));
        assert_eq!(reply["result"]["totalCount"], 0);
        let repos = dispatch(&req("r", "repo.list"), &Fixed(vec![]), &host());
        assert_eq!(repos["result"]["repos"], json!([]));
    }

    #[test]
    fn a_catalog_failure_is_an_error_reply_not_an_empty_list() {
        let reply = dispatch(&req("w", "worktree.ps"), &Broken, &host());
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], "unavailable");
    }

    #[test]
    fn a_directory_less_group_has_a_plain_name() {
        let projects = vec![Project {
            dir: String::new(),
            sessions: vec![Session { harness: "omp".into(), title: "t".into(), updated: 1.0 }],
        }];
        let reply = dispatch(&req("w", "worktree.ps"), &Fixed(projects), &host());
        assert_eq!(reply["result"]["worktrees"][0]["displayName"], "no project directory");
        assert_eq!(reply["result"]["worktrees"][0]["path"], "");
    }
}
