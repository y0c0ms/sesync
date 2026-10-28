//! The team trigger's rules: when a marked session is due, what is pushed, and the guard that reads the
//! stored team copy under the lease before it writes. Uses temp files and in-memory remotes; no real
//! harness store and no network. Changes are judged on completed turns, so the fixtures below are real
//! journals with completed turns (`journal_of`), not arbitrary bytes.
use sesync::harness::{Location, Locations, Tools};
use sesync::state::{ConflictKind, State, TeamConflict};
use sesync::team_push::{PushError, TeamRemote};
use sesync::team_sync::{canonical_for_source, is_due, push_due, Codec, ResealCheck, TeamPass, DEFAULT_TEAM_QUIET_SECS};
use sesync::turns::{fingerprint, turns_of};
use sesync_team::membership::Device;
use sesync_team::signed_records::{seal_record, sequence_of};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The canonical bytes for a `fmt` session, built the way the team pass builds them: `conv` is the
/// converter, `cli` the harness's own command when the test provides one.
fn canonical(fmt: &str, key: &Path, conv: &Path, cli: Option<&Path>, scratch: &Path) -> Result<Vec<u8>, String> {
    let mut locations = Locations::default();
    if let Some(cli) = cli {
        locations.set(fmt, Location { root: PathBuf::from("/no/store"), cli: Some(cli.to_path_buf()) });
    }
    let home = PathBuf::from("/no/home");
    let key = key.display().to_string();
    canonical_for_source(fmt, &key, &locations, &Tools { home: &home, sesync_conv: conv }, scratch)
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-team-sync-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// An omp journal whose completed turns are `(prompt, answer)` pairs, in order.
fn journal_of(turns: &[(&str, &str)]) -> Vec<u8> {
    let mut out = String::from("{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"cwd\":\"/tmp\"}\n");
    let mut parent = "null".to_string();
    for (i, (prompt, answer)) in turns.iter().enumerate() {
        let (u, a) = (format!("u{i}"), format!("a{i}"));
        out.push_str(&format!("{{\"type\":\"message\",\"id\":\"{u}\",\"parentId\":{parent},\"message\":{{\"role\":\"user\",\"content\":\"{prompt}\"}}}}\n"));
        out.push_str(&format!("{{\"type\":\"message\",\"id\":\"{a}\",\"parentId\":\"{u}\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{answer}\"}}]}}}}\n"));
        parent = format!("\"{a}\"");
    }
    out.into_bytes()
}

/// A journal with one completed turn.
fn journal(answer: &str) -> Vec<u8> {
    journal_of(&[("question", answer)])
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

/// A codec that seals with `seal`. The stored copy is opened as its own bytes, so these tests need no
/// real records; the guard is checked against the text itself.
struct Seal<S>(S);

impl<S: FnMut(&str, u64, &[u8]) -> Result<Vec<u8>, String>> Codec for Seal<S> {
    fn seal(&mut self, conversation: &str, version: u64, plain: &[u8]) -> Result<Vec<u8>, String> {
        (self.0)(conversation, version, plain)
    }
    fn open(&mut self, _conversation: &str, _floor: u64, stored: &[u8]) -> Result<Vec<u8>, String> {
        Ok(stored.to_vec())
    }
}

fn echo_seal(_: &str, _: u64, b: &[u8]) -> Result<Vec<u8>, String> { Ok(b.to_vec()) }

fn echo() -> Seal<fn(&str, u64, &[u8]) -> Result<Vec<u8>, String>> { Seal(echo_seal) }

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
        |_| Ok(journal("secret")), |_| Some(0.0), &mut echo(), None);
    assert!(remote.writes.borrow().is_empty(), "nothing is sent for an unmarked session");
    assert!(pass.pushed.is_empty());
}

