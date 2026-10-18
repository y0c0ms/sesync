//! Postgres implementation of the service `Store`. It persists what the service decides and
//! holds no rules of its own. Spaces are stored as wrapped bytes (see `wrap`), records as
//! sealed bytes; gates and backup policies as their serialized form. Nothing is readable
//! from the database without the master key.

use crate::backup::BackupPolicy;
use crate::service::Store;
use crate::spaces::Space;
use crate::totp::JoinGate;
use postgres::{Client, NoTls};
use std::cell::RefCell;

pub struct PgStore {
    client: RefCell<Client>,
    url: String,
    master: [u8; 32],
}

impl PgStore {
    pub fn connect(url: &str, master: [u8; 32]) -> Result<PgStore, String> {
        let client = Client::connect(url, NoTls).map_err(|e| e.to_string())?;
        Ok(PgStore { client: RefCell::new(client), url: url.to_string(), master })
    }

    /// Connect, retrying with a growing wait. A pod that connects in its first seconds can
    /// be refused while the network policy is still being programmed (kube-router race), so a
    /// single attempt at startup is not enough. Gives up after `attempts` tries.
    pub fn connect_with_retry(url: &str, master: [u8; 32], attempts: u32, wait: std::time::Duration)
        -> Result<PgStore, String>
    {
        let mut last = String::from("no attempt made");
        for n in 0..attempts.max(1) {
            match PgStore::connect(url, master) {
                Ok(store) => return Ok(store),
                Err(e) => {
                    last = e;
                    if n + 1 < attempts {
                        std::thread::sleep(wait * (n + 1));
                    }
                }
            }
        }
        Err(last)
    }

    /// Make sure the connection is alive, reconnecting if it was dropped (for example when the
    /// database restarted). A failed reconnect leaves the old client in place, so the next call
    /// tries again rather than panicking.
    fn ensure_connected(&self) {
        let alive = self.client.borrow_mut().simple_query("SELECT 1").is_ok();
        if !alive {
            if let Ok(fresh) = Client::connect(&self.url, NoTls) {
                *self.client.borrow_mut() = fresh;
            }
        }
    }

    /// Create any missing tables, never drop or change existing ones. Safe on a live database,
    /// and safe to run on every start. (`reset` is for tests only: it drops everything.)
    pub fn migrate(&self) -> Result<(), String> {
        self.client.borrow_mut().batch_execute(
            "CREATE TABLE IF NOT EXISTS spaces (name TEXT PRIMARY KEY, wrapped BYTEA NOT NULL);
             CREATE TABLE IF NOT EXISTS records (space TEXT NOT NULL, conversation TEXT NOT NULL,
                 sealed BYTEA NOT NULL, PRIMARY KEY (space, conversation));
             CREATE TABLE IF NOT EXISTS gates (space TEXT NOT NULL, machine TEXT NOT NULL, state BYTEA NOT NULL,
                 PRIMARY KEY (space, machine));
             CREATE TABLE IF NOT EXISTS backups (space TEXT PRIMARY KEY, policy TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS owners (space TEXT NOT NULL, machine TEXT NOT NULL, member TEXT NOT NULL,
                 PRIMARY KEY (space, machine));
             CREATE TABLE IF NOT EXISTS enrolled (space TEXT NOT NULL, machine TEXT NOT NULL, at BIGINT NOT NULL,
                 PRIMARY KEY (space, machine));
             CREATE TABLE IF NOT EXISTS trust (space TEXT PRIMARY KEY, signed_list BYTEA NOT NULL);
             CREATE TABLE IF NOT EXISTS admin_keys (space TEXT PRIMARY KEY, admin BYTEA NOT NULL);
             CREATE TABLE IF NOT EXISTS devices (space TEXT NOT NULL, machine TEXT NOT NULL, entry BYTEA NOT NULL,
                 PRIMARY KEY (space, machine));",
        ).map_err(|e| match e.as_db_error() {
            Some(db) => format!("{}: {}", db.code().code(), db.message()),
            None => e.to_string(),
        })
    }

    /// Create the tables. For tests and first start; migrations come later.
    pub fn reset(&self) -> Result<(), String> {
        self.client.borrow_mut().batch_execute(
            "DROP TABLE IF EXISTS records, spaces, gates, backups, owners;
             CREATE TABLE spaces (name TEXT PRIMARY KEY, wrapped BYTEA NOT NULL);
             CREATE TABLE records (space TEXT NOT NULL, conversation TEXT NOT NULL,
                 sealed BYTEA NOT NULL, PRIMARY KEY (space, conversation));
             CREATE TABLE gates (space TEXT NOT NULL, machine TEXT NOT NULL, state BYTEA NOT NULL,
                 PRIMARY KEY (space, machine));
             CREATE TABLE backups (space TEXT PRIMARY KEY, policy TEXT NOT NULL);
             CREATE TABLE owners (space TEXT NOT NULL, machine TEXT NOT NULL, member TEXT NOT NULL,
                 PRIMARY KEY (space, machine));",
        ).map_err(|e| match e.as_db_error() {
            Some(db) => format!("{}: {}", db.code().code(), db.message()),
            None => e.to_string(),
        })
    }

    fn one_bytes(&self, sql: &str, params: &[&(dyn postgres::types::ToSql + Sync)]) -> Option<Vec<u8>> {
        self.ensure_connected();
        self.client.borrow_mut().query_opt(sql, params).ok().flatten().map(|r| r.get::<_, Vec<u8>>(0))
    }
}

impl Store for PgStore {
    fn load_space(&self, name: &str) -> Option<Space> {
        self.ensure_connected();
        let wrapped = self.one_bytes("SELECT wrapped FROM spaces WHERE name = $1", &[&name])?;
        crate::wrap::unwrap_space(&wrapped, &self.master).ok()
    }

