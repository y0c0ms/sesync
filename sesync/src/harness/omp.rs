//! omp (oh-my-pi): one JSONL journal per conversation under `<agent dir>/sessions/<bucket>/`,
//! named `<timestamp>_<uuid>.jsonl`. The bucket is the project directory relative to the home;
//! the first line is a fixed-width title slot. The canonical journal is omp's own format.

use super::{journal, Harness, Location, Tools};
use std::path::{Path, PathBuf};

pub struct Omp;

impl Harness for Omp {
    fn id(&self) -> &'static str {
        "omp"
    }
    fn icon(&self) -> &'static str {
        "Ω"
    }
    fn locate(&self, home: &Path) -> Location {
        Location { root: super::env_path("OMP_AGENT_DIR", home.join(".omp/agent")), cli: None }
    }
    fn present(&self, loc: &Location) -> bool {
        loc.root.is_dir()
    }
    fn sessions_dir(&self, loc: &Location) -> PathBuf {
        loc.root.join("sessions")
    }
    fn sessions(&self, loc: &Location) -> Vec<PathBuf> {
        if loc.root.is_dir() { journal::jsonl_sessions(&loc.root) } else { Vec::new() }
    }
    fn title(&self, path: &Path) -> String {
        journal::title(path, journal::omp_title_from_lines)
    }
    fn cwd(&self, path: &Path) -> Option<String> {
        journal::cwd_from_head(path)
    }
    fn placed_by_cwd(&self) -> bool {
        true
    }
    /// omp files a conversation under its project directory relative to the home. A copy in
    /// another folder (an older write with the wrong `--home`, or a dropped cwd) is never listed.
    fn misplaced(&self, path: &str, cwd: Option<&str>, home: &str) -> bool {
        match cwd.and_then(|c| home_bucket(c, home)) {
            Some(expected) => journal::parent_name(path).as_deref() != Some(expected.as_str()),
            None => false,
        }
    }
    fn newest_prompt(&self, _loc: &Location, key: &str) -> Option<f64> {
        journal::newest_prompt(Path::new(key), journal::message_prompt_time)
    }
    fn native_journal(&self) -> bool {
        true
    }
    fn to_journal(&self, _loc: &Location, _tools: &Tools, _key: &str, _scratch: &Path) -> Result<Option<PathBuf>, String> {
        Ok(None)
    }
    fn id_in(&self, key: &str) -> Option<String> {
        journal::parse_omp_name(Path::new(key).file_name()?.to_str()?)
    }
    fn install(&self, loc: &Location, tools: &Tools, journal: &Path, mirror_id: &str, title: &str) -> Result<String, String> {
        // omp buckets are home-relative, so the converter needs the real home, never the store's
        // parent (`~/.omp` would put every copy in `--home-...-`).
        let home = vec!["--home".to_string(), tools.home.display().to_string()];
        journal::install(tools.sesync_conv, journal, "to-omp", "sessions", &loc.root, &home, mirror_id, title)
            .map(|p| p.display().to_string())
    }
    fn rename(&self, _loc: &Location, _tools: &Tools, key: &str, title: &str) -> Result<Option<String>, String> {
        journal::rewrite(Path::new(key), title, |text| retitled(text, title)).map(|()| None)
    }
}

/// omp's project folder when `cwd` sits under `home`: the relative path with a leading '-'. None
/// when cwd is outside home: omp's fallback encoding there is not pinned down on this platform,
/// and a guessed folder would flag good copies as misplaced. Keep in step with the converter.
fn home_bucket(cwd: &str, home: &str) -> Option<String> {
    let norm = |s: &str| s.replace('/', "\\");
    let (c, h) = (norm(cwd.trim_end_matches(['\\', '/'])), norm(home.trim_end_matches(['\\', '/'])));
    if c.len() >= h.len() && c[..h.len()].eq_ignore_ascii_case(&h) && (c.len() == h.len() || c.as_bytes()[h.len()] == b'\\') {
        let rel = &c[h.len()..];
        if rel.is_empty() {
            return Some("-".to_string());
        }
        return Some(journal::seg_join(rel));
    }
    None
}

