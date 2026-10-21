//! One forwarding store for every test that needs a store to misbehave on purpose. It wraps
//! `MemoryStore`, forwards every call, and adds fault switches. When the `Store` trait grows a
//! method, only this file needs the new forwarding line; the test files do not.
#![allow(dead_code)]

use sesync_team::backup::BackupPolicy;
use sesync_team::service::{JoinRequest, MemoryStore, Store};
use sesync_team::spaces::Space;
use sesync_team::totp::JoinGate;
use std::cell::Cell;
use std::rc::Rc;

/// The fault switches. Cheap to clone; the clones share the same switches.
#[derive(Clone, Default)]
pub struct Faults {
    /// The database is unreachable: every read fails, every write is dropped, `ping` says false.
    pub database_down: Rc<Cell<bool>>,
}

impl Faults {
    pub fn down(&self) -> bool {
        self.database_down.get()
    }
}

pub struct TestStore {
    inner: MemoryStore,
    faults: Faults,
}

impl TestStore {
    pub fn new() -> Self {
        Self { inner: MemoryStore::default(), faults: Faults::default() }
    }

    pub fn with_faults(faults: Faults) -> Self {
        Self { inner: MemoryStore::default(), faults }
    }

    /// A store whose database is unreachable from the start.
    pub fn database_down() -> Self {
        let store = Self::new();
        store.faults.database_down.set(true);
        store
    }

    fn down(&self) -> bool {
        self.faults.down()
    }
}

impl Default for TestStore {
    fn default() -> Self {
        Self::new()
    }
}

impl Store for TestStore {
    fn load_space(&self, name: &str) -> Option<Space> {
        if self.down() { None } else { self.inner.load_space(name) }
    }
    fn save_space(&mut self, space: Space) {
        if !self.down() { self.inner.save_space(space) }
    }
    fn record(&self, space: &str, conv: &str) -> Option<Vec<u8>> {
        if self.down() { None } else { self.inner.record(space, conv) }
    }
    fn put_record(&mut self, space: &str, conv: &str, sealed: Vec<u8>) {
        if !self.down() { self.inner.put_record(space, conv, sealed) }
    }
    fn list_conversations(&self, space: &str) -> Vec<String> {
        if self.down() { vec![] } else { self.inner.list_conversations(space) }
    }
    fn ping(&self) -> bool {
        !self.down()
    }
    fn save_trust_list(&mut self, space: &str, bytes: Vec<u8>) {
        if !self.down() { self.inner.save_trust_list(space, bytes) }
    }
    fn load_trust_list(&self, space: &str) -> Option<Vec<u8>> {
        if self.down() { None } else { self.inner.load_trust_list(space) }
    }
    fn save_admin_key(&mut self, space: &str, key: Vec<u8>) {
        if !self.down() { self.inner.save_admin_key(space, key) }
    }
    fn load_admin_key(&self, space: &str) -> Option<Vec<u8>> {
        if self.down() { None } else { self.inner.load_admin_key(space) }
    }
    fn load_gate(&self, space: &str, machine: &str) -> Option<JoinGate> {
        if self.down() { None } else { self.inner.load_gate(space, machine) }
    }
    fn save_gate(&mut self, space: &str, machine: &str, gate: JoinGate) {
        if !self.down() { self.inner.save_gate(space, machine, gate) }
    }
    fn load_backup(&self, space: &str) -> Option<BackupPolicy> {
        if self.down() { None } else { self.inner.load_backup(space) }
    }
    fn save_backup(&mut self, space: &str, policy: BackupPolicy) {
        if !self.down() { self.inner.save_backup(space, policy) }
    }
    fn load_token_key(&self, space: &str) -> Option<[u8; 32]> {
        if self.down() { None } else { self.inner.load_token_key(space) }
    }
    fn load_owner(&self, space: &str, machine: &str) -> Option<String> {
        if self.down() { None } else { self.inner.load_owner(space, machine) }
    }
    fn save_owner(&mut self, space: &str, machine: &str, member: &str) {
        if !self.down() { self.inner.save_owner(space, machine, member) }
    }
    fn load_enrolled_at(&self, space: &str, machine: &str) -> Option<u64> {
        if self.down() { None } else { self.inner.load_enrolled_at(space, machine) }
    }
    fn save_enrolled_at(&mut self, space: &str, machine: &str, at: u64) {
        if !self.down() { self.inner.save_enrolled_at(space, machine, at) }
    }
    fn load_device(&self, space: &str, machine: &str) -> Option<Vec<u8>> {
        if self.down() { None } else { self.inner.load_device(space, machine) }
    }
    fn save_device(&mut self, space: &str, machine: &str, entry: Vec<u8>) {
        if !self.down() { self.inner.save_device(space, machine, entry) }
    }
    fn devices(&self, space: &str) -> Vec<(String, Vec<u8>)> {
        if self.down() { Vec::new() } else { self.inner.devices(space) }
    }
    fn remove_device(&mut self, space: &str, machine: &str) {
        if !self.down() { self.inner.remove_device(space, machine) }
    }
    fn requests(&self, space: &str) -> Vec<JoinRequest> {
        if self.down() { Vec::new() } else { self.inner.requests(space) }
    }
    fn save_request(&mut self, space: &str, request: &JoinRequest) {
        if !self.down() { self.inner.save_request(space, request) }
    }
    fn remove_request(&mut self, space: &str, machine: &str) {
        if !self.down() { self.inner.remove_request(space, machine) }
    }
    fn is_tombstoned(&self, space: &str, entry: &[u8]) -> bool {
        !self.down() && self.inner.is_tombstoned(space, entry)
    }
    fn save_tombstone(&mut self, space: &str, entry: &[u8]) {
        if !self.down() { self.inner.save_tombstone(space, entry) }
    }
    fn remove_tombstone(&mut self, space: &str, entry: &[u8]) {
        if !self.down() { self.inner.remove_tombstone(space, entry) }
    }
}

/// Shared guard for tests that need a real database and the real server binary. Returns the
/// database URL and the binary path. Without them the test skips politely, unless the run is CI
/// (`TEAM_REQUIRE_PG`), where a skip must fail instead of quietly shrinking the suite.
pub fn live_env() -> Option<(String, String)> {
    match (std::env::var("TEAM_TEST_PG"), std::env::var("TEAM_SERVER_BIN")) {
        (Ok(pg), Ok(bin)) => Some((pg, bin)),
        _ => {
            if std::env::var("TEAM_REQUIRE_PG").is_ok() {
                panic!("TEAM_REQUIRE_PG is set but this test needs TEAM_TEST_PG and TEAM_SERVER_BIN");
            }
            eprintln!("skipped: set TEAM_TEST_PG and TEAM_SERVER_BIN");
            None
        }
    }
}
