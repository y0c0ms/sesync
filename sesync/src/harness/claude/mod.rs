//! Claude Code: one JSONL transcript per conversation under `<config dir>/projects/<bucket>/`,
//! named `<uuid>.jsonl`, with subagent transcripts beside it. Not a canonical journal: it is
//! converted with `sesync-conv from-claude`, and copies are written with `to-claude`.

pub mod compaction;

use super::journal::{self, is_uuid, one_level_down, user_text};
use super::{Harness, Location, Message, Role, Tools};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Claude;

impl Harness for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn icon(&self) -> &'static str {
        "✶"
    }
    fn locate(&self, home: &Path) -> Location {
        Location { root: super::env_path("CLAUDE_CONFIG_DIR", home.join(".claude")), cli: None }
    }
    fn present(&self, loc: &Location) -> bool {
        loc.root.is_dir()
    }
    fn sessions_dir(&self, loc: &Location) -> PathBuf {
        loc.root.join("projects")
    }
    fn sessions(&self, loc: &Location) -> Vec<PathBuf> {
        claude_sessions(&loc.root)
    }
    fn subagents(&self, loc: &Location) -> Vec<(String, PathBuf)> {
        claude_subagents(&loc.root)
    }
    fn title(&self, path: &Path) -> String {
        journal::title(path, claude_title_from_lines)
    }
    fn cwd(&self, path: &Path) -> Option<String> {
        journal::cwd_from_head(path)
    }
    fn newest_prompt(&self, _loc: &Location, key: &str) -> Option<f64> {
        journal::newest_prompt(Path::new(key), prompt_time)
    }
    fn to_journal(&self, _loc: &Location, tools: &Tools, key: &str, scratch: &Path) -> Result<Option<PathBuf>, String> {
        journal::convert_from(tools.sesync_conv, "from-claude", Path::new(key), scratch, "Claude session").map(Some)
    }
    /// A Claude transcript is named by its session uuid.
    fn id_in(&self, key: &str) -> Option<String> {
        let name = Path::new(key).file_name()?.to_str()?;
        name.strip_suffix(".jsonl").filter(|stem| uuid::Uuid::parse_str(stem).is_ok()).map(str::to_string)
    }
    fn install(&self, loc: &Location, tools: &Tools, journal: &Path, mirror_id: &str, title: &str) -> Result<String, String> {
        journal::install(tools.sesync_conv, journal, "to-claude", "projects", &loc.root, &[], mirror_id, title)
            .map(|p| p.display().to_string())
    }
    /// Claude's own rename: a `custom-title` record is appended (what /rename and -n write); the
    /// transcript itself is never rewritten.
    fn rename(&self, _loc: &Location, _tools: &Tools, key: &str, title: &str) -> Result<Option<String>, String> {
        let path = Path::new(key);
        let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        let rec = serde_json::json!({"type": "custom-title", "customTitle": title, "sessionId": session_id});
        journal::rewrite(path, title, |text| journal::append_record(text, rec)).map(|()| None)
    }
    /// Read from the transcript's end: converting a transcript of hundreds of megabytes to show
    /// its last turns would take seconds. Meta records, sidechains (subagent work), compaction
    /// summaries and Claude's own command wrappers are not turns.
    fn recent_messages(&self, _loc: &Location, _tools: &Tools, key: &str, _scratch: &Path, max: usize) -> Result<Vec<Message>, String> {
        let lines = journal::tail_lines(Path::new(key), max * 4, |l| l.contains("\"user\"") || l.contains("\"assistant\""))?;
        let mut out = Vec::new();
        for line in &lines {
            let Ok(r) = serde_json::from_str::<Value>(line) else { continue };
            let role = match r.get("type").and_then(Value::as_str) {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                _ => continue,
            };
            let flag = |k: &str| r.get(k).and_then(Value::as_bool).unwrap_or(false);
            if flag("isMeta") || flag("isSidechain") || flag("isCompactSummary") {
                continue;
            }
            let text = journal::content_text(r.pointer("/message/content"));
            if text.is_empty() || (role == Role::User && text.starts_with('<')) {
                continue;
            }
            out.push(Message { role, text, at: journal::record_time(&r).unwrap_or(0.0) });
        }
        Ok(super::last_turns(out, max))
    }
}

