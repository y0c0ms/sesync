//! Orphans and mirror listing, ported from daemon/hs_manage.py list_groups. A source
//! recorded in state whose file is gone stays listed as an orphan, so its leftover
//! mirrors can still be cleaned up. Renaming an orphan is refused: there is nothing to
//! rename. A mirror file is never listed as a source.

use sesync::manage::{list_groups, rename_group, Roots};
use sesync::state::{Mirror, Source, State};
use std::fs;
use std::path::PathBuf;

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-orphan-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn roots(sb: &PathBuf) -> Roots {
    Roots {
        omp: sb.join("omp").join("sessions"),
        pi: sb.join("pi").join("sessions"),
        claude: sb.join("claude").join("projects"),
        gemini: sb.join("gemini").join("tmp"),
    }
}

#[test]
fn a_source_in_state_with_no_file_is_listed_as_an_orphan_with_its_mirror() {
    let sb = sandbox("orphan");
    let r = roots(&sb);
    fs::create_dir_all(&r.omp).unwrap();
    let gone = r.omp.join("-tmp").join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    let mirror = r.pi.join("-tmp").join("mirror.jsonl");
    let mut state = State::default();
    let mut src = Source::new("omp", &gone.display().to_string());
    src.mirrors.insert("pi".into(), Mirror {
        target: "pi".into(), path: mirror.display().to_string(),
        source_hash: String::new(), installed_hash: String::new(), diverged: false,
        edit_stamp: String::new(),
        target_updated: 0.0,
    });
    state.sources.insert(gone.display().to_string(), src);

    let groups = list_groups(&r, &state);
    let g = groups.iter().find(|g| g.key == gone.display().to_string())
        .expect("an orphan must stay listed so its mirror can be cleaned");
    assert!(g.orphan, "a missing source must be flagged as an orphan");
    assert_eq!(g.mirrors.len(), 1, "its recorded mirror must be listed with it");
}

#[test]
fn renaming_an_orphan_is_refused_because_there_is_nothing_to_rename() {
    let sb = sandbox("rename-orphan");
    let r = roots(&sb);
    fs::create_dir_all(&r.omp).unwrap();
    let gone = r.omp.join("-tmp").join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    let mut state = State::default();
    state.sources.insert(gone.display().to_string(), Source::new("omp", &gone.display().to_string()));
    let g = list_groups(&r, &state).into_iter().find(|g| g.orphan).expect("orphan listed");
    let res = rename_group(&r, &g, "New", &sb.join("locks"), &sb.join("no-sesync_lock"));
    assert!(res.is_err(), "renaming a missing file must be refused");
    assert!(!gone.exists(), "a refused rename must not create the file");
}

#[test]
fn a_mirror_path_is_never_listed_as_a_source() {
    let sb = sandbox("mirror");
    let r = roots(&sb);
    let dir = r.pi.join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    let mirror = dir.join("2026-10-01T10-00-00-000Z_bbbbbbbb-1111-2222-3333-444444444444.jsonl");
    fs::write(&mirror, "{\"type\":\"session\"}\n").unwrap();
    let mut state = State::default();
    state.mirror_keys.insert(mirror.display().to_string());
    let groups = list_groups(&r, &state);
    assert!(groups.iter().all(|g| g.key != mirror.display().to_string()),
        "a file the daemon created as a mirror must not be a source");
}

#[test]
fn a_live_session_is_listed_and_not_an_orphan() {
    let sb = sandbox("live");
    let r = roots(&sb);
    let dir = r.omp.join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("2026-10-01T10-00-00-000Z_cccccccc-1111-2222-3333-444444444444.jsonl");
    fs::write(&p, "{\"type\":\"session\",\"title\":\"Live\"}\n").unwrap();
    let groups = list_groups(&r, &State::default());
    let g = groups.iter().find(|g| g.key == p.display().to_string()).expect("live session listed");
    assert!(!g.orphan);
    assert_eq!(g.title, "Live");
}

#[test]
fn a_session_lists_its_subagent_transcripts_read_only_and_they_are_not_groups() {
    let sb = sandbox("subs");
    let r = roots(&sb);
    let dir = r.claude.join("-proj");
    let parent = "ec447302-155c-4280-9c34-2b5122fcd4a4";
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("{parent}.jsonl"));
    fs::write(&p, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n").unwrap();
    let mut state = State::default();
    state.subagents.insert(parent.into(), vec!["/x/agent-a1.jsonl".into()]);
    let groups = list_groups(&r, &state);
    let g = groups.iter().find(|g| g.key == p.display().to_string()).expect("parent listed");
    assert_eq!(g.subagents, vec!["/x/agent-a1.jsonl".to_string()]);
    assert!(groups.iter().all(|g| g.key != "/x/agent-a1.jsonl"),
        "a subagent transcript must not be a conversation of its own");
}

#[test]
fn deleting_a_session_leaves_its_subagent_transcripts_alone() {
    // Subagent transcripts are read-only history; deleting the session must not remove them.
    let sb = sandbox("subs-delete");
    let r = roots(&sb);
    let dir = r.claude.join("-proj");
    let parent = "ec447302-155c-4280-9c34-2b5122fcd4a4";
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("{parent}.jsonl"));
    fs::write(&p, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n").unwrap();
    let sub = dir.join(parent).join("subagents");
    fs::create_dir_all(&sub).unwrap();
    let a = sub.join("agent-a1.jsonl");
    fs::write(&a, "keep me").unwrap();
    let mut state = State::default();
    state.subagents.insert(parent.into(), vec![a.display().to_string()]);
    let g = list_groups(&r, &state).into_iter().find(|g| g.key == p.display().to_string()).unwrap();
    rename_group(&r, &g, "Renamed", &sb.join("locks"), &sb.join("no-sesync_lock")).unwrap();
    assert_eq!(fs::read_to_string(&a).unwrap(), "keep me", "a rename must not touch subagent files");
    sesync::manage::delete_group(&r, &g, &sb.join("locks"), &sb.join("no-sesync_lock")).unwrap();
    assert!(a.exists(), "deleting the session must not delete its subagent transcripts");
}

#[test]
fn an_opencode_session_shows_its_recorded_title_not_its_id() {
    // O-penCode sources are keyed by session id and have no file on disk. The name the daemon
    // recorded at scan time is what the list must show.
    let sb = sandbox("opencode-title");
    let r = roots(&sb);
    let mut state = State::default();
    let mut src = Source::new("opencode", "ses_eb579e7e9ffefKdqb3YByMN2xG");
    src.title = "Fixing sesync session failures and locked sessions".into();
    state.sources.insert("ses_eb579e7e9ffefKdqb3YByMN2xG".into(), src);
    let groups = list_groups(&r, &state);
    let g = groups.iter().find(|g| g.key == "ses_eb579e7e9ffefKdqb3YByMN2xG").expect("listed");
    assert_eq!(g.title, "Fixing sesync session failures and locked sessions");
    assert!(!g.title.starts_with("ses_"), "the id must not be the visible name");
}

#[test]
fn an_opencode_session_without_a_recorded_title_falls_back_to_its_id() {
    let sb = sandbox("opencode-noname");
    let r = roots(&sb);
    let mut state = State::default();
    state.sources.insert("ses_x".into(), Source::new("opencode", "ses_x"));
    let groups = list_groups(&r, &state);
    let g = groups.iter().find(|g| g.key == "ses_x").expect("listed");
    assert_eq!(g.title, "ses_x", "with no recorded name, the id is the only honest label");
}
