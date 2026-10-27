//! Test doubles the bridge's unit tests share: a fixed conversation list, a herdr that answers nothing,
//! and a request context over a directory. Compiled only for tests.

use super::catalog::{Catalog, Conversation};
use super::herdr::Herdr;
use super::rpc::Ctx;
use super::{notify, start, uploads, usage};
use super::turns::{LockTools, Turns};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// A catalog that lists the conversations it was given.
pub struct Fixed(pub Vec<Conversation>);

impl Catalog for Fixed {
    fn conversations(&self) -> Result<Vec<Conversation>, String> {
        Ok(self.0.clone())
    }
}

/// A herdr that refuses every call, as when no herdr is running.
pub struct NoHerdr;

impl Herdr for NoHerdr {
    fn call(&self, method: &str, _params: Value) -> Result<Value, String> {
        Err(format!("no herdr for {method}"))
    }
}

/// One omp conversation whose project directory is `dir`.
pub fn conversation(title: &str, dir: &str) -> Conversation {
    Conversation {
        harness: "omp".into(),
        key: format!("/s/{title}.jsonl"),
        title: title.into(),
        dir: dir.into(),
        updated: 1_700_000_000.0,
        idents: vec![format!("ident-{title}")],
    }
}

/// A request context that lists `conversations`, has no herdr, and keeps its state under `state_dir`.
pub fn ctx_with(conversations: Vec<Conversation>, state_dir: &Path) -> Ctx {
    Ctx {
        catalog: Arc::new(Fixed(conversations)),
        herdr: Arc::new(NoHerdr),
        host: super::rpc::Host { machine_name: "box".into(), platform: "linux" },
        locks: LockTools { sesync_lock: "/nonexistent/sesync-lock".into(), lock_dir: "/nonexistent".into() },
        turns: Turns::default(),
        dir: state_dir.to_path_buf(),
        notify: notify::Hub::new(None),
        usage: usage::Store::default(),
        uploads: uploads::Store::default(),
        launch: start::Launch::default(),
    }
}

/// A fresh, empty directory under the system temp dir, unique to `name` and this process.
pub fn sandbox(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sesync-bridge-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