    fn save_space(&mut self, space: Space) {
        self.ensure_connected();
        if let Ok(wrapped) = crate::wrap::wrap_space(&space, &self.master) {
            let _ = self.client.borrow_mut().execute(
                "INSERT INTO spaces (name, wrapped) VALUES ($1, $2)
                 ON CONFLICT (name) DO UPDATE SET wrapped = EXCLUDED.wrapped",
                &[&space.name, &wrapped]);
        }
    }

    fn record(&self, space: &str, conv: &str) -> Option<Vec<u8>> {
        self.ensure_connected();
        self.one_bytes("SELECT sealed FROM records WHERE space = $1 AND conversation = $2", &[&space, &conv])
    }

    fn save_trust_list(&mut self, space: &str, bytes: Vec<u8>) {
        self.ensure_connected();
        let _ = self.client.borrow_mut().execute(
            "INSERT INTO trust (space, signed_list) VALUES ($1, $2)
             ON CONFLICT (space) DO UPDATE SET signed_list = EXCLUDED.signed_list",
            &[&space, &bytes]);
    }

    fn load_trust_list(&self, space: &str) -> Option<Vec<u8>> {
        self.ensure_connected();
        self.client.borrow_mut()
            .query_opt("SELECT signed_list FROM trust WHERE space = $1", &[&space])
            .ok().flatten().map(|r| r.get::<_, Vec<u8>>(0))
    }

    fn save_admin_key(&mut self, space: &str, key: Vec<u8>) {
        self.ensure_connected();
        // Pinned once: a later attempt to change the admin key is ignored.
        let _ = self.client.borrow_mut().execute(
            "INSERT INTO admin_keys (space, admin) VALUES ($1, $2) ON CONFLICT (space) DO NOTHING",
            &[&space, &key]);
    }

    fn load_admin_key(&self, space: &str) -> Option<Vec<u8>> {
        self.ensure_connected();
        self.client.borrow_mut()
            .query_opt("SELECT admin FROM admin_keys WHERE space = $1", &[&space])
            .ok().flatten().map(|r| r.get::<_, Vec<u8>>(0))
    }

    fn ping(&self) -> bool {
        self.ensure_connected();
        self.client.borrow_mut().simple_query("SELECT 1").is_ok()
    }

    fn list_conversations(&self, space: &str) -> Vec<String> {
        self.ensure_connected();
        let rows = self.client.borrow_mut()
            .query("SELECT conversation FROM records WHERE space = $1 ORDER BY conversation", &[&space])
            .unwrap_or_default();
        rows.iter().map(|r| r.get::<_, String>(0)).collect()
    }

    fn put_record(&mut self, space: &str, conv: &str, sealed: Vec<u8>) {
        self.ensure_connected();
        let _ = self.client.borrow_mut().execute(
            "INSERT INTO records (space, conversation, sealed) VALUES ($1, $2, $3)
             ON CONFLICT (space, conversation) DO UPDATE SET sealed = EXCLUDED.sealed",
            &[&space, &conv, &sealed]);
    }

