//! spidair: a harness built on Pi. Same JSONL journal, same project buckets and `session_info`
//! names as Pi; only the agent dir differs (`$SPIDAIR_AGENT_DIR`, default
//! `~/.config/spidair/agent`) and the resume command is `spidair --session <file>`.

use super::{journal, Harness, Location, Tools};
use std::path::{Path, PathBuf};

pub struct Spidair;

impl Harness for Spidair {
    fn id(&self) -> &'static str {
        "spidair"
    }
    // One cell wide like the other icons (an emoji is dropped by the screen scrub): Cyrillic zhe looks like a spider.
    fn icon(&self) -> &'static str {
        "ж"
    }
    fn locate(&self, home: &Path) -> Location {
        Location { root: super::env_path("SPIDAIR_AGENT_DIR", home.join(".config/spidair/agent")), cli: None }
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
    fn resume_argv(&self, _loc: &Location, key: &str) -> Result<Vec<String>, String> {
        Ok(vec!["spidair".into(), "--session".into(), key.into()])
    }
    fn placed_by_cwd(&self) -> bool {
        true
    }
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
    fn rename(&self, _loc: &Location, _tools: &Tools, key: &str, title: &str) -> Result<Option<String>, String> {
        let rec = serde_json::json!({"type": "session_info", "name": title});
        journal::rewrite(Path::new(key), title, |text| journal::append_record(text, rec)).map(|()| None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lives_in_the_spidair_agent_dir_and_resumes_with_spidair() {
        let home = Path::new("/home/u");
        let loc = Spidair.locate(home);
        if std::env::var_os("SPIDAIR_AGENT_DIR").is_none() {
            assert_eq!(loc.root, home.join(".config/spidair/agent"));
        }
        assert_eq!(Spidair.resume_argv(&loc, "/s/a.jsonl").unwrap(), vec!["spidair", "--session", "/s/a.jsonl"]);
        assert!(Spidair.native_journal());
    }
}
