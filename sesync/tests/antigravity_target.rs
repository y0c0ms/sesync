//! The Antigravity CLI (`agy`) mirror target and source, end to end through the real daemon and
//! the real converter (`../sesync-conv/target/release/sesync-conv`, build it first). A chosen
//! session is written as `<ANTIGRAVITY_CLI_HOME>/conversations/<id>.db` and recorded in
//! `mirror_keys`, because the same store is scanned for sources. agy appends a live turn to the
//! database's WAL only, so a prompt typed there must be seen from the WAL alone. No real harness
//! runs; no real store is touched (every store points into a temporary sandbox).

use rusqlite::Connection;
use sesync::state::{Source, State};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sesync");
const OMP_ID: &str = "aaaaaaaa-1111-2222-3333-444444444444";

fn sesync_conv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/target/release/sesync-conv")
}

/// A native agy 1.4.4 conversation: prompt "TOOLREAD please read hello", a read_file call, its
/// result and a reply.
fn native_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sesync-conv/tests/fixtures/agy-1.4.4-read-file.db")
}

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-agy-target-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn store(sb: &Path) -> PathBuf {
    sb.join("agy")
}

/// Pre-seed the user's choice exactly as the TUI would save it, before any pass runs.
fn choose(sb: &Path, fmt: &str, key: &str, targets: &[&str]) {
    fs::create_dir_all(sb.join("state")).unwrap();
    let mut state = State::default();
    let mut src = Source::new(fmt, key);
    for t in targets {
        src.chosen_targets.insert((*t).to_string());
    }
    state.sources.insert(key.to_string(), src);
    state.save(&sb.join("state").join("state.json")).unwrap();
}

fn run(sb: &Path) -> Output {
    Command::new(BIN)
        .arg("--once")
        .env("SESYNC_DIR", sb.join("state"))
        .env("OMP_AGENT_DIR", sb.join("omp"))
        .env("PI_AGENT_DIR", sb.join("pi"))
        .env("CLAUDE_CONFIG_DIR", sb.join("claude"))
        .env("ANTIGRAVITY_CLI_HOME", store(sb))
        .env("OPENCODE_DB", sb.join("no-opencode").join("opencode.db"))
        .env("OPENCODE_BIN", sb.join("no-opencode-bin"))
        .env("SESYNC_CONV", sesync_conv())
        .env("SESYNC_LOCK", sb.join("no-sesync_lock"))
        .env("SESYNC_QUIET", "0")
        .env("SESYNC_MAX_AGE_DAYS", "3650")
        .output()
        .expect("run sesync")
}

fn out(o: &Output) -> String {
    format!("stdout: {}\nstderr: {}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// A native omp journal with a title and a cwd (agy lists conversations by workspace), padded
/// past the 512-byte minimum a session needs.
fn omp_session(sb: &Path) -> PathBuf {
    let dir = sb.join("omp").join("sessions").join("-tmp-proj");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("2026-10-01T10-00-00-000Z_{OMP_ID}.jsonl"));
    let header = format!(r#"{{"type":"session","version":3,"id":"{OMP_ID}","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/tmp/proj","title":"Agy Target Title"}}"#);
    let msg = format!(r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{{"role":"user","content":"{}"}}}}"#, "hello antigravity target ".repeat(30));
    let reply = r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"first reply"}]}}"#;
    fs::write(&path, format!("{header}\n{msg}\n{reply}\n")).unwrap();
    path
}

fn agy_dbs(sb: &Path) -> Vec<PathBuf> {
    sesync::harness::antigravity::store::antigravity_sessions(&store(sb))
}

/// Every journal in the Pi store, as the daemon writes them.
fn pi_files(sb: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(dirs) = fs::read_dir(sb.join("pi").join("sessions")) else { return out };
    for d in dirs.flatten() {
        let Ok(files) = fs::read_dir(d.path()) else { continue };
        out.extend(files.flatten().map(|f| f.path()).filter(|p| p.extension().map_or(false, |x| x == "jsonl")));
    }
    out.sort();
    out
}