    fn load_gate(&self, space: &str, machine: &str) -> Option<JoinGate> {
        self.ensure_connected();
        let state = self.one_bytes("SELECT state FROM gates WHERE space = $1 AND machine = $2", &[&space, &machine])?;
        JoinGate::from_bytes(&state, &self.master)
    }

    fn save_gate(&mut self, space: &str, machine: &str, gate: JoinGate) {
        self.ensure_connected();
        let state = gate.to_bytes(&self.master);
        let _ = self.client.borrow_mut().execute(
            "INSERT INTO gates (space, machine, state) VALUES ($1, $2, $3)
             ON CONFLICT (space, machine) DO UPDATE SET state = EXCLUDED.state",
            &[&space, &machine, &state]);
    }

    fn load_backup(&self, space: &str) -> Option<BackupPolicy> {
        self.ensure_connected();
        let row = self.client.borrow_mut()
            .query_opt("SELECT policy FROM backups WHERE space = $1", &[&space]).ok().flatten()?;
        serde_json::from_str(&row.get::<_, String>(0)).ok()
    }

    fn save_backup(&mut self, space: &str, policy: BackupPolicy) {
        self.ensure_connected();
        let text = serde_json::to_string(&policy).unwrap_or_default();
        let _ = self.client.borrow_mut().execute(
            "INSERT INTO backups (space, policy) VALUES ($1, $2)
             ON CONFLICT (space) DO UPDATE SET policy = EXCLUDED.policy",
            &[&space, &text]);
    }

    fn load_owner(&self, space: &str, machine: &str) -> Option<String> {
        self.ensure_connected();
        self.client.borrow_mut()
            .query_opt("SELECT member FROM owners WHERE space = $1 AND machine = $2", &[&space, &machine])
            .ok().flatten().map(|r| r.get::<_, String>(0))
    }

    fn load_enrolled_at(&self, space: &str, machine: &str) -> Option<u64> {
        self.ensure_connected();
        self.client.borrow_mut()
            .query_opt("SELECT at FROM enrolled WHERE space = $1 AND machine = $2", &[&space, &machine])
            .ok().flatten().map(|r| r.get::<_, i64>(0) as u64)
    }

    fn save_enrolled_at(&mut self, space: &str, machine: &str, at: u64) {
        self.ensure_connected();
        // The first confirmed enrolment sets the start of the renewal window; later ones keep it.
        let _ = self.client.borrow_mut().execute(
            "INSERT INTO enrolled (space, machine, at) VALUES ($1, $2, $3)
             ON CONFLICT (space, machine) DO NOTHING",
            &[&space, &machine, &(at as i64)]);
    }

    fn save_owner(&mut self, space: &str, machine: &str, member: &str) {
        self.ensure_connected();
        let _ = self.client.borrow_mut().execute(
            "INSERT INTO owners (space, machine, member) VALUES ($1, $2, $3)
             ON CONFLICT (space, machine) DO UPDATE SET member = EXCLUDED.member",
            &[&space, &machine, &member]);
    }

    fn load_device(&self, space: &str, machine: &str) -> Option<Vec<u8>> {
        self.ensure_connected();
        self.client.borrow_mut()
            .query_opt("SELECT entry FROM devices WHERE space = $1 AND machine = $2", &[&space, &machine])
            .ok().flatten().map(|r| r.get::<_, Vec<u8>>(0))
    }

    fn save_device(&mut self, space: &str, machine: &str, entry: Vec<u8>) {
        self.ensure_connected();
        let _ = self.client.borrow_mut().execute(
            "INSERT INTO devices (space, machine, entry) VALUES ($1, $2, $3)
             ON CONFLICT (space, machine) DO UPDATE SET entry = EXCLUDED.entry",
            &[&space, &machine, &entry]);
    }
    fn devices(&self, space: &str) -> Vec<(String, Vec<u8>)> {
        self.ensure_connected();
        let rows = self.client.borrow_mut()
            .query("SELECT machine, entry FROM devices WHERE space = $1 ORDER BY machine", &[&space])
            .unwrap_or_default();
        rows.iter().map(|r| (r.get::<_, String>(0), r.get::<_, Vec<u8>>(1))).collect()
    }
    fn remove_device(&mut self, space: &str, machine: &str) {
        self.ensure_connected();
        let _ = self.client.borrow_mut().execute(
            "DELETE FROM devices WHERE space = $1 AND machine = $2", &[&space, &machine]);
    }

    fn load_token_key(&self, space: &str) -> Option<[u8; 32]> {
        self.ensure_connected();
        self.load_space(space).and_then(|sp| sp.token_key())
    }
}
