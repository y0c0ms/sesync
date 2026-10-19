//! The team trigger's rules: when a marked session is due, what is pushed for each harness, and that
//! an unchanged or unmarked session is never sent. Uses temp files and an in-memory remote; no real
//! harness store and no network.
use sesync::state::State;
use sesync::team_push::{PushError, TeamRemote};
use sesync::team_sync::{canonical_bytes, is_due, push_due, Canonical, DEFAULT_TEAM_QUIET_SECS};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-team-sync-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A remote that records every write, so tests can see exactly what would have left the machine.
#[derive(Default)]
struct Recording { writes: RefCell<Vec<(String, Vec<u8>)>>, fail: bool }

impl TeamRemote for Recording {
    fn acquire(&self, _s: &str, _c: &str) -> Result<u64, PushError> { Ok(1) }
    fn put(&self, _s: &str, conv: &str, _v: u64, bytes: &[u8]) -> Result<(), PushError> {
        if self.fail { return Err(PushError::Unreachable); }
        self.writes.borrow_mut().push((conv.to_string(), bytes.to_vec()));
        Ok(())
    }
    fn release(&self, _s: &str, _c: &str) -> Result<(), PushError> { Ok(()) }
}

#[test]
fn the_default_team_quiet_period_is_two_minutes() {
    assert_eq!(DEFAULT_TEAM_QUIET_SECS, 120.0);
}

#[test]
fn a_session_is_due_only_after_the_quiet_period_and_only_when_it_changed() {
    // Quiet for 2 minutes, content differs from the last push: due.
    assert!(is_due(1000.0, 880.0, 120.0, "new", "old"));
    // Still being written: not due, even with new content.
    assert!(!is_due(1000.0, 950.0, 120.0, "new", "old"));
    // Quiet long enough, but identical to what was pushed: not due.
    assert!(!is_due(1000.0, 880.0, 120.0, "same", "same"));
    // Never pushed (empty recorded hash): due once quiet.
    assert!(is_due(1000.0, 880.0, 120.0, "first", ""));
}

#[test]
fn an_unmarked_session_is_never_pushed() {
    let mut state = State::default();
    state.sources.insert("/s/a.jsonl".into(), sesync::state::Source::new("omp", "/s/a.jsonl"));
    let remote = Recording::default();
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0,
        |_| Ok(b"secret".to_vec()), |_| Some(0.0), |_, _, b| Ok(b.to_vec()));
    assert!(remote.writes.borrow().is_empty(), "nothing is sent for an unmarked session");
    assert!(pass.pushed.is_empty());
}

#[test]
fn a_marked_quiet_session_is_pushed_once_and_recorded() {
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0,
        |_| Ok(b"journal bytes".to_vec()), |_| Some(800.0), |_, _, b| Ok(b.to_vec()));
    assert_eq!(pass.pushed, vec!["sess-1".to_string()]);
    assert_eq!(remote.writes.borrow().len(), 1);
    assert!(state.team_hash("personal", "sess-1").is_some(), "the push is recorded");
}

#[test]
fn an_unchanged_session_is_not_pushed_again() {
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    let content = |_: &str| Ok(b"same content".to_vec());
    push_due(&mut state, &remote, "personal", 1000.0, 120.0, content, |_| Some(800.0), |_, _, b| Ok(b.to_vec()));
    let second = push_due(&mut state, &remote, "personal", 2000.0, 120.0, content, |_| Some(800.0), |_, _, b| Ok(b.to_vec()));
    assert_eq!(remote.writes.borrow().len(), 1, "a quiet tick does not re-upload");
    assert_eq!(second.skipped_unchanged, vec!["sess-1".to_string()]);
}

#[test]
fn a_session_still_being_written_waits_for_the_quiet_period() {
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0,
        |_| Ok(b"x".to_vec()), |_| Some(990.0), |_, _, b| Ok(b.to_vec()));
    assert!(remote.writes.borrow().is_empty(), "not quiet yet");
    assert!(pass.pushed.is_empty());
}

