//! The TUI lists sessions without opening them: titles come from the cache a previous run saved,
//! else the name the daemon recorded, and a worker reads the rest in the background, rows on
//! screen first. Opening the TUI must never wait on reading every conversation.

use sesync::app::App;
use sesync::manage::{read_title, Roots, TitleCache};
use sesync::state::{Source, State};
use sesync::title_worker::Worker;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-titlework-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// omp sessions named by their header, plus the roots.
fn sessions(sb: &PathBuf, names: &[&str]) -> (Roots, Vec<String>) {
    let roots = Roots::under(sb, sb.join("no-sesync-conv"));
    let dir = roots.locations.sessions_dir("omp").join("-tmp");
    fs::create_dir_all(&dir).unwrap();
    let mut keys = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let id = format!("{i:08}-1111-2222-3333-444444444444");
        let path = dir.join(format!("2026-10-0{}T10-00-00-000Z_{id}.jsonl", i + 1));
        fs::write(&path, format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"cwd\":\"/tmp\",\"title\":\"{name}\"}}\n")).unwrap();
        // Newer files later, so the list (newest first) is the reverse of `names`.
        let t = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000 + i as u64 * 60);
        fs::File::options().write(true).open(&path).unwrap().set_modified(t).unwrap();
        keys.push(path.display().to_string());
    }
    (roots, keys)
}

fn deferred(sb: &PathBuf, roots: Roots, state: State, cache: TitleCache) -> App {
    App::new_deferred(roots, state, sb.join("locks"), sb.join("no-sesync_lock"), cache)
}

fn titles(a: &App) -> Vec<String> {
    a.visible_rows().iter().map(|g| g.title.clone()).collect()
}

#[test]
fn the_list_opens_no_conversation_and_names_the_titles_still_to_read() {
    let sb = sandbox("defer");
    let (roots, keys) = sessions(&sb, &["First", "Second"]);
    let mut a = deferred(&sb, roots, State::default(), TitleCache::default());
    assert_eq!(titles(&a), vec!["untitled", "untitled"], "no title is read while listing");

    let asked = a.take_title_requests();
    let order: Vec<&str> = asked.iter().map(|(k, _)| k.as_str()).collect();
    let shown: Vec<String> = a.visible_rows().iter().map(|g| g.key.clone()).collect();
    assert_eq!(order, shown, "rows are asked for in the order they are shown");
    assert!(a.take_title_requests().is_empty(), "a title being read is not asked for twice");

    a.apply_titles(keys.iter().map(|k| read_title(k, "omp").unwrap()).collect());
    assert_eq!(titles(&a), vec!["Second", "First"]);
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_daemon_recorded_name_stands_in_until_the_title_is_read() {
    let sb = sandbox("daemon-name");
    let (roots, keys) = sessions(&sb, &["From the file"]);
    let mut state = State::default();
    let mut src = Source::new("omp", &keys[0]);
    src.title = "From the daemon".into();
    state.sources.insert(keys[0].clone(), src);
    let mut a = deferred(&sb, roots, state, TitleCache::default());
    assert_eq!(titles(&a), vec!["From the daemon"]);
    assert_eq!(a.take_title_requests().len(), 1, "the file is still read: it may have been renamed since");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn a_saved_cache_names_unchanged_files_and_only_changed_ones_are_read() {
    let sb = sandbox("saved");
    let (roots, keys) = sessions(&sb, &["Kept", "Grows"]);
    let saved = sb.join("state").join("titles.json");
    let mut cache = TitleCache::default();
    for k in &keys {
        cache.put(read_title(k, "omp").unwrap());
    }
    cache.save(&saved).unwrap();
    assert!(!cache.is_dirty());

    // The next run: nothing changed, so nothing is read.
    let mut a = deferred(&sb, Roots::under(&sb, sb.join("no-sesync-conv")), State::default(), TitleCache::load(&saved));
    assert_eq!(titles(&a), vec!["Grows", "Kept"]);
    assert!(a.take_title_requests().is_empty(), "unchanged files are named from the saved cache");

    // A session that changed since is read again; until then it keeps its last title.
    fs::OpenOptions::new().append(true).open(&keys[1]).unwrap()
        .write_all_at_end("{\"type\":\"title_change\",\"title\":\"Renamed\"}\n");
    let mut b = deferred(&sb, roots, State::default(), TitleCache::load(&saved));
    assert_eq!(titles(&b), vec!["Grows", "Kept"]);
    let asked = b.take_title_requests();
    assert_eq!(asked, vec![(keys[1].clone(), "omp".to_string())]);
    b.apply_titles(vec![read_title(&keys[1], "omp").unwrap()]);
    assert_eq!(titles(&b), vec!["Renamed", "Kept"]);
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn an_unreadable_saved_cache_is_an_empty_one() {
    let sb = sandbox("corrupt");
    let saved = sb.join("titles.json");
    fs::write(&saved, "{not json").unwrap();
    let (roots, _) = sessions(&sb, &["Only"]);
    let mut a = deferred(&sb, roots, State::default(), TitleCache::load(&saved));
    assert_eq!(a.take_title_requests().len(), 1, "every title is read again; nothing fails");
    let _ = fs::remove_dir_all(&sb);
}

#[test]
fn the_worker_reads_what_it_is_asked_for() {
    let sb = sandbox("worker");
    let (_, keys) = sessions(&sb, &["Alpha", "Beta"]);
    let w = Worker::start();
    w.request(keys.iter().map(|k| (k.clone(), "omp".to_string())).collect());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    while got.len() < 2 && Instant::now() < deadline {
        got.extend(w.take());
        std::thread::sleep(Duration::from_millis(10));
    }
    let names: Vec<&str> = got.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(names, vec!["Alpha", "Beta"], "answered in the order asked");
    assert!(w.idle());
    let _ = fs::remove_dir_all(&sb);
}

/// Append text to a file in place (the harness writing a new record).
trait AppendText {
    fn write_all_at_end(self, text: &str);
}

impl AppendText for fs::File {
    fn write_all_at_end(mut self, text: &str) {
        use std::io::Write;
        self.write_all(text.as_bytes()).unwrap();
    }
}