/// omp's own rename: the 256-byte title slot (the first line, which omp reads first) is
/// rewritten, the header title is kept in step for readers that fall back to it, and a
/// `title_change` entry is appended, exactly what omp records when the rename is made inside it.
fn retitled(text: &str, title: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let previous = {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        journal::omp_title_from_lines(&refs).0
    };
    let now = now_iso_ms();
    let mut slot_written = false;
    let mut header_written = false;
    if let Some(first) = lines.first_mut() {
        // omp always has the slot as the first line. Any title record there is the slot; a file
        // with no slot and no header cannot show the name, and the caller's read-back check turns
        // that into an error.
        let is_slot = serde_json::from_str::<serde_json::Value>(first)
            .ok()
            .and_then(|v| v.get("type").and_then(serde_json::Value::as_str).map(str::to_string))
            .as_deref() == Some("title");
        if is_slot {
            *first = slot_line(title, &now);
            slot_written = true;
        }
    }
    for l in lines.iter_mut() {
        if l.starts_with("{\"type\":\"session\"") {
            if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(l) {
                v["title"] = serde_json::Value::String(title.into());
                *l = v.to_string();
                header_written = true;
            }
        }
    }
    if slot_written || header_written {
        // omp chains each entry to the one before it; parentId is that entry's id.
        let parent = lines.iter().rev().find_map(|l| {
            serde_json::from_str::<serde_json::Value>(l).ok()
                .and_then(|v| v.get("id").and_then(serde_json::Value::as_str).map(str::to_string))
        });
        let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, format!("{now}|{previous}|{title}").as_bytes())
            .simple().to_string();
        let mut rec = serde_json::Map::new();
        rec.insert("type".into(), serde_json::Value::String("title_change".into()));
        rec.insert("id".into(), serde_json::Value::String(id[..8].to_string()));
        if let Some(p) = parent {
            rec.insert("parentId".into(), serde_json::Value::String(p));
        }
        rec.insert("source".into(), serde_json::Value::String("user".into()));
        rec.insert("timestamp".into(), serde_json::Value::String(now.clone()));
        if !previous.is_empty() {
            rec.insert("previousTitle".into(), serde_json::Value::String(previous));
        }
        rec.insert("title".into(), serde_json::Value::String(title.into()));
        lines.push(serde_json::Value::Object(rec).to_string());
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

const TITLE_SLOT_BYTES: usize = 256;

/// omp's fixed-width title slot line (exactly 256 bytes including the newline), matching the
/// serializer in omp 18.8.6: keys in this order, `source` is `"auto"` or `"user"`, and the title
/// is trimmed until it fits.
fn slot_line(title: &str, updated_at: &str) -> String {
    let mut t = title.to_string();
    loop {
        let mut m = serde_json::Map::new();
        m.insert("type".into(), serde_json::Value::String("title".into()));
        m.insert("v".into(), serde_json::Value::from(1));
        m.insert("title".into(), serde_json::Value::String(t.clone()));
        m.insert("source".into(), serde_json::Value::String("user".into()));
        m.insert("updatedAt".into(), serde_json::Value::String(updated_at.into()));
        m.insert("pad".into(), serde_json::Value::String(String::new()));
        let base = serde_json::to_string(&m).unwrap();
        if base.len() + 1 <= TITLE_SLOT_BYTES {
            let pad = TITLE_SLOT_BYTES - 1 - base.len();
            m.insert("pad".into(), serde_json::Value::String(" ".repeat(pad)));
            return serde_json::to_string(&m).unwrap();
        }
        if t.pop().is_none() {
            return base;
        }
    }
}

/// UTC now as `2026-10-18T17:01:16.266Z`, the shape omp stamps on entries (days-from-civil
/// inverse; no timezone other than Z is ever written here).
fn now_iso_ms() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let (month, year) = if mp < 10 { (mp + 3, y) } else { (mp - 9, y + 1) };
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{mi:02}:{s:02}.{:03}Z", d.subsec_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_folders_match_the_converter_table() {
        assert_eq!(home_bucket("/home/u/proj", "/home/u").as_deref(), Some("-proj"));
        assert_eq!(home_bucket("/home/u/projects/CodeTeacher", "/home/u").as_deref(), Some("-projects-CodeTeacher"));
        assert_eq!(home_bucket("/home/u", "/home/u").as_deref(), Some("-"));
        assert_eq!(home_bucket("/tmp/x", "/home/u"), None, "outside home: not checked");
        assert_eq!(home_bucket("/home/other", "/home/u"), None, "a prefix is not under home");
    }

    #[test]
    fn a_copy_outside_its_project_folder_is_misplaced_and_an_unknown_place_is_not() {
        let cwd = Some("/home/u/proj");
        assert!(Omp.misplaced("/store/sessions/--/x.jsonl", cwd, "/home/u"));
        assert!(Omp.misplaced("/store/sessions/--home-u-proj-/x.jsonl", cwd, "/home/u"));
        assert!(!Omp.misplaced("/store/sessions/-proj/x.jsonl", cwd, "/home/u"));
        assert!(!Omp.misplaced("/store/sessions/--/x.jsonl", None, "/home/u"));
        assert!(!Omp.misplaced("/store/sessions/--tmp-/x.jsonl", Some("/tmp"), "/home/u"), "outside home is not checked");
    }
}