#[test]
fn a_marked_quiet_session_is_pushed_once_and_recorded() {
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0,
        |_| Ok(journal("first answer")), |_| Some(800.0), &mut echo(), None);
    assert_eq!(pass.pushed, vec!["sess-1".to_string()]);
    assert_eq!(remote.writes.borrow().len(), 1);
    assert!(state.team_hash("personal", "sess-1").is_some(), "the push is recorded");
    assert_eq!(state.team_record("personal", "sess-1").turns, fingerprint(&turns_of(&journal("first answer")).unwrap()),
        "the completed turns it pushed are recorded");
}

#[test]
fn an_unchanged_session_is_not_pushed_again() {
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    let content = |_: &str| Ok(journal("same content"));
    push_due(&mut state, &remote, "personal", 1000.0, 120.0, content, |_| Some(800.0), &mut echo(), None);
    let second = push_due(&mut state, &remote, "personal", 2000.0, 120.0, content, |_| Some(800.0), &mut echo(), None);
    assert_eq!(remote.writes.borrow().len(), 1, "a quiet tick does not re-upload");
    assert_eq!(second.skipped_unchanged, vec!["sess-1".to_string()]);
}

#[test]
fn a_session_still_being_written_waits_for_the_quiet_period() {
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0,
        |_| Ok(journal("x")), |_| Some(990.0), &mut echo(), None);
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
        |_| Ok(journal("x")), |_| Some(0.0), &mut echo(), None);
    assert_eq!(pass.failed.len(), 2, "each failure is named");
    assert!(state.team_hash("team-a", "bad").is_none(), "a failed push records nothing");
}

#[test]
fn a_session_whose_content_cannot_be_built_is_reported_not_sent() {
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0,
        |_| Err("converter failed".into()), |_| Some(0.0), &mut echo(), None);
    assert!(remote.writes.borrow().is_empty());
    assert_eq!(pass.failed[0].1, "converter failed");
}

#[test]
fn an_unreadable_journal_is_reported_and_nothing_is_sent() {
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0,
        |_| Ok(b"not a journal\n".to_vec()), |_| Some(0.0), &mut echo(), None);
    assert!(remote.writes.borrow().is_empty(), "the guard needs turns it can read");
    assert!(pass.failed[0].1.contains("cannot read the journal"), "{pass:?}");
}