/// A user prompt in a Claude transcript: a `user` record that is not meta, a sidechain or a
/// compaction summary, with content someone typed. Its time, or None.
fn prompt_time(r: &serde_json::Value) -> Option<f64> {
    let flag = |k: &str| r.get(k).and_then(serde_json::Value::as_bool).unwrap_or(false);
    let is_prompt = r.get("type").and_then(serde_json::Value::as_str) == Some("user")
        && !flag("isMeta")
        && !flag("isSidechain")
        && !flag("isCompactSummary")
        && r.pointer("/message/content").is_some_and(crate::prompts::is_prompt_content);
    if is_prompt { journal::record_time(r) } else { None }
}

/// `<uuid>.jsonl`, the Claude Code transcript name. Subagent files are not sessions.
pub fn parse_claude_name(name: &str) -> Option<String> {
    let id = name.strip_suffix(".jsonl")?;
    if is_uuid(id) { Some(id.to_string()) } else { None }
}

/// Claude Code transcripts under `<claude_dir>/projects/*/`.
pub fn claude_sessions(claude_dir: &Path) -> Vec<PathBuf> {
    one_level_down(&claude_dir.join("projects"), |n| parse_claude_name(n).is_some())
}

/// Claude subagent transcripts: `<projects>/<bucket>/<parent-uuid>/subagents/*.jsonl`.
/// Returns (parent session id, subagent file). They belong to the parent session.
pub fn claude_subagents(claude_dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(buckets) = fs::read_dir(claude_dir.join("projects")) else { return Vec::new() };
    let mut out = Vec::new();
    for b in buckets.flatten().filter(|b| b.path().is_dir()) {
        let Ok(parents) = fs::read_dir(b.path()) else { continue };
        for p in parents.flatten().filter(|p| p.path().is_dir()) {
            let parent = p.file_name().to_string_lossy().into_owned();
            let sub = p.path().join("subagents");
            let Ok(files) = fs::read_dir(&sub) else { continue };
            for f in files.flatten() {
                let path = f.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if path.is_file() && name.ends_with(".jsonl") {
                    out.push((parent.clone(), path));
                }
            }
        }
    }
    out.sort();
    out
}

/// Claude Code's own continuation banners and wrapper text are never titles.
const CLAUDE_BANNER: [&str; 2] = ["This session is being continued", "Caveat:"];

/// (title, has_user_turn) for a Claude Code transcript, from its lines.
///
/// Precedence, as Claude Code's own reader applies it: custom-title (/rename or -n,
/// latest wins), then ai-title, then agent-name, then the first real user prompt.
pub fn claude_title_from_lines(lines: &[&str]) -> (String, bool) {
    let (mut custom, mut ai, mut agent, mut first, mut has_user) =
        (String::new(), String::new(), String::new(), String::new(), false);
    for line in lines {
        let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
        let kind = rec.get("type").and_then(Value::as_str).unwrap_or("");
        let field = |k: &str| rec.get(k).and_then(Value::as_str).map(|s| s.trim().to_string());
        match kind {
            "custom-title" => { if let Some(v) = field("customTitle").filter(|v| !v.is_empty()) { custom = v; } }
            "ai-title" => { if let Some(v) = field("aiTitle").filter(|v| !v.is_empty()) { ai = v; } }
            "agent-name" => { if let Some(v) = field("agentName").filter(|v| !v.is_empty()) { agent = v; } }
            "user" if rec.get("isMeta").and_then(Value::as_bool) != Some(true) => {
                if let Some(text) = rec.get("message").and_then(user_text) {
                    let text = text.trim();
                    if !text.is_empty() && !text.starts_with('<') {
                        has_user = true;
                        if first.is_empty() && !CLAUDE_BANNER.iter().any(|b| text.starts_with(b)) {
                            first = text.split('\n').next().unwrap_or("").chars().take(80).collect();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let title = [custom, ai, agent, first].into_iter().find(|t| !t.is_empty()).unwrap_or_default();
    (title, has_user)
}
