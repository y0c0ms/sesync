//! Two TUIs open at once on one state file (two terminals), driven by their own keys and saved
//! and re-read the way the TUI does: neither overwrites the other's shares, and each shows the
//! other's once it has written or read the file.
use sesync::app::{App, Ev};
use sesync::manage::Roots;
use sesync::state::State;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sesync-two-tuis-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// An omp session the Sessions list shows.
fn omp_session(root: &Path, id: &str) -> String {
    let path = root.join("omp").join("sessions").join("-proj").join(format!("2026-10-01T10-00-00-000Z_{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!(
        "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"/home/u/proj\",\"timestamp\":\"2026-10-01T10:00:00.000Z\"}}\n\
         {{\"type\":\"message\",\"id\":\"m1\",\"message\":{{\"role\":\"user\",\"content\":\"{}\"}}}}\n", "hello ".repeat(100))).unwrap();
    path.display().to_string()
}

/// A TUI started on the state file as it is now, on a machine in one team.
fn open(root: &Path, file: &Path) -> App {
    let mut a = App::new(Roots::under(root, root.join("no-sesync-conv")), State::load(file).unwrap(),
        root.join("locks"), root.join("hs.lock"));
    a.set_teams(vec!["team-x".into()]);
    a
}

/// Move the cursor to the row of `key`.
fn select(a: &mut App, key: &str) {
    let at = a.rows().iter().position(|g| g.key == key).expect("the session is listed");
    a.handle(Ev::Home, Instant::now());
    for _ in 0..at {
        a.handle(Ev::Down, Instant::now());
    }
}

#[test]
fn two_open_tuis_keep_each_others_shares() {
    let root = sandbox("shares");
    let a_key = omp_session(&root, "aaaaaaaa-0000-4000-8000-000000000001");
    let b_key = omp_session(&root, "bbbbbbbb-0000-4000-8000-000000000002");
    let file = root.join("state.json");
    let mut one = open(&root, &file);
    let mut two = open(&root, &file);
    let conv = |key: &str| sesync::manage::session_id_of(key);

    select(&mut one, &a_key);
    one.handle(Ev::Char('m'), Instant::now());
    assert!(one.take_user_dirty());
    one.save_user(&file).unwrap();

    // The second TUI was opened before that share and shares another session.
    select(&mut two, &b_key);
    two.handle(Ev::Char('m'), Instant::now());
    assert!(two.take_user_dirty());
    two.save_user(&file).unwrap();

    let saved = State::load(&file).unwrap();
    assert!(saved.is_shared_with(&conv(&a_key), "team-x"), "the first TUI's share survives the second's save");
    assert!(saved.is_shared_with(&conv(&b_key), "team-x"));
    assert!(two.state().is_shared_with(&conv(&a_key), "team-x"), "the second TUI shows the first one's share");

    // The first TUI reads the file again, as it does whenever the file changes.
    one.absorb_daemon_state(&State::load(&file).unwrap());
    assert!(one.state().is_shared_with(&conv(&b_key), "team-x"), "the first TUI shows the second one's share");
    let _ = std::fs::remove_dir_all(&root);
}