#[test]
fn an_omp_or_pi_session_is_pushed_as_its_own_journal_bytes() {
    let d = sandbox("journal");
    let f = d.join("2026-10-01T10-00-00-000Z_aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    std::fs::write(&f, b"{\"type\":\"session\"}\n").unwrap();
    let bytes = canonical("omp", &f, Path::new("/no/conv"), None, &d.join("scratch")).unwrap();
    assert_eq!(bytes, b"{\"type\":\"session\"}\n", "a journal is sent unchanged");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_missing_source_is_an_error_not_an_empty_push() {
    let d = sandbox("missing");
    let err = canonical("omp", &d.join("gone.jsonl"), Path::new("/no/conv"), None, &d).unwrap_err();
    assert!(err.contains("cannot read"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_claude_session_without_the_converter_is_an_error() {
    let d = sandbox("noconv");
    let f = d.join("transcript.jsonl");
    std::fs::write(&f, b"{}\n").unwrap();
    let err = canonical("claude", &f, Path::new("/definitely/not/here"), None, &d.join("s")).unwrap_err();
    assert!(err.contains("cannot run sesync-conv"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_team_push_record_survives_a_restart() {
    let d = sandbox("restart");
    let path = d.join("state.json");
    let mut state = State::default();
    state.record_team_push("personal", "sess-1", "abc123", 1234.5);
    state.set_team_turns("personal", "sess-1", "turns-1");
    state.save(&path).unwrap();
    let back = State::load(&path).unwrap();
    assert_eq!(back.team_hash("personal", "sess-1").as_deref(), Some("abc123"), "the hash is kept so no re-upload happens after restart");
    assert_eq!(back.team_pushed_at("personal", "sess-1"), Some(1234.5));
    assert_eq!(back.team_record("personal", "sess-1").turns, "turns-1", "the turn fingerprint is kept too");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_randomized_seal_does_not_make_an_unchanged_session_look_changed() {
    // Sealing is randomized (fresh key and nonce per call). Change must be judged on the completed turns,
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
    let content = |_: &str| Ok(journal("same plaintext"));
    let first = push_due(&mut state, &remote, "personal", 1000.0, 120.0, content, |_| Some(800.0), &mut Seal(seal), None);
    let second = push_due(&mut state, &remote, "personal", 2000.0, 120.0, content, |_| Some(800.0), &mut Seal(seal), None);
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
        |_| Ok(journal("plain answer")), |_| Some(0.0),
        &mut Seal(|_: &str, _: u64, b: &[u8]| { let mut v = b"SEALED:".to_vec(); v.extend_from_slice(b); Ok(v) }), None);
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
    let plain = journal("plain");
    state.share("sess-9", "personal");
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0,
        |_| Ok(plain.clone()), |_| Some(0.0), &mut Seal(seal), None);
    assert_eq!(pass.pushed, vec!["sess-9".to_string()]);
    assert_eq!(*seen.borrow(), vec![("sess-9".to_string(), 7, plain)]);
    assert_eq!(*remote.writes.borrow(), vec![("sess-9".to_string(), 7, b"sess-9@7".to_vec())],
        "the bytes are written under the version the seal was given");
}

#[cfg(unix)]
fn converter() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv");
    p.exists().then_some(p)
}

/// A stand-in for the O-penCode CLI: prints a fixed export for any `export <id>`, or fails.
/// A shell script, so Unix only.
#[cfg(unix)]
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

#[cfg(unix)]
#[test]
fn an_opencode_session_becomes_a_journal_the_team_can_install() {
    let Some(conv) = converter() else { eprintln!("skipped: build sesync-conv first"); return; };
    let d = sandbox("ocx");
    let cli = fake_opencode(&d, false);
    let bytes = canonical("opencode", Path::new("ses_fake"), &conv, Some(&cli), &d.join("scratch")).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("the opencode question"), "the user turn is in the journal: {text}");
    assert!(text.contains("the opencode answer"), "the answer is in the journal: {text}");
    assert!(!d.join("scratch").join("ses_fake.journal.jsonl").exists(), "the scratch journal is removed after reading");
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(unix)]
#[test]
fn an_opencode_export_that_fails_is_reported_and_nothing_is_pushed() {
    let Some(conv) = converter() else { eprintln!("skipped: build sesync-conv first"); return; };
    let d = sandbox("ocx-fail");
    let cli = fake_opencode(&d, true);
    let err = canonical("opencode", Path::new("ses_gone"), &conv, Some(&cli), &d.join("scratch")).unwrap_err();
    assert!(err.contains("opencode export failed"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_harness_with_no_journal_format_is_refused() {
    let d = sandbox("unknown-fmt");
    // "cursor" is not a harness this daemon knows; a session of an unknown harness is refused.
    let err = canonical("cursor", Path::new("x"), Path::new("/no"), None, &d).unwrap_err();
    assert!(err.contains("no canonical journal"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

// ----- a re-seal checks the stored record under the lease ---------------------------------------

/// A server where another member's write can land between a read and a lease. `stored` answers
/// from `record`; the next `acquire` lets `rival` land first. Every call is logged, in order.
#[derive(Default)]
struct Server {
    record: RefCell<Option<Vec<u8>>>,
    rival: RefCell<Option<Vec<u8>>>,
    writes: RefCell<Vec<Vec<u8>>>,
    log: RefCell<Vec<&'static str>>,
}

impl TeamRemote for Server {
    fn acquire(&self, _s: &str, _c: &str) -> Result<u64, PushError> {
        self.log.borrow_mut().push("acquire");
        if let Some(rival) = self.rival.borrow_mut().take() {
            *self.record.borrow_mut() = Some(rival);
        }
        Ok(1)
    }
    fn put(&self, _s: &str, _c: &str, _v: u64, bytes: &[u8]) -> Result<(), PushError> {
        self.log.borrow_mut().push("put");
        self.writes.borrow_mut().push(bytes.to_vec());
        *self.record.borrow_mut() = Some(bytes.to_vec());
        Ok(())
    }
    fn release(&self, _s: &str, _c: &str) -> Result<(), PushError> {
        self.log.borrow_mut().push("release");
        Ok(())
    }
    fn stored(&self, _s: &str, _c: &str) -> Result<Option<Vec<u8>>, PushError> {
        self.log.borrow_mut().push("stored");
        Ok(self.record.borrow().clone())
    }
}

/// Alice's copy is unchanged since her push and the member list moves to 2. Bob's newer record lands
/// after her check but before her lease, so her re-seal must not replace it.
#[test]
fn a_reseal_checks_the_stored_record_under_the_lease_so_a_newer_push_is_kept() {
    let (alice, bob) = (Device::generate(), Device::generate());
    let server = Server::default();
    let mut state = State::default();
    state.share("conv-1", "personal");
    let plain = journal("alice's turn");

    // Pass one, at list 1: Alice's copy is pushed, and her record is stored.
    let seal_at_1 = |id: &str, v: u64, p: &[u8]| seal_record(&alice, 1, v, "personal", id, p).map_err(|e| format!("{e:?}"));
    let pass = push_due(&mut state, &server, "personal", 1000.0, 120.0,
        |_| Ok(plain.clone()), |_| Some(0.0), &mut Seal(seal_at_1), None);
    assert_eq!(pass.pushed, vec!["conv-1".to_string()], "{pass:?}");

    // Pass two, at list 2: nothing changed, so only a re-seal could write. Bob's push lands first.
    *server.rival.borrow_mut() = Some(seal_record(&bob, 2, 2, "personal", "conv-1", &journal("newer, from bob")).unwrap());
    server.log.borrow_mut().clear();
    let seal_at_2 = |id: &str, v: u64, p: &[u8]| seal_record(&alice, 2, v, "personal", id, p).map_err(|e| format!("{e:?}"));
    let check = ResealCheck { list: 2, author: alice.public_key() };
    let pass = push_due(&mut state, &server, "personal", 2000.0, 120.0,
        |_| Ok(plain.clone()), |_| Some(0.0), &mut Seal(seal_at_2), Some(&check));
    assert!(pass.resealed.is_empty() && pass.pushed.is_empty() && pass.failed.is_empty(), "{pass:?}");
    assert_eq!(pass.skipped_unchanged, vec!["conv-1".to_string()], "{pass:?}");
    assert_eq!(server.writes.borrow().len(), 1, "Alice wrote nothing over Bob's record");
    assert_eq!(*server.log.borrow(), vec!["acquire", "stored", "release"], "the lease is taken before the check");
}

// ----- the push guard: the stored team copy is read under the lease and compared -----------------

/// A team server that keeps one record per conversation and logs its calls, so a test can see what the
/// guard read and what it wrote. `acquire` grants the next sequence, as the real server does.
#[derive(Default)]
struct Shelf {
    records: RefCell<BTreeMap<String, Vec<u8>>>,
    writes: RefCell<Vec<(String, u64)>>,
    log: RefCell<Vec<String>>,
}

impl Shelf {
    fn stored_seq(&self, conv: &str) -> u64 {
        self.records.borrow().get(conv).and_then(|r| sequence_of(r)).unwrap_or(0)
    }
    fn calls(&self, what: &str) -> usize {
        self.log.borrow().iter().filter(|c| c.as_str() == what).count()
    }
    fn stored(&self, conv: &str) -> Vec<u8> {
        self.records.borrow()[conv].clone()
    }
}

impl TeamRemote for Shelf {
    fn acquire(&self, _s: &str, c: &str) -> Result<u64, PushError> {
        self.log.borrow_mut().push("acquire".into());
        Ok(self.stored_seq(c) + 1)
    }
    fn put(&self, _s: &str, c: &str, v: u64, bytes: &[u8]) -> Result<(), PushError> {
        self.log.borrow_mut().push("put".into());
        self.writes.borrow_mut().push((c.to_string(), v));
        self.records.borrow_mut().insert(c.to_string(), bytes.to_vec());
        Ok(())
    }
    fn release(&self, _s: &str, _c: &str) -> Result<(), PushError> {
        self.log.borrow_mut().push("release".into());
        Ok(())
    }
    fn stored(&self, _s: &str, c: &str) -> Result<Option<Vec<u8>>, PushError> {
        self.log.borrow_mut().push("stored".into());
        Ok(self.records.borrow().get(c).cloned())
    }
}

/// A member's codec for these tests. It seals real v2 records for `author` under list `list`, and opens
/// a stored copy by the text it was given (`known`), which stands for the decryption the daemon does.
/// A record it did not seal or learn of cannot be opened.
struct Member { author: Device, list: u64, known: BTreeMap<Vec<u8>, Vec<u8>> }

impl Member {
    fn new(author: Device) -> Member {
        Member { author, list: 1, known: BTreeMap::new() }
    }
    /// Seal `text` as this member would for `conv` at `seq`, and return the record.
    fn record(&mut self, conv: &str, seq: u64, text: &[u8]) -> Vec<u8> {
        let rec = seal_record(&self.author, self.list, seq, "personal", conv, text).unwrap();
        self.known.insert(rec.clone(), text.to_vec());
        rec
    }
}

impl Codec for Member {
    fn seal(&mut self, conversation: &str, version: u64, plain: &[u8]) -> Result<Vec<u8>, String> {
        Ok(self.record(conversation, version, plain))
    }
    fn open(&mut self, _conversation: &str, _floor: u64, stored: &[u8]) -> Result<Vec<u8>, String> {
        self.known.get(stored).cloned().ok_or_else(|| "sealed under an older member list".to_string())
    }
}

/// A teammate's record of `text` at `seq`, already on the server as `conv`. Alice is given its text.
fn teammate_copy(shelf: &Shelf, alice: &mut Member, conv: &str, seq: u64, text: &[u8]) {
    let mut bob = Member::new(Device::generate());
    let rec = bob.record(conv, seq, text);
    alice.known.insert(rec.clone(), text.to_vec());
    shelf.records.borrow_mut().insert(conv.to_string(), rec);
}

/// One pass of alice's daemon at `now`, with her local journal `text`.
fn run_pass(state: &mut State, shelf: &Shelf, alice: &mut Member, text: &[u8], now: f64) -> TeamPass {
    let text = text.to_vec();
    push_due(state, shelf, "personal", now, 120.0, |_| Ok(text.clone()), |_| Some(0.0), alice, None)
}

#[test]
fn a_behind_machine_never_replaces_newer_turns() {
    // The team holds two turns; this machine has only the first. Its push used to replace the team copy.
    let shelf = Shelf::default();
    let mut alice = Member::new(Device::generate());
    teammate_copy(&shelf, &mut alice, "conv-1", 1, &journal_of(&[("q1", "a1"), ("q2", "a2 from bob")]));
    let before = shelf.stored("conv-1");
    let mut state = State::default();
    state.share("conv-1", "personal");

    let pass = run_pass(&mut state, &shelf, &mut alice, &journal_of(&[("q1", "a1")]), 1000.0);
    assert!(shelf.writes.borrow().is_empty(), "nothing is written over the newer team copy");
    assert_eq!(shelf.stored("conv-1"), before, "the team copy is untouched");
    assert_eq!(pass.refused.len(), 1, "{pass:?}");
    assert!(pass.refused[0].1.contains("1 more completed turn"), "{pass:?}");
    let c = state.team_conflict("personal", "conv-1").expect("the conflict is recorded for the TUI");
    assert_eq!((c.kind, c.count), (ConflictKind::Behind, 1));
}

#[test]
fn a_machine_ahead_of_the_team_copy_pushes_even_when_the_team_copy_is_newer_than_it_last_saw() {
    let shelf = Shelf::default();
    let mut alice = Member::new(Device::generate());
    teammate_copy(&shelf, &mut alice, "conv-1", 3, &journal_of(&[("q1", "a1")]));
    let mut state = State::default();
    state.share("conv-1", "personal");
    // This machine saw sequence 1 of the record before: the team's 3 is newer than anything it accepted.
    state.raise_seen("personal", "conv-1", 1);

    let pass = run_pass(&mut state, &shelf, &mut alice, &journal_of(&[("q1", "a1"), ("q2", "a2")]), 1000.0);
    assert_eq!(pass.pushed, vec!["conv-1".to_string()], "{pass:?}");
    assert_eq!(shelf.writes.borrow().clone(), vec![("conv-1".to_string(), 4)], "written as the next version");
    assert!(state.team_conflict("personal", "conv-1").is_none());
}

#[test]
fn a_diverged_copy_is_never_written_over() {
    let shelf = Shelf::default();
    let mut alice = Member::new(Device::generate());
    teammate_copy(&shelf, &mut alice, "conv-1", 1, &journal_of(&[("q1", "a1"), ("q2", "bob's answer")]));
    let mut state = State::default();
    state.share("conv-1", "personal");

    let pass = run_pass(&mut state, &shelf, &mut alice, &journal_of(&[("q1", "a1"), ("q2", "alice's answer")]), 1000.0);
    assert!(shelf.writes.borrow().is_empty(), "a diverged copy is not written over");
    assert_eq!(pass.refused.len(), 1, "{pass:?}");
    let c = state.team_conflict("personal", "conv-1").unwrap();
    assert_eq!((c.kind, c.count, c.local_new, c.team_new), (ConflictKind::Diverged, 1, 1, 1));
}

#[test]
fn a_metadata_only_change_never_pushes() {
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    let base = journal_of(&[("q1", "a1")]);
    push_due(&mut state, &remote, "personal", 1000.0, 120.0, |_| Ok(base.clone()), |_| Some(0.0), &mut echo(), None);
    assert_eq!(remote.writes.borrow().len(), 1);

    let mut grown = base.clone();
    grown.extend_from_slice(b"{\"type\":\"session_info\",\"id\":\"si\",\"parentId\":\"a0\",\"name\":\"renamed\"}\n");
    grown.extend_from_slice(b"{\"type\":\"model_change\",\"id\":\"mc\",\"parentId\":\"a0\",\"model\":\"other\"}\n");
    let later = push_due(&mut state, &remote, "personal", 5000.0, 120.0, |_| Ok(grown.clone()), |_| Some(4000.0), &mut echo(), None);
    assert_eq!(remote.writes.borrow().len(), 1, "metadata appended to a journal is not a change");
    assert_eq!(later.skipped_unchanged, vec!["sess-1".to_string()]);
}

#[test]
fn a_prompt_without_a_reply_is_not_pushed() {
    let mut state = State::default();
    state.share("sess-1", "personal");
    let remote = Recording::default();
    let waiting = {
        let mut j = journal_of(&[("q1", "a1")]);
        j.extend_from_slice(b"{\"type\":\"message\",\"id\":\"u9\",\"parentId\":\"a0\",\"message\":{\"role\":\"user\",\"content\":\"still there?\"}}\n");
        j
    };
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0, |_| Ok(waiting.clone()), |_| Some(0.0), &mut echo(), None);
    assert_eq!(pass.pushed, vec!["sess-1".to_string()], "the completed turn is pushed; the trailing prompt is not part of it");
    assert_eq!(remote.writes.borrow().len(), 1);

    let only_a_prompt = b"{\"type\":\"session\",\"id\":\"n\"}\n{\"type\":\"message\",\"id\":\"u1\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n".to_vec();
    let mut fresh = State::default();
    fresh.share("sess-2", "personal");
    let pass = push_due(&mut fresh, &remote, "personal", 1000.0, 120.0, |_| Ok(only_a_prompt.clone()), |_| Some(0.0), &mut echo(), None);
    assert_eq!(pass.waiting_for_turn, vec!["sess-2".to_string()], "no completed turn: nothing to send yet");
    assert_eq!(remote.writes.borrow().len(), 1, "nothing was written for it");
}

#[test]
fn the_first_pass_after_an_upgrade_records_the_turns_without_pushing() {
    // A state file from before turns existed: the recorded bytes are the bytes the team holds.
    let text = journal_of(&[("q1", "a1")]);
    let mut state = State::default();
    state.share("sess-1", "personal");
    state.record_team_push("personal", "sess-1", &sesync::mirror::file_hash_bytes(&text), 100.0);
    let remote = Recording::default();
    let pass = push_due(&mut state, &remote, "personal", 1000.0, 120.0, |_| Ok(text.clone()), |_| Some(0.0), &mut echo(), None);
    assert!(remote.writes.borrow().is_empty(), "the upgrade does not re-push a session");
    assert_eq!(pass.skipped_unchanged, vec!["sess-1".to_string()]);
    assert_eq!(state.team_record("personal", "sess-1").turns, fingerprint(&turns_of(&text).unwrap()));
}

#[test]
fn an_identical_team_copy_is_recorded_without_a_write() {
    let shelf = Shelf::default();
    let text = journal_of(&[("q1", "a1")]);
    let mut alice = Member::new(Device::generate());
    // Another machine pushed exactly this conversation first.
    teammate_copy(&shelf, &mut alice, "conv-1", 1, &text);
    let mut state = State::default();
    state.share("conv-1", "personal");

    let pass = run_pass(&mut state, &shelf, &mut alice, &text, 1000.0);
    assert!(shelf.writes.borrow().is_empty(), "an identical copy is left alone");
    assert_eq!(pass.skipped_unchanged, vec!["conv-1".to_string()]);
    assert_eq!(state.team_record("personal", "conv-1").turns, fingerprint(&turns_of(&text).unwrap()));
    assert!(state.team_conflict("personal", "conv-1").is_none());
}

#[test]
fn a_stored_copy_that_cannot_be_opened_is_refused_with_its_reason() {
    let shelf = Shelf::default();
    let mut alice = Member::new(Device::generate());
    // A record alice has never been given the text of: she cannot read it, so she cannot compare.
    let mut bob = Member::new(Device::generate());
    shelf.records.borrow_mut().insert("conv-1".into(), bob.record("conv-1", 1, &journal("bob")));
    let mut state = State::default();
    state.share("conv-1", "personal");

    let pass = run_pass(&mut state, &shelf, &mut alice, &journal("mine"), 1000.0);
    assert!(shelf.writes.borrow().is_empty(), "an unverifiable copy is not written over");
    assert!(pass.refused[0].1.contains("older member list"), "the reason is named: {pass:?}");
    assert_eq!(state.team_conflict("personal", "conv-1").unwrap().kind, ConflictKind::Unverified);
}

#[test]
fn a_held_conversation_is_not_checked_again_until_its_turns_change() {
    let shelf = Shelf::default();
    let mut alice = Member::new(Device::generate());
    teammate_copy(&shelf, &mut alice, "conv-1", 1, &journal_of(&[("q1", "a1"), ("q2", "a2 from bob")]));
    let mut state = State::default();
    state.share("conv-1", "personal");
    let mine = journal_of(&[("q1", "a1")]);
    run_pass(&mut state, &shelf, &mut alice, &mine, 1000.0);
    let reads = shelf.calls("stored");
    assert_eq!(reads, 1, "the first pass read the team copy once");

    // Nothing changed here: the pass is held without touching the server.
    let again = run_pass(&mut state, &shelf, &mut alice, &mine, 2000.0);
    assert_eq!(shelf.calls("stored"), reads, "a held conversation is not read again");
    assert_eq!(again.refused.len(), 1);

    // This machine's turns change: it is checked again.
    let changed = journal_of(&[("q1", "a1"), ("q2", "something else")]);
    run_pass(&mut state, &shelf, &mut alice, &changed, 3000.0);
    assert!(shelf.calls("stored") > reads, "a changed conversation is checked again");
    assert!(shelf.writes.borrow().is_empty());
}

#[test]
fn a_keep_mine_request_pushes_over_the_team_copy_the_user_reviewed() {
    let shelf = Shelf::default();
    let mut alice = Member::new(Device::generate());
    teammate_copy(&shelf, &mut alice, "conv-1", 1, &journal_of(&[("q1", "a1"), ("q2", "bob's answer")]));
    let mut state = State::default();
    state.share("conv-1", "personal");
    let mine = journal_of(&[("q1", "a1"), ("q2", "alice's answer")]);
    run_pass(&mut state, &shelf, &mut alice, &mine, 1000.0);
    assert!(state.team_conflict("personal", "conv-1").is_some(), "held: diverged");

    // The user reviewed the team's sequence 1 and this machine's turns, and chose "keep mine".
    let local = fingerprint(&turns_of(&mine).unwrap());
    state.request_team_force("personal", "conv-1", &local, 1);
    let forced = run_pass(&mut state, &shelf, &mut alice, &mine, 2000.0);
    assert_eq!(forced.pushed, vec!["conv-1".to_string()], "{forced:?}");
    assert_eq!(shelf.writes.borrow().clone(), vec![("conv-1".to_string(), 2)], "the next version, over the reviewed one");
    assert!(state.team_conflict("personal", "conv-1").is_none(), "the hold is resolved");
}

#[test]
fn a_keep_mine_request_is_inert_once_the_team_copy_has_moved_on() {
    let shelf = Shelf::default();
    let mut alice = Member::new(Device::generate());
    teammate_copy(&shelf, &mut alice, "conv-1", 1, &journal_of(&[("q1", "a1"), ("q2", "bob's answer")]));
    let mut state = State::default();
    state.share("conv-1", "personal");
    let mine = journal_of(&[("q1", "a1"), ("q2", "alice's answer")]);
    let local = fingerprint(&turns_of(&mine).unwrap());
    // The user reviewed sequence 1; before they chose, Bob pushed sequence 2.
    state.request_team_force("personal", "conv-1", &local, 1);
    let newer = journal_of(&[("q1", "a1"), ("q2", "bob's answer"), ("q3", "bob again")]);
    let mut bob = Member::new(Device::generate());
    shelf.records.borrow_mut().insert("conv-1".into(), bob.record("conv-1", 2, &newer));
    alice.known.insert(shelf.stored("conv-1"), newer);

    let p = run_pass(&mut state, &shelf, &mut alice, &mine, 2000.0);
    assert!(shelf.writes.borrow().is_empty(), "a request for sequence 1 does not overwrite sequence 2: {p:?}");
    assert_eq!(p.refused.len(), 1);
}

#[test]
fn keep_mine_requests_and_conflicts_survive_a_restart() {
    let d = sandbox("held-state");
    let path = d.join("state.json");
    let mut state = State::default();
    state.set_team_conflict("personal", "conv-1", Some(TeamConflict {
        local: "l".into(), at: 5.0, kind: ConflictKind::Diverged, count: 2, local_new: 1, team_new: 3, why: String::new(),
    }));
    state.request_team_force("personal", "conv-1", "l", 7);
    state.save(&path).unwrap();
    let back = State::load(&path).unwrap();
    let c = back.team_conflict("personal", "conv-1").expect("the conflict is kept");
    assert_eq!((c.kind, c.count, c.local_new, c.team_new), (ConflictKind::Diverged, 2, 1, 3));
    assert_eq!(back.team_force("personal", "conv-1").map(|f| f.team_seq), Some(7));
    let _ = std::fs::remove_dir_all(&d);
}