/// The conversation as the converter reads it back, as journal text.
fn read_back(db: &Path, scratch: &Path) -> String {
    let journal = scratch.join("read-back.jsonl");
    let o = Command::new(sesync_conv()).arg("from-antigravity").arg(db).arg("--out").arg(&journal).output().unwrap();
    assert!(o.status.success(), "{}", out(&o));
    fs::read_to_string(&journal).unwrap()
}

fn sha(p: &Path) -> String {
    sesync::mirror::file_hash(p).unwrap()
}

fn with_suffix(p: &Path, s: &str) -> PathBuf {
    let mut o = p.as_os_str().to_owned();
    o.push(s);
    PathBuf::from(o)
}

// --------------------------------------------------------------- protobuf (writer, test only)

fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn int_field(f: u64, v: u64, out: &mut Vec<u8>) {
    varint(f << 3, out);
    varint(v, out);
}

fn bytes_field(f: u64, b: &[u8], out: &mut Vec<u8>) {
    varint((f << 3) | 2, out);
    varint(b.len() as u64, out);
    out.extend_from_slice(b);
}

/// StepMetadata {1: Timestamp{1 seconds}, 3: source USER_EXPLICIT}.
fn step_metadata(secs: u64) -> Vec<u8> {
    let mut ts = Vec::new();
    int_field(1, secs, &mut ts);
    let mut md = Vec::new();
    bytes_field(1, &ts, &mut md);
    int_field(3, 4, &mut md);
    md
}