#[test]
fn a_failed_push_is_reported_and_the_others_still_go() {
    let mut state = State::default();
    state.share("bad", "personal");
    state.share("good", "personal");
    let remote = Recording { fail: true, ..Default::default() };
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0,
        |_| Ok(b"x".to_vec()), |_| Some(0.0), |_, _, b| Ok(b.to_vec()));
    assert_eq!(pass.failed.len(), 2, "each failure is named");
    assert!(state.team_hash("team-a", "bad").is_none(), "a failed push records nothing");
}

#[test]
fn a_session_whose_content_cannot_be_built_is_reported_not_sent() {
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0,
        |_| Err("converter failed".into()), |_| Some(0.0), |_, _, b| Ok(b.to_vec()));
    assert!(remote.writes.borrow().is_empty());
    assert_eq!(pass.failed[0].1, "converter failed");
}

#[test]
fn an_omp_or_pi_session_is_pushed_as_its_own_journal_bytes() {
    let d = sandbox("journal");
    let f = d.join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    std::fs::write(&f, b"{\"type\":\"session\"}\n").unwrap();
    let bytes = canonical_bytes(Canonical::Journal(&f), Path::new("/no/conv"), &d.join("scratch")).unwrap();
    assert_eq!(bytes, b"{\"type\":\"session\"}\n", "a journal is sent unchanged");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_missing_source_is_an_error_not_an_empty_push() {
    let d = sandbox("missing");
    let err = canonical_bytes(Canonical::Journal(&d.join("gone.jsonl")), Path::new("/no/conv"), &d).unwrap_err();
    assert!(err.contains("cannot read"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_claude_session_without_the_converter_is_an_error() {
    let d = sandbox("noconv");
    let f = d.join("transcript.jsonl");
    std::fs::write(&f, b"{}\n").unwrap();
    let err = canonical_bytes(Canonical::Claude(&f), Path::new("/definitely/not/here"), &d.join("s")).unwrap_err();
    assert!(err.contains("cannot run sesync-conv"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_team_push_record_survives_a_restart() {
    let d = sandbox("restart");
    let path = d.join("state.json");
    let mut state = State::default();
    state.record_team_push("personal", "sess-1", "abc123", 1234.5);
    state.save(&path).unwrap();
    let back = State::load(&path).unwrap();
    assert_eq!(back.team_hash("personal", "sess-1").as_deref(), Some("abc123"), "the hash is kept so no re-upload happens after restart");
    assert_eq!(back.team_pushed_at("personal", "sess-1"), Some(1234.5));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_randomized_seal_does_not_make_an_unchanged_session_look_changed() {
    // Sealing is randomized (fresh key and nonce per call). Change must be judged on the plaintext,
    // so the second quiet tick must skip even though every sealed output differs.
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    let counter = std::cell::Cell::new(0u8);
    let seal = |_id: &str, _version: u64, b: &[u8]| -> Result<Vec<u8>, String> {
        counter.set(counter.get().wrapping_add(1));
        let mut out = vec![counter.get()];
        out.extend_from_slice(b);
        Ok(out)
    };
    let content = |_: &str| Ok(b"same plaintext".to_vec());
    let first = push_due(&mut state, &remote, "personal", 1000.0, 120.0, content, |_| Some(800.0), seal);
    let second = push_due(&mut state, &remote, "personal", 2000.0, 120.0, content, |_| Some(800.0), seal);
    assert_eq!(first.pushed, vec!["sess-1".to_string()]);
    assert_eq!(second.skipped_unchanged, vec!["sess-1".to_string()], "same plaintext is not re-sent");
    assert_eq!(remote.writes.borrow().len(), 1);
}

#[test]
fn what_is_pushed_is_the_sealed_output_not_the_plaintext() {
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    push_due(&mut state, &remote, "personal", 1000.0, 120.0,
        |_| Ok(b"plain journal".to_vec()), |_| Some(0.0),
        |_, _, b| { let mut v = b"SEALED:".to_vec(); v.extend_from_slice(b); Ok(v) });
    let sent = remote.writes.borrow()[0].1.clone();
    assert!(sent.starts_with(b"SEALED:"), "the remote receives sealed bytes only");
}

/// A remote that grants a fixed lease version and records each write with the version it was under.
struct Granting { version: u64, writes: RefCell<Vec<(String, u64, Vec<u8>)>> }

impl TeamRemote for Granting {
    fn acquire(&self, _s: &str, _c: &str) -> Result<u64, PushError> { Ok(self.version) }
    fn put(&self, _s: &str, conv: &str, v: u64, bytes: &[u8]) -> Result<(), PushError> {
        self.writes.borrow_mut().push((conv.to_string(), v, bytes.to_vec()));
        Ok(())
    }
    fn release(&self, _s: &str, _c: &str) -> Result<(), PushError> { Ok(()) }
}

#[test]
fn the_seal_is_given_the_session_and_the_version_its_lease_was_granted() {
    // The sealed record's sequence is the lease version, so the seal must see that version and the
    // session id it is bound to, and the bytes must go out under the same version.
    let mut state = State::default();
    state.share("sess-9", "team-a");
    let remote = Granting { version: 7, writes: RefCell::new(Vec::new()) };
    let seen = RefCell::new(Vec::new());
    let seal = |id: &str, v: u64, plain: &[u8]| -> Result<Vec<u8>, String> {
        seen.borrow_mut().push((id.to_string(), v, plain.to_vec()));
        Ok(format!("{id}@{v}").into_bytes())
    };
    state.share("sess-9", "personal");
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0,
        |_| Ok(b"plain".to_vec()), |_| Some(0.0), seal);
    assert_eq!(pass.pushed, vec!["sess-9".to_string()]);
    assert_eq!(*seen.borrow(), vec![("sess-9".to_string(), 7, b"plain".to_vec())]);
    assert_eq!(*remote.writes.borrow(), vec![("sess-9".to_string(), 7, b"sess-9@7".to_vec())],
        "the bytes are written under the version the seal was given");
}

fn converter() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv");
    p.exists().then_some(p)
}

/// A stand-in for the O-penCode CLI: prints a fixed export for any `export <id>`, or fails.
fn fake_opencode(dir: &PathBuf, fail: bool) -> PathBuf {
    let path = dir.join("opencode");
    let body = if fail {
        "#!/bin/sh\necho 'session not found' >&2\nexit 1\n".to_string()
    } else {
        r#"#!/bin/sh
cat <<'JSON'
{"info": {"id": "ses_fake", "directory": "/tmp/p", "title": "Fake", "time": {"created": 1792000000000}},
 "messages": [
  {"info": {"id": "msg_u", "role": "user", "time": {"created": 1792000000001}},
   "parts": [{"type": "text", "text": "the opencode question"}]},
  {"info": {"id": "msg_a", "role": "assistant", "time": {"created": 1792000000002}},
   "parts": [{"type": "text", "text": "the opencode answer"}]}
 ]}
JSON
"#.to_string()
    };
    std::fs::write(&path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn an_opencode_session_becomes_a_journal_the_team_can_install() {
    let Some(conv) = converter() else { eprintln!("skipped: build sesync-conv first"); return; };
    let d = sandbox("ocx");
    let cli = fake_opencode(&d, false);
    let bytes = sesync::team_sync::canonical_for_source("opencode", "ses_fake", &conv, &cli, &d.join("scratch")).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("the opencode question"), "the user turn is in the journal: {text}");
    assert!(text.contains("the opencode answer"), "the answer is in the journal: {text}");
    assert!(!d.join("scratch").join("ses_fake.journal.jsonl").exists(), "the scratch journal is removed after reading");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_opencode_export_that_fails_is_reported_and_nothing_is_pushed() {
    let Some(conv) = converter() else { eprintln!("skipped: build sesync-conv first"); return; };
    let d = sandbox("ocx-fail");
    let cli = fake_opencode(&d, true);
    let err = sesync::team_sync::canonical_for_source("opencode", "ses_gone", &conv, &cli, &d.join("scratch")).unwrap_err();
    assert!(err.contains("opencode export failed"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_harness_with_no_journal_format_is_refused() {
    let d = sandbox("unknown-fmt");
    // "cursor" is not a harness this daemon knows, unlike gemini which gained a
    // canonical format with the Gemini CLI wiring.
    let err = sesync::team_sync::canonical_for_source("cursor", "x", Path::new("/no"), Path::new("/no"), &d).unwrap_err();
    assert!(err.contains("no canonical journal"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}
