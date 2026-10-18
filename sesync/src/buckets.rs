//! Bucket names for the file-backed harness stores, and the cwd they depend on.
//!
//! The converter (`sesync-conv/src/journal.rs`) is the single writer of mirrors; this copy
//! exists only so the daemon can notice that a mirror recorded in state sits in a bucket
//! the harness will never list. Keep the rules in sync with the converter. The test table
//! below mirrors the one verified against every bucket on this box.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Each path separator becomes '-', the drive colon too. Every other character is kept.
fn seg_join(path: &str) -> String {
    let mut out = String::new();
    for ch in path.chars() {
        match ch {
            '\\' | '/' | ':' => out.push('-'),
            c => out.push(c),
        }
    }
    out
}

/// Pi's bucket: the absolute path, separators dashed, wrapped in a leading and trailing '-'.
/// Verified on disk: `/home/u/proj` -> `--home-u-proj-`, `/tmp` -> `--tmp-`.
pub fn pi_bucket(cwd: &str) -> String {
    format!("-{}-", seg_join(cwd.trim_end_matches(['\\', '/'])))
}

/// omp's bucket when `cwd` sits under `home`: the relative path with a leading '-'.
/// `None` when cwd is outside home: omp's fallback encoding there is not pinned down on this
/// platform, and a guessed "expected bucket" would flag good mirrors as stale.
pub fn omp_home_bucket(cwd: &str, home: &str) -> Option<String> {
    let norm = |s: &str| s.replace('/', "\\");
    let (c, h) = (norm(cwd.trim_end_matches(['\\', '/'])), norm(home.trim_end_matches(['\\', '/'])));
    if c.len() >= h.len() && c[..h.len()].eq_ignore_ascii_case(&h) && (c.len() == h.len() || c.as_bytes()[h.len()] == b'\\') {
        let rel = &c[h.len()..];
        if rel.is_empty() {
            return Some("-".to_string());
        }
        return Some(seg_join(rel));
    }
    None
}

/// The first cwd a source file records, read from a 64 KiB head window. omp journals carry
/// it on the `session` record; Claude transcripts on most records. `None` for formats that
/// keep no cwd (gemini, opencode) and for an unreadable file.
pub fn cwd_from_head(fmt: &str, path: &Path) -> Option<String> {
    if fmt != "omp" && fmt != "pi" && fmt != "claude" {
        return None;
    }
    let mut f = File::open(path).ok()?;
    let mut buf = vec![0u8; 64 * 1024];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                return Some(c.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_match_the_converter_table() {
        assert_eq!(omp_home_bucket("/home/u/proj", "/home/u").as_deref(), Some("-proj"));
        assert_eq!(omp_home_bucket("/home/u/projects/CodeTeacher", "/home/u").as_deref(), Some("-projects-CodeTeacher"));
        assert_eq!(omp_home_bucket("/home/u", "/home/u").as_deref(), Some("-"));
        assert_eq!(omp_home_bucket("/tmp/x", "/home/u"), None, "outside home: not checked");
        assert_eq!(omp_home_bucket("/home/other", "/home/u"), None, "a prefix is not under home");
        assert_eq!(pi_bucket("/home/u/proj"), "--home-u-proj-");
        assert_eq!(pi_bucket("/tmp"), "--tmp-");
    }

    fn head_file(name: &str, text: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("sesync-buckets-{}-{name}.jsonl", std::process::id()));
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn the_cwd_is_found_in_a_journal_header_and_a_claude_record() {
        let omp = head_file("omp", "{\"type\":\"title\",\"title\":\"T\"}\n{\"type\":\"session\",\"cwd\":\"/home/u/proj\"}\n");
        assert_eq!(cwd_from_head("omp", &omp).as_deref(), Some("/home/u/proj"));
        let claude = head_file("claude", "{\"type\":\"mode\"}\n{\"type\":\"system\",\"cwd\":\"/home/u/projects\"}\n");
        assert_eq!(cwd_from_head("claude", &claude).as_deref(), Some("/home/u/projects"));
        let none = head_file("none", "{\"type\":\"mode\"}\n");
        assert_eq!(cwd_from_head("claude", &none), None);
        assert_eq!(cwd_from_head("gemini", &claude), None, "formats without a cwd are not read");
        let _ = std::fs::remove_file(&omp);
        let _ = std::fs::remove_file(&claude);
        let _ = std::fs::remove_file(&none);
    }
}