/// A USER_INPUT step exactly as agy stores one: payload {1: 14, 4: DONE, 5: metadata, 19: {2: text}}.
fn append_user_step(c: &Connection, secs: u64, text: &str) {
    let md = step_metadata(secs);
    let mut input = Vec::new();
    bytes_field(2, text.as_bytes(), &mut input);
    let mut payload = Vec::new();
    int_field(1, 14, &mut payload);
    int_field(4, 3, &mut payload);
    bytes_field(5, &md, &mut payload);
    bytes_field(19, &input, &mut payload);
    let idx: i64 = c.query_row("SELECT coalesce(max(idx), -1) + 1 FROM steps", [], |r| r.get(0)).unwrap();
    c.execute(
        "INSERT INTO steps (idx, step_type, status, metadata, step_payload) VALUES (?1, 14, 3, ?2, ?3)",
        rusqlite::params![idx, md, payload],
    ).unwrap();
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

// ------------------------------------------------------------------------------- tests

#[test]
fn an_omp_session_chosen_for_antigravity_is_installed_titled_and_refreshed_in_place() {
    let sb = sandbox("install");
    let path = omp_session(&sb);
    let key = path.display().to_string();
    choose(&sb, "omp", &key, &["antigravity"]);

    let o = run(&sb);
    assert!(o.status.success(), "{}", out(&o));
    let dbs = agy_dbs(&sb);
    assert_eq!(dbs.len(), 1, "one chosen antigravity mirror must be written: {}", out(&o));
    let db = dbs[0].clone();
    let expected_id = sesync::mirror::mirror_uuid(OMP_ID, "antigravity");
    assert_eq!(db.file_stem().unwrap().to_string_lossy(), expected_id, "the conversation id is minted from the stable id");

    // A real agy conversation: agy 1.4.4 needs user_version 1 and the (4, 17) trajectory.
    let c = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let uv: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(uv, 1);
    let (ty, src): (i64, i64) = c.query_row("SELECT trajectory_type, source FROM trajectory_meta", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((ty, src), (4, 17));
    drop(c);
    assert_eq!(sesync::harness::antigravity::store::title(&db), "Agy Target Title", "the source's title is written where agy reads it");
    let at = newest_prompt(&db).expect("the mirrored prompt is a prompt");
    assert_eq!(at, sesync::prompts::parse_rfc3339("2026-10-01T10:00:01.000Z").unwrap(),
        "the copy carries the source's own prompt time, so it never reads as an answer");
    assert!(read_back(&db, &sb).contains("hello antigravity target"), "the conversation is carried");

    let state = State::load(&sb.join("state").join("state.json")).unwrap();
    let m = &state.sources[&key].mirrors["antigravity"];
    assert_eq!(m.path, db.display().to_string(), "the mirror records the installed database");
    assert!(state.mirror_keys.contains(&m.path), "an antigravity mirror is a file inside the scanned store");
    assert_eq!(m.installed_hash, sha(&db), "the record holds what sesync wrote");

    // Nothing changed: no second write, and the copy never becomes a source.
    let o = run(&sb);
    assert!(String::from_utf8_lossy(&o.stdout).contains("0 source(s) changed"), "{}", out(&o));
    let state = State::load(&sb.join("state").join("state.json")).unwrap();
    assert_eq!(state.sources.len(), 1, "the mirror never becomes a source");

    // The conversation changed: the same database is replaced in place.
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str(r#"{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-10-01T10:00:03.000Z","message":{"role":"assistant","content":[{"type":"text","text":"a later turn"}]}}"#);
    text.push('\n');
    fs::write(&path, text).unwrap();
    let o = run(&sb);
    assert!(o.status.success(), "{}", out(&o));
    assert_eq!(agy_dbs(&sb), vec![db.clone()], "one conversation keeps one database across refreshes");
    assert!(read_back(&db, &sb).contains("a later turn"), "the refreshed database carries the new turn");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_prompt_agy_wrote_only_to_the_wal_moves_the_source_to_the_antigravity_copy() {
    let sb = sandbox("wal");
    let path = omp_session(&sb);
    let key = path.display().to_string();
    choose(&sb, "omp", &key, &["antigravity"]);
    let o = run(&sb);
    assert!(o.status.success(), "{}", out(&o));
    let db = agy_dbs(&sb).pop().expect("the mirror is written");

    // agy switches the database to WAL on its first open (that rewrites the header) and keeps
    // the bookkeeping it adds: changed bytes, no prompt, so no turn.
    {
        let c = Connection::open(&db).unwrap();
        c.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0)).unwrap();
    }
    let o = run(&sb);
    assert!(o.status.success(), "{}", out(&o));
    let state = State::load(&sb.join("state").join("state.json")).unwrap();
    assert_eq!(state.sources[&key].fmt, "omp", "changed bytes without a prompt never move the source: {}", out(&o));

    // agy is running and the user types: the step lands in the WAL, the database is untouched.
    let before_db = sha(&db);
    let c = Connection::open(&db).unwrap();
    c.query_row("PRAGMA wal_autocheckpoint=0", [], |r| r.get::<_, i64>(0)).unwrap();
    append_user_step(&c, now_secs(), "WAL_ONLY question typed in agy");
    assert!(with_suffix(&db, "-wal").is_file(), "the turn sits in the WAL");
    assert_eq!(sha(&db), before_db, "the database file itself did not change");

    let o = run(&sb);
    assert!(o.status.success(), "{}", out(&o));
    let state = State::load(&sb.join("state").join("state.json")).unwrap();
    let new_key = db.display().to_string();
    assert!(state.sources.get(&new_key).is_some_and(|s| s.fmt == "antigravity"),
        "the answered antigravity copy is the source now: {}", out(&o));
    assert!(state.sources.get(&key).is_none(), "the old omp entry is a copy now");
    // The omp copy keeps its session id; the rewrite may move it into the bucket of the source's
    // cwd (this sandbox placed the original elsewhere), so the record names where it is now.
    let omp_copy = PathBuf::from(&state.sources[&new_key].mirrors["omp"].path);
    assert!(omp_copy.file_name().unwrap().to_string_lossy().ends_with(&format!("_{OMP_ID}.jsonl")),
        "the omp file is the copy in its own harness, under its own id: {}", omp_copy.display());
    assert!(fs::read_to_string(&omp_copy).unwrap().contains("WAL_ONLY question typed in agy"),
        "the omp copy is rewritten from the WAL's content while agy still holds the database");
    let archived = fs::read_dir(sb.join("state").join("archive")).map(|d| d.count()).unwrap_or(0);
    assert!(archived > 0, "the user's original omp bytes were kept before being replaced");
    drop(c);
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn an_antigravity_source_is_converted_to_a_journal_for_the_pi_mirror_without_touching_it() {
    let sb = sandbox("source");
    let conv = store(&sb).join("conversations");
    fs::create_dir_all(&conv).unwrap();
    fs::create_dir_all(sb.join("pi").join("sessions")).unwrap();
    let db = conv.join("28b4f6a2-2ecf-4ad2-933f-25dfd05896bf.db");
    fs::copy(native_fixture(), &db).unwrap();
    let before = sha(&db);
    let key = db.display().to_string();
    choose(&sb, "antigravity", &key, &["pi"]);

    let o = run(&sb);
    assert!(o.status.success(), "{}", out(&o));
    let state = State::load(&sb.join("state").join("state.json")).unwrap();
    assert_eq!(state.sources[&key].fmt, "antigravity", "the antigravity store is scanned for sources");
    assert_eq!(state.sources[&key].conv, "28b4f6a2-2ecf-4ad2-933f-25dfd05896bf", "the conversation id is the file stem");
    let mirrors = pi_files(&sb);
    assert_eq!(mirrors.len(), 1, "the chosen pi mirror must be written: {}", out(&o));
    let body = fs::read_to_string(&mirrors[0]).unwrap();
    assert!(body.contains("TOOLREAD please read hello"), "the conversation is carried:\n{body}");
    assert!(body.contains("\"type\":\"message\""), "the mirror is a journal:\n{body}");
    assert_eq!(sha(&db), before, "reading a source never changes agy's file");
    assert!(!with_suffix(&db, "-wal").exists() && !with_suffix(&db, "-shm").exists(),
        "no -wal/-shm is left next to agy's file: the next pass would read it as a change");

    let o = run(&sb);
    assert!(String::from_utf8_lossy(&o.stdout).contains("0 source(s) changed"), "{}", out(&o));
    assert_eq!(pi_files(&sb).len(), 1);
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_newest_prompt_of_a_native_agy_conversation_is_its_user_input_step() {
    let sb = sandbox("prompt");
    let db = sb.join("conversations").join("c.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    fs::copy(native_fixture(), &db).unwrap();
    let before = sha(&db);
    // StepMetadata 1 of step 0, the only USER_INPUT: 1792591870 s + 608266600 ns.
    let at = newest_prompt(&db).expect("a prompt");
    assert!((at - 1_792_591_870.6082666).abs() < 1e-6, "{at}");
    assert_eq!(sha(&db), before, "reading never changes the file");
    assert!(!with_suffix(&db, "-wal").exists() && !with_suffix(&db, "-shm").exists(), "no -wal/-shm created");

    // sesync's own compaction note is stamped later but is not a prompt.
    {
        let c = Connection::open(&db).unwrap();
        append_user_step(&c, 1_792_600_000, "Earlier turns of this conversation were compacted. Their summary:\n\nx");
    }
    let at = newest_prompt(&db).unwrap();
    assert!((at - 1_792_591_870.6082666).abs() < 1e-6, "the note is skipped: {at}");
    {
        let c = Connection::open(&db).unwrap();
        append_user_step(&c, 1_792_600_100, "a real follow-up");
    }
    assert_eq!(newest_prompt(&db), Some(1_792_600_100.0));
    assert_eq!(newest_prompt(&sb.join("missing.db")), None);
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_wal_only_change_moves_the_copy_stamp_and_hash() {
    let sb = sandbox("stamp");
    let db = sb.join("conversations").join("c.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    fs::copy(native_fixture(), &db).unwrap();
    {
        let c = Connection::open(&db).unwrap();
        c.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0)).unwrap();
    }
    let (stamp, hash) = (sesync::mirror::copy_stamp("antigravity", &db), sesync::mirror::copy_hash("antigravity", &db).unwrap());
    assert_eq!(hash, sha(&db), "without a WAL the copy hash is the file hash");
    let c = Connection::open(&db).unwrap();
    c.query_row("PRAGMA wal_autocheckpoint=0", [], |r| r.get::<_, i64>(0)).unwrap();
    append_user_step(&c, now_secs(), "typed");
    assert_ne!(sesync::mirror::copy_stamp("antigravity", &db), stamp, "the stamp covers the WAL");
    assert_ne!(sesync::mirror::copy_hash("antigravity", &db).unwrap(), hash, "the hash covers the WAL");
    assert_eq!(sesync::mirror::copy_hash("omp", &db).unwrap(), hash, "other formats are their one file");
    drop(c);
    let _ = fs::remove_dir_all(&sb);
}

/// The newest user prompt of an agy conversation, read the way the daemon reads it.
fn newest_prompt(db: &Path) -> Option<f64> {
    let h = sesync::harness::get("antigravity").expect("antigravity is a harness");
    h.newest_prompt(&sesync::harness::Location::default(), &db.display().to_string())
}

fn roots(sb: &Path, sesync_conv: PathBuf) -> sesync::manage::Roots {
    let mut roots = sesync::manage::Roots::under(sb, sesync_conv);
    roots.locations.set("antigravity", sesync::harness::Location { root: store(sb), cli: None });
    roots
}

/// An agy conversation in the sandbox store, as the TUI lists it.
fn listed_native(sb: &Path) -> (PathBuf, sesync::manage::Group) {
    let conv = store(sb).join("conversations");
    fs::create_dir_all(&conv).unwrap();
    let db = conv.join("28b4f6a2-2ecf-4ad2-933f-25dfd05896bf.db");
    fs::copy(native_fixture(), &db).unwrap();
    let groups = sesync::manage::list_groups(&roots(sb, sesync_conv()), &State::default());
    let g = groups.into_iter().find(|g| g.key == db.display().to_string()).expect("the agy conversation is listed");
    assert_eq!(g.fmt, "antigravity");
    (db, g)
}

#[test]
fn the_tui_renames_an_antigravity_conversation_through_the_converter_and_shows_it() {
    let sb = sandbox("rename");
    let (db, g) = listed_native(&sb);
    let r = roots(&sb, sesync_conv());
    let mut cache = sesync::manage::TitleCache::default();
    let before = sesync::manage::list_groups_cached(&r, &State::default(), &mut cache);
    assert_eq!(before.iter().find(|x| x.key == g.key).unwrap().title, "untitled", "no annotation, no cached summary yet");

    let (saved, note) = sesync::manage::rename_group_noted(&r, &g, "Renamed \"in\" sesync", &sb.join("locks"), &sb.join("no-sesync_lock")).unwrap();
    assert_eq!(saved, "Renamed \"in\" sesync");
    assert_eq!(note, None, "no agy runs, so the name applies at once");
    assert_eq!(sesync::harness::antigravity::store::title(&db), "Renamed \"in\" sesync", "written to annotations, escaped and read back");
    let after = sesync::manage::list_groups_cached(&r, &State::default(), &mut cache);
    assert_eq!(after.iter().find(|x| x.key == g.key).unwrap().title, "Renamed \"in\" sesync",
        "the title cache sees the annotations file change, not only the database");
    let _ = fs::remove_dir_all(&sb);
}

/// A stand-in converter that answers like `antigravity-retitle` with a running agy (exit 2).
fn busy_converter(sb: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let p = sb.join("busy-conv.cmd");
        fs::write(&p, "@echo sesync_conv: title written to annotations 1>&2\r\n@exit /b 2\r\n").unwrap();
        p
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = sb.join("busy-conv.sh");
        fs::write(&p, "#!/bin/sh\necho 'sesync_conv: title written to annotations' >&2\nexit 2\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
}

#[test]
fn a_rename_while_agy_holds_its_cache_is_saved_with_a_restart_note() {
    let sb = sandbox("rename-busy");
    let (_db, g) = listed_native(&sb);
    let r = roots(&sb, busy_converter(&sb));
    let (saved, note) = sesync::manage::rename_group_noted(&r, &g, "Later name", &sb.join("locks"), &sb.join("no-sesync_lock")).unwrap();
    assert_eq!(saved, "Later name");
    let note = note.expect("the user is told the name shows later");
    assert!(note.contains("restarts"), "{note}");

    // Any other converter failure is a refusal, never a silent success.
    let r = roots(&sb, sb.join("no-such-converter"));
    assert!(sesync::manage::rename_group_noted(&r, &g, "Other", &sb.join("locks"), &sb.join("no-sesync_lock")).is_err());
    let _ = fs::remove_dir_all(&sb);
}
