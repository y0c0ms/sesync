//! Mirror install, ported from daemon/sesync.py install_jsonl and
//! _install_jsonl_sesync_conv. Runs the real sesync_conv binary. Writes only inside a
//! private temp store; the real omp/pi/claude stores are never touched.

use sesync::install::{install_journal, InstallTarget};
use std::fs;
use std::path::PathBuf;

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-install-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// A minimal omp journal: header plus one user message.
fn journal(dir: &PathBuf) -> PathBuf {
    journal_at(dir, "/tmp/proj")
}

/// The same with an explicit cwd, for bucket assertions.
fn journal_at(dir: &PathBuf, cwd: &str) -> PathBuf {
    let p = dir.join("src.jsonl");
    let id = "aaaaaaaa-1111-2222-3333-444444444444";
    let body = format!(
        "{}\n{}\n",
        format!(r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-10-01T10:00:00.000Z","cwd":"{cwd}"}}"#),
        r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"user","content":"hello install"}}"#,
    );
    fs::write(&p, body).unwrap();
    p
}

#[test]
fn pi_mirror_is_written_with_the_title_and_a_stable_name() {
    let d = sandbox("pi");
    let src = journal(&d);
    let store = d.join("pi-agent");
    let out = install_journal(&sesync_conv(), &src, InstallTarget::Pi { agent_dir: store.clone() },
                              "/tmp/proj", "mirror-id-1", "Mirror title").unwrap();
    assert!(out.starts_with(&store), "the mirror must land inside the given store");
    let text = fs::read_to_string(&out).unwrap();
    assert!(text.contains("Mirror title"), "the title must be written into the mirror");
}

#[test]
fn reinstalling_replaces_the_same_mirror_not_a_copy() {
    let d = sandbox("replace");
    let src = journal(&d);
    let store = d.join("pi-agent");
    let t = || InstallTarget::Pi { agent_dir: store.clone() };
    let a = install_journal(&sesync_conv(), &src, t(), "/tmp/proj", "mirror-id-2", "First").unwrap();
    let b = install_journal(&sesync_conv(), &src, t(), "/tmp/proj", "mirror-id-2", "Second").unwrap();
    assert_eq!(a, b, "same mirror id must map to the same file");
    let count = walk_jsonl(&store).len();
    assert_eq!(count, 1, "a re-install must not leave a second copy");
    assert!(fs::read_to_string(&b).unwrap().contains("Second"));
}

fn walk_jsonl(root: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk_jsonl(&p));
            } else if p.extension().map_or(false, |x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn a_failed_converter_is_an_error_and_writes_nothing() {
    let d = sandbox("fail");
    let src = d.join("garbage.jsonl");
    fs::write(&src, "not a journal\n").unwrap();
    let store = d.join("pi-agent");
    let r = install_journal(&sesync_conv(), &src, InstallTarget::Pi { agent_dir: store.clone() },
                            "/tmp", "mirror-id-3", "t");
    assert!(r.is_err(), "a converter failure must surface");
    assert!(walk_jsonl(&store).is_empty(), "nothing may be written when conversion fails");
}

#[test]
fn an_older_mirror_with_a_different_timestamp_is_removed() {
    // The Python code removes any file ending in <mirror_id>.jsonl, because the
    // timestamp prefix can differ between runs. Plant an old copy with a
    // different prefix; after install it must be gone and only one file remains.
    let d = sandbox("oldts");
    let src = journal(&d);
    let store = d.join("pi-agent");
    let t = || InstallTarget::Pi { agent_dir: store.clone() };
    let first = install_journal(&sesync_conv(), &src, t(), "/tmp/proj", "mirror-id-4", "First").unwrap();
    let bucket = first.parent().unwrap().to_path_buf();
    let stale = bucket.join("2020-01-01T00-00-00-000Z_mirror-id-4.jsonl");
    fs::write(&stale, "stale").unwrap();
    install_journal(&sesync_conv(), &src, t(), "/tmp/proj", "mirror-id-4", "Second").unwrap();
    assert!(!stale.exists(), "the stale copy with an old timestamp must be removed");
    assert_eq!(walk_jsonl(&store).len(), 1, "exactly one mirror must remain");
}

#[test]
fn an_omp_mirror_lands_in_the_home_relative_bucket() {
    // omp lists a project's sessions from its home-relative bucket. The daemon once passed
    // the store's parent as `--home`, which put every mirror in a bucket omp never reads.
    let d = sandbox("omp-bucket");
    let home = d.join("home");
    let cwd = home.join("proj");
    fs::create_dir_all(&cwd).unwrap();
    let src = journal_at(&d, cwd.to_str().unwrap());
    let store = d.join("omp-agent");
    let out = install_journal(&sesync_conv(), &src,
                              InstallTarget::Omp { agent_dir: store.clone(), home: home.clone() },
                              "/", "mirror-id-omp", "T").unwrap();
    assert_eq!(out.parent().unwrap().file_name().unwrap(), "-proj",
               "the mirror must sit in the bucket omp actually lists: {}", out.display());
}

#[test]
fn a_claude_mirror_lands_under_projects_with_a_uuid_name() {
    // The Claude store is <claude>/projects/<bucket>/<uuid>.jsonl. The mirror name
    // must be a bare uuid, the form Claude Code and claude_sessions() expect.
    let d = sandbox("claude");
    let src = journal(&d);
    let claude = d.join("claude");
    let out = sesync::install::install_journal(
        &sesync_conv(), &src,
        InstallTarget::Claude { claude_dir: claude.clone() },
        "/tmp/proj", "5e55a0b1-7c3e-4d2a-9f1b-0a12ab34cd56", "Claude mirror").unwrap();
    assert!(out.starts_with(claude.join("projects")), "{}", out.display());
    let name = out.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.ends_with(".jsonl"));
    let stem = name.trim_end_matches(".jsonl");
    assert!(uuid::Uuid::parse_str(stem).is_ok(), "the file must be named by a uuid, got {stem}");
}
