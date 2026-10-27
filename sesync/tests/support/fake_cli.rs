//! Fake CLIs for tests that run a stand-in for a real tool.
//!
//! Writing an executable and then running it races every other test thread that forks: a child
//! forked while our write handle is open keeps that handle until its own exec, and our exec then
//! fails with ETXTBSY ("Text file busy"). The fork can come from code under test, so no lock in
//! the test suite can cover it. `stage` therefore never writes anything that gets exec'd: it
//! links the fake's path to one committed executable, `tests/fixtures/fake-cli.sh`, and writes
//! the fake's script to `<path>.body`, which that executable runs with `sh`. Creating a symlink
//! and writing a file that is not executed cannot produce ETXTBSY, and no sleep or retry is needed.

use std::path::{Path, PathBuf};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-cli.sh");

/// Make `path` an executable whose behaviour is the `sh` script `body`.
pub fn stage(path: &Path, body: &str) {
    let _ = std::fs::remove_file(path);
    std::os::unix::fs::symlink(FIXTURE, path).unwrap();
    let mut script = path.as_os_str().to_owned();
    script.push(".body");
    std::fs::write(PathBuf::from(script), body).unwrap();
}
