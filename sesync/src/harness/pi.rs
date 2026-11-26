//! Pi: the same JSONL journal as omp, one file per conversation under
//! `<agent dir>/sessions/<bucket>/<timestamp>_<uuid>.jsonl`, but the bucket is the absolute
//! project path (`journal::pi_bucket`), and Pi has no title slot: it shows the last
//! `session_info` name.

use super::{journal, Harness, Location, Tools};
use std::path::{Path, PathBuf};

pub struct Pi;

impl Harness for Pi {
    fn id(&self) -> &'static str {
        "pi"
    }
    fn icon(&self) -> &'static str {
        "π"
    }
    fn locate(&self, home: &Path) -> Location {
        Location { root: super::env_path("PI_AGENT_DIR", home.join(".pi/agent")), cli: None }
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
    /// `pi --session` takes the session file's path. Unverified: Pi is not installed on the machine
    /// this was written on, so check the flag against `pi --help` before relying on it.
    fn resume_argv(&self, _loc: &Location, key: &str) -> Result<Vec<String>, String> {
        Ok(vec!["pi".into(), "--session".into(), key.into()])
    }
    fn placed_by_cwd(&self) -> bool {
        true
    }
    /// Pi files a conversation under its absolute project path (`journal::pi_bucket`); a copy in
    /// another folder is never listed for its project.
    fn misplaced(&self, path: &str, cwd: Option<&str>, _home: &str) -> bool {
        cwd.is_some_and(|c| journal::parent_name(path).as_deref() != Some(journal::pi_bucket(c).as_str()))
    }
    fn folder_for_cwd(&self, loc: &Location, _home: &str, cwd: &str) -> Option<PathBuf> {
        Some(self.sessions_dir(loc).join(journal::pi_bucket(cwd)))
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
        journal::install(tools.sesync_conv, journal, "to-pi", "sessions", &loc.root, &[], mirror_id, title)
            .map(|p| p.display().to_string())
    }
    /// Pi's own rename: a `session_info` record with the name is appended; history is kept.
    fn rename(&self, _loc: &Location, _tools: &Tools, key: &str, title: &str) -> Result<Option<String>, String> {
        journal::rewrite(Path::new(key), title, |text| journal::append_session_info(text, title)).map(|()| None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_outside_its_project_folder_is_misplaced() {
        let cwd = Some("/home/u/proj");
        assert!(!Pi.misplaced("/store/sessions/--home-u-proj--/x.jsonl", cwd, "/home/u"));
        assert!(Pi.misplaced("/store/sessions/--home-u-proj-/x.jsonl", cwd, "/home/u"),
            "a copy in the folder an older rule named is not where Pi lists it");
        assert!(Pi.misplaced("/store/sessions/--/x.jsonl", cwd, "/home/u"));
        assert!(!Pi.misplaced("/store/sessions/--/x.jsonl", None, "/home/u"), "without a cwd nothing is called misplaced");
    }

    /// A rename appends a name record that Pi resumes from: it must hang off the leaf, or Pi
    /// resumes with no context (`pi_resume_leaf` is Pi's rule: the last record's id).
    #[test]
    fn a_rename_keeps_pi_resuming_with_its_context() {
        let dir = std::env::temp_dir().join(format!("pi-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("2026-11-22T10-00-00-000Z_01a20546-5d97-73e2-a862-187a4d9eb592.jsonl");
        let text = concat!(
            r#"{"type":"session","version":3,"id":"01a20546-5d97-73e2-a862-187a4d9eb592","timestamp":"2026-11-22T10:00:00.000Z","cwd":"/home/u/proj"}"#, "\n",
            r#"{"type":"message","id":"m1","parentId":null,"message":{"role":"user","content":"q1"}}"#, "\n",
            r#"{"type":"message","id":"a1","parentId":"m1","message":{"role":"assistant","content":[{"type":"text","text":"a1"}]}}"#, "\n",
            r#"{"type":"message","id":"m2","parentId":"a1","message":{"role":"user","content":"q2"}}"#, "\n",
            r#"{"type":"message","id":"a2","parentId":"m2","message":{"role":"assistant","content":[{"type":"text","text":"a2"}]}}"#, "\n",
        );
        std::fs::write(&file, text).unwrap();
        let loc = Location { root: dir.clone(), cli: None };
        let tools = Tools { home: &dir, sesync_conv: &dir };
        Pi.rename(&loc, &tools, file.to_str().unwrap(), "Renamed Pi").unwrap();
        let after = std::fs::read_to_string(&file).unwrap();
        let last: serde_json::Value = serde_json::from_str(after.lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "session_info");
        assert_eq!(last["name"], "Renamed Pi");
        assert_eq!(last["parentId"], "a2", "the name hangs off the leaf");
        assert_eq!(journal::pi_resume_leaf(&after).as_deref(), last["id"].as_str(), "Pi resumes from the name record");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
