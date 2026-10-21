//! The team service: the rules the HTTP layer enforces, over the tested core modules.
//! Storage is behind the `Store` trait. `MemoryStore` is used by the tests; Postgres
//! implements the same trait for the server. No rule lives in the storage layer.

use crate::backup::{BackupPolicy, Retention};
use crate::lease::{Lease, LeaseError, LeaseTable};
use crate::spaces::{Space, SpaceError};
use crate::totp::{JoinError, JoinGate, MemberAttempts};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, PartialEq, Eq)]
pub enum ServiceError {
    /// The store could not answer. Not a judgement on the request: retry later.
    Unavailable,
    /// The request conflicts with what exists, such as creating a team that already exists.
    Conflict(String),
    NoSpace(String),
    NotMember(String),
    Auth(String),
    Lease(String),
    Crypto(String),
    Policy(String),
    /// The request names something the service cannot accept, such as a member name a token
    /// cannot carry. The message says what to change.
    Invalid(String),
    /// The device key was revoked or its join declined: it is refused at every later join.
    Removed,
    /// Too many open join requests; the message says which limit.
    Limit(String),
}

/// A join waiting for the admin: a device that proved the team code with a key the signed list
/// does not name. It binds nothing and opens nothing until the admin signs the key into the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinRequest {
    pub member: String,
    pub machine: String,
    pub entry: Vec<u8>,
    pub requested_at: u64,
}

/// What a code-confirmed join led to.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// A team without a trust root: the code alone admits (legacy behaviour).
    Legacy(String),
    /// The key is in the current signed list and bound to this machine.
    Listed(String),
    /// A new key: a join request now waits for the admin. The token opens nothing until then.
    Pending { token: String, fingerprint: String },
}

impl Admission {
    pub fn token(&self) -> &str {
        match self {
            Admission::Legacy(t) | Admission::Listed(t) => t,
            Admission::Pending { token, .. } => token,
        }
    }
}

/// A listed device as the admin sees it: the machine and member bound to the entry, when one is.
/// The admin's own key, and a listed key no machine has joined with yet, have neither.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedDevice {
    pub member: Option<String>,
    pub machine: Option<String>,
    pub entry: Vec<u8>,
}

/// The admin's view of a team: the current signed list, who each listed key belongs to, and the
/// joins waiting for approval. Admin only; members never see names or requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roster {
    pub version: u64,
    pub signed_list: Vec<u8>,
    pub listed: Vec<ListedDevice>,
    pub pending: Vec<JoinRequest>,
}

/// Member and machine names travel inside a token whose fields are separated by `|`, so a name
/// holding one would get a token that never authenticates. Empty names and control characters
/// (a newline would split the line-based credential file) are refused too.
fn check_name(kind: &str, name: &str) -> Result<(), ServiceError> {
    if name.is_empty() {
        return Err(ServiceError::Invalid(format!("the {kind} name is empty")));
    }
    if name.contains('|') || name.chars().any(char::is_control) {
        return Err(ServiceError::Invalid(format!(
            "the {kind} name '{}' may not contain '|' or control characters", name.escape_debug())));
    }
    Ok(())
}

impl From<LeaseError> for ServiceError {
    fn from(e: LeaseError) -> Self { ServiceError::Lease(format!("{e:?}")) }
}

impl From<SpaceError> for ServiceError {
    fn from(e: SpaceError) -> Self { ServiceError::Crypto(e.to_string()) }
}

impl From<JoinError> for ServiceError {
    fn from(e: JoinError) -> Self { ServiceError::Auth(format!("{e:?}")) }
}

/// Persistence for the service. Implemented in memory for tests and by Postgres for the
/// server. It stores what the service gives it; it makes no decisions.
pub trait Store {
    /// Load a space by value (or None). Owned, so a remote store works the same way.
    fn load_space(&self, name: &str) -> Option<Space>;
    fn save_space(&mut self, space: Space);
    fn record(&self, space: &str, conv: &str) -> Option<Vec<u8>>;
    fn put_record(&mut self, space: &str, conv: &str, sealed: Vec<u8>);
    /// Conversation ids stored in a space, sorted. Ids only; contents stay sealed.
    fn list_conversations(&self, space: &str) -> Vec<String>;
    /// True if the store answers a trivial query. Used by the health check.
    fn ping(&self) -> bool;
    fn save_trust_list(&mut self, space: &str, bytes: Vec<u8>);
    fn load_trust_list(&self, space: &str) -> Option<Vec<u8>>;
    /// The admin public key pinned for a space at creation.
    fn save_admin_key(&mut self, space: &str, key: Vec<u8>);
    fn load_admin_key(&self, space: &str) -> Option<Vec<u8>>;
    fn load_gate(&self, space: &str, machine: &str) -> Option<JoinGate>;
    fn save_gate(&mut self, space: &str, machine: &str, gate: JoinGate);
    fn load_backup(&self, space: &str) -> Option<BackupPolicy>;
    fn save_backup(&mut self, space: &str, policy: BackupPolicy);
    fn load_token_key(&self, space: &str) -> Option<[u8; 32]>;
    fn load_owner(&self, space: &str, machine: &str) -> Option<String>;
    fn save_owner(&mut self, space: &str, machine: &str, member: &str);
    /// When this machine's enrolment was last confirmed by the code: the start of its renewal
    /// window. Each code-confirmed enrolment moves it to that enrolment; a renewal never does.
    fn load_enrolled_at(&self, space: &str, machine: &str) -> Option<u64>;
    fn save_enrolled_at(&mut self, space: &str, machine: &str, at: u64);
    /// The device entry (signing and box keys) bound to this machine: set when the machine joins
    /// with a listed key or the admin approves its request, cleared when the admin signs the key
    /// out. The machine's authority comes from the entry being named by the current signed list.
    fn load_device(&self, space: &str, machine: &str) -> Option<Vec<u8>>;
    fn save_device(&mut self, space: &str, machine: &str, entry: Vec<u8>);
    /// Every machine binding of a space, as (machine, entry), sorted by machine.
    fn devices(&self, space: &str) -> Vec<(String, Vec<u8>)>;
    /// Unbind a machine. Its token stops working at once.
    fn remove_device(&mut self, space: &str, machine: &str);
    /// The open join requests of a space, sorted by machine. One per machine name.
    fn requests(&self, space: &str) -> Vec<JoinRequest>;
    /// Record or replace the request for `request.machine`.
    fn save_request(&mut self, space: &str, request: &JoinRequest);
    fn remove_request(&mut self, space: &str, machine: &str);
    /// Whether a device entry was revoked or declined in this space.
    fn is_tombstoned(&self, space: &str, entry: &[u8]) -> bool;
    fn save_tombstone(&mut self, space: &str, entry: &[u8]);
    fn remove_tombstone(&mut self, space: &str, entry: &[u8]);
}

#[derive(Default)]
pub struct MemoryStore {
    spaces: HashMap<String, Space>,
    records: HashMap<(String, String), Vec<u8>>,
    gates: HashMap<(String, String), JoinGate>,
    backups: HashMap<String, BackupPolicy>,
    owners: HashMap<(String, String), String>,
    trust: HashMap<String, Vec<u8>>,
    admin_keys: HashMap<String, Vec<u8>>,
    enrolled_at: HashMap<(String, String), u64>,
    devices: HashMap<(String, String), Vec<u8>>,
    requests: HashMap<(String, String), JoinRequest>,
    tombstones: HashSet<(String, Vec<u8>)>,
}

impl Store for MemoryStore {
    fn load_space(&self, name: &str) -> Option<Space> { self.spaces.get(name).and_then(|s| Space::from_bytes(&s.to_bytes())) }
    fn save_space(&mut self, space: Space) { self.spaces.insert(space.name.clone(), space); }
    fn record(&self, space: &str, conv: &str) -> Option<Vec<u8>> {
        self.records.get(&(space.to_string(), conv.to_string())).cloned()
    }
    fn ping(&self) -> bool { true }
    fn save_trust_list(&mut self, space: &str, bytes: Vec<u8>) { self.trust.insert(space.to_string(), bytes); }
    fn load_trust_list(&self, space: &str) -> Option<Vec<u8>> { self.trust.get(space).cloned() }
    fn save_admin_key(&mut self, space: &str, key: Vec<u8>) { self.admin_keys.insert(space.to_string(), key); }
    fn load_admin_key(&self, space: &str) -> Option<Vec<u8>> { self.admin_keys.get(space).cloned() }
    fn list_conversations(&self, space: &str) -> Vec<String> {
        let mut ids: Vec<String> = self.records.keys()
            .filter(|(s, _)| s == space)
            .map(|(_, c)| c.clone())
            .collect();
        ids.sort();
        ids
    }
    fn put_record(&mut self, space: &str, conv: &str, sealed: Vec<u8>) {
        self.records.insert((space.to_string(), conv.to_string()), sealed);
    }
    fn load_gate(&self, space: &str, machine: &str) -> Option<JoinGate> {
        self.gates.get(&(space.to_string(), machine.to_string())).map(|g| g.clone_state())
    }
    fn save_gate(&mut self, space: &str, machine: &str, gate: JoinGate) {
        self.gates.insert((space.to_string(), machine.to_string()), gate);
    }
    fn load_backup(&self, space: &str) -> Option<BackupPolicy> { self.backups.get(space).cloned() }
    fn load_token_key(&self, space: &str) -> Option<[u8; 32]> {
        self.spaces.get(space).and_then(|sp| sp.token_key())
    }
    fn load_owner(&self, space: &str, machine: &str) -> Option<String> {
        self.owners.get(&(space.to_string(), machine.to_string())).cloned()
    }
    fn load_enrolled_at(&self, space: &str, machine: &str) -> Option<u64> {
        self.enrolled_at.get(&(space.to_string(), machine.to_string())).copied()
    }
    fn save_enrolled_at(&mut self, space: &str, machine: &str, at: u64) {
        self.enrolled_at.insert((space.to_string(), machine.to_string()), at);
    }
    fn save_owner(&mut self, space: &str, machine: &str, member: &str) {
        self.owners.insert((space.to_string(), machine.to_string()), member.to_string());
    }
    fn load_device(&self, space: &str, machine: &str) -> Option<Vec<u8>> {
        self.devices.get(&(space.to_string(), machine.to_string())).cloned()
    }
    fn save_device(&mut self, space: &str, machine: &str, entry: Vec<u8>) {
        self.devices.insert((space.to_string(), machine.to_string()), entry);
    }
    fn devices(&self, space: &str) -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<(String, Vec<u8>)> = self.devices.iter()
            .filter(|((s, _), _)| s == space)
            .map(|((_, m), e)| (m.clone(), e.clone()))
            .collect();
        out.sort();
        out
    }
    fn remove_device(&mut self, space: &str, machine: &str) {
        self.devices.remove(&(space.to_string(), machine.to_string()));
    }
    fn requests(&self, space: &str) -> Vec<JoinRequest> {
        let mut out: Vec<JoinRequest> = self.requests.iter()
            .filter(|((s, _), _)| s == space)
            .map(|(_, r)| r.clone())
            .collect();
        out.sort_by(|a, b| a.machine.cmp(&b.machine));
        out
    }
    fn save_request(&mut self, space: &str, request: &JoinRequest) {
        self.requests.insert((space.to_string(), request.machine.clone()), request.clone());
    }
    fn remove_request(&mut self, space: &str, machine: &str) {
        self.requests.remove(&(space.to_string(), machine.to_string()));
    }
    fn is_tombstoned(&self, space: &str, entry: &[u8]) -> bool {
        self.tombstones.contains(&(space.to_string(), entry.to_vec()))
    }
    fn save_tombstone(&mut self, space: &str, entry: &[u8]) {
        self.tombstones.insert((space.to_string(), entry.to_vec()));
    }
    fn remove_tombstone(&mut self, space: &str, entry: &[u8]) {
        self.tombstones.remove(&(space.to_string(), entry.to_vec()));
    }
    fn save_backup(&mut self, space: &str, policy: BackupPolicy) {
        self.backups.insert(space.to_string(), policy);
    }
}

/// Token lifetime: 24 hours. After that the machine must confirm the authenticator again.
pub const TOKEN_LIFETIME_SECS: u64 = 24 * 60 * 60;

/// A member can renew silently for this long after the code-confirmed enrolment. After that the
/// authenticator code is required again.
pub const RENEW_WINDOW_SECS: u64 = 7 * 24 * 60 * 60;

/// A join request the admin has not answered expires after this long: 7 days, the same span as
/// the renewal window the request's token starts.
pub const REQUEST_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Open join requests one member may have at a time.
pub const MAX_REQUESTS_PER_MEMBER: usize = 3;

/// Open join requests one team may have at a time.
pub const MAX_REQUESTS_PER_TEAM: usize = 20;

/// The refusal for a new key under a machine name a listed device already holds. The real device
/// keeps working; the admin revokes it first if the machine really has a new key.
const MACHINE_TAKEN: &str = "machine name already in the team";

pub struct Service<S: Store> {
    store: S,
    /// Leases are in-memory by design: they are short-lived and a restart ends them safely
    /// (every holder must re-acquire, which the fencing version then enforces).
    leases: LeaseTable,
    /// Wrong join codes per (space, member), across machine names. In memory like the leases:
    /// the per-machine gate is the stored one, this only stops guessing under fresh machine names.
    attempts: HashMap<(String, String), MemberAttempts>,
}

impl<S: Store> Service<S> {
    pub fn new(store: S) -> Self {
        Service { store, leases: LeaseTable::default(), attempts: HashMap::new() }
    }

    /// Create a space. Refused if it already exists: replacing one would replace its secret and
    /// content keys, which would lock every member out of their data. The name may hold any
    /// printable character (clients encode it where it becomes a URI or a file name), but not a
    /// control character, and member names must be names a token can carry.
    pub fn create_space(&mut self, name: &str, members: &[&str]) -> Result<(), ServiceError> {
        if name.is_empty() || name.chars().any(char::is_control) {
            return Err(ServiceError::Invalid("the team name is empty or holds a control character".into()));
        }
        for m in members {
            check_name("member", m)?;
        }
        if self.store.load_space(name).is_some() {
            return Err(ServiceError::Conflict(format!("team '{name}' already exists")));
        }
        // A random 160-bit secret per space, stored with the space so it is wrapped at rest.
        let mut secret = vec![0u8; 20];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut secret);
        let mut space = Space::new(name, members);
        space.set_secret(secret);
        let mut tk = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut tk);
        space.set_token_key(tk);
        self.store.save_space(space);
        Ok(())
    }

    /// Add a member to an existing space. Inviting someone already in it does nothing.
    pub fn invite(&mut self, space: &str, who: &str) -> Result<(), ServiceError> {
        check_name("member", who)?;
        let mut s = self.store.load_space(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        s.add_member(who);
        self.store.save_space(s);
        Ok(())
    }

    /// Whether `who` is a member of `space`.
    pub fn is_member(&self, space: &str, who: &str) -> bool {
        self.member(space, who).is_ok()
    }

    /// The space's authenticator secret, for enrolling the authenticator app. Only the
    /// operator who creates a space sees it; the server never returns it over HTTP.
    pub fn space_secret(&self, space: &str) -> Option<Vec<u8>> {
        self.store.load_space(space).and_then(|s| s.secret().map(|b| b.to_vec()))
    }

    /// Consume the service and return its store, so a restart can be simulated.
    pub fn into_store(self) -> S {
        self.store
    }

    fn member(&self, space: &str, who: &str) -> Result<(), ServiceError> {
        let s = self.store.load_space(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        s.seal_for_member_check(who).map_err(|_| ServiceError::NotMember(who.into()))
    }

    /// Check the authenticator code a machine sends for `now`. Refused while the member is locked
    /// out by wrong codes from any machine, as well as while this machine's gate is locked.
    /// Records nothing about the machine: what a confirmed code leads to is `enrol`'s decision.
    fn check_code(&mut self, space: &str, who: &str, machine: &str, code: &str, now: u64) -> Result<(), ServiceError> {
        let secret = self.space_secret(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        let key = (space.to_string(), who.to_string());
        if let Some(a) = self.attempts.get(&key) {
            a.check(now)?;
        }
        let mut gate = self.store.load_gate(space, machine).unwrap_or_else(|| JoinGate::new(secret.clone()));
        let r = gate.verify(code, now);
        self.store.save_gate(space, machine, gate);
        match &r {
            Ok(()) => { self.attempts.remove(&key); }
            Err(JoinError::Wrong) => self.attempts.entry(key).or_default().wrong(now),
            Err(_) => {}
        }
        r.map_err(ServiceError::from)
    }

    /// A machine joins with the authenticator code and, once the team has a trust root, its device
    /// entry. The code proves only that someone was given the authenticator. What it leads to:
    ///
    /// - no trust root (a legacy team): the code alone admits, as it always did;
    /// - a revoked or declined key: refused (`Removed`), no token;
    /// - a key in the current list: re-authentication if it is bound to this machine for this
    ///   member, or bound now if no machine holds it yet; refused if another machine holds it;
    /// - a new key: refused if the machine name is bound to a listed key (a listed machine's key is
    ///   never replaced through a join), otherwise a join request waits for the admin and the
    ///   token opens nothing until the admin signs the key into the list.
    ///
    /// Nothing about the machine is stored before the code is confirmed.
    pub fn enrol(&mut self, space: &str, who: &str, machine: &str, code: &str, device: Option<&[u8]>, now: u64)
        -> Result<Admission, ServiceError>
    {
        self.check_store()?;
        check_name("machine", machine)?;
        self.member(space, who)?;
        let root = self.current_list(space)?;
        if root.is_some() && device.is_none() {
            return Err(ServiceError::Invalid("this team needs the device key; upgrade the client".into()));
        }
        self.check_code(space, who, machine, code, now)?;
        let key = self.store.load_token_key(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        let token = crate::auth::issue_token(&key, who, machine, now + TOKEN_LIFETIME_SECS);
        let Some(list) = root else {
            self.store.save_owner(space, machine, who);
            if let Some(entry) = device {
                self.store.save_device(space, machine, entry.to_vec());
            }
            self.store.save_enrolled_at(space, machine, now);
            return Ok(Admission::Legacy(token));
        };
        let entry = device.unwrap_or_default();
        self.settle(space, &list, now);
        if self.store.is_tombstoned(space, entry) {
            return Err(ServiceError::Removed);
        }
        // After `settle`, every binding names a listed key.
        let bindings = self.store.devices(space);
        let machine_bound = bindings.iter().any(|(m, _)| m == machine);
        if list.list.devices.iter().any(|e| e.as_slice() == entry) {
            match bindings.iter().find(|(_, e)| e.as_slice() == entry) {
                Some((m, _)) if m == machine => {
                    if self.store.load_owner(space, machine).as_deref() != Some(who) {
                        return Err(ServiceError::Conflict("this device is in the team under another member".into()));
                    }
                }
                Some(_) => {
                    return Err(ServiceError::Conflict("this device key is already in the team under another machine name".into()));
                }
                None if machine_bound => {
                    return Err(ServiceError::Conflict(MACHINE_TAKEN.into()));
                }
                None => {
                    // A listed key no machine holds yet (the admin listed it directly): bind it.
                    self.store.save_device(space, machine, entry.to_vec());
                    self.store.save_owner(space, machine, who);
                    self.store.remove_request(space, machine);
                }
            }
            self.store.save_enrolled_at(space, machine, now);
            return Ok(Admission::Listed(token));
        }
        if machine_bound {
            return Err(ServiceError::Conflict(MACHINE_TAKEN.into()));
        }
        let requests = self.store.requests(space);
        if requests.iter().any(|r| r.machine != machine && r.entry.as_slice() == entry) {
            return Err(ServiceError::Conflict("this device key already has a join request under another machine name".into()));
        }
        match requests.iter().find(|r| r.machine == machine) {
            Some(r) if r.member != who => {
                return Err(ServiceError::Conflict("this machine name already has a join request from another member".into()));
            }
            Some(_) => {}
            None => {
                if requests.iter().filter(|r| r.member == who).count() >= MAX_REQUESTS_PER_MEMBER {
                    return Err(ServiceError::Limit(format!(
                        "this member already has {MAX_REQUESTS_PER_MEMBER} join requests waiting; ask the admin to approve or decline them")));
                }
                if requests.len() >= MAX_REQUESTS_PER_TEAM {
                    return Err(ServiceError::Limit(format!(
                        "this team already has {MAX_REQUESTS_PER_TEAM} join requests waiting; ask the admin to approve or decline them")));
                }
            }
        }
        self.store.save_request(space, &JoinRequest {
            member: who.to_string(), machine: machine.to_string(), entry: entry.to_vec(), requested_at: now,
        });
        self.store.save_enrolled_at(space, machine, now);
        Ok(Admission::Pending { token, fingerprint: crate::membership::fingerprint(entry) })
    }

    /// The space's current signed list, parsed. None for a team without a trust root.
    fn current_list(&self, space: &str) -> Result<Option<crate::membership::SignedList>, ServiceError> {
        match self.store.load_trust_list(space) {
            None => Ok(None),
            Some(bytes) => crate::membership::SignedList::from_bytes(&bytes)
                .map(Some)
                .map_err(|_| ServiceError::Crypto("trust root unreadable".into())),
        }
    }

    /// Bring a space's membership records in line with `list` before acting on them: requests
    /// past REQUEST_TTL_SECS are dropped, and a binding whose key the list does not name (left by
    /// the enrolment rules before join requests existed) becomes a join request for the admin to
    /// approve or decline. It is never tombstoned: nobody revoked it. Idempotent.
    fn settle(&mut self, space: &str, list: &crate::membership::SignedList, now: u64) {
        for r in self.store.requests(space) {
            if now >= r.requested_at.saturating_add(REQUEST_TTL_SECS) {
                self.store.remove_request(space, &r.machine);
            }
        }
        for (machine, entry) in self.store.devices(space) {
            if list.list.devices.contains(&entry) {
                continue;
            }
            self.store.remove_device(space, &machine);
            let owner = self.store.load_owner(space, &machine);
            if let Some(member) = owner {
                if !self.store.is_tombstoned(space, &entry) {
                    self.store.save_request(space, &JoinRequest { member, machine, entry, requested_at: now });
                }
            }
        }
    }

    /// Renew a member token without the code. Allowed only if the token is valid for this
    /// machine (or was valid recently enough to prove possession), and the enrolment is still
    /// inside the 7-day window. The new token never outlives the window.
    pub fn renew(&mut self, space: &str, token: &str, machine: &str, now: u64) -> Result<String, ServiceError> {
        let key = self.store.load_token_key(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        let member = crate::auth::check_token_ignoring_expiry(&key, token, machine)
            .map_err(|e| ServiceError::Auth(format!("{e:?}")))?;
        let enrolled = self.store.load_enrolled_at(space, machine)
            .ok_or_else(|| ServiceError::Auth("not enrolled on this machine".into()))?;
        self.device_listed(space, machine)?;
        let window_end = enrolled + RENEW_WINDOW_SECS;
        if now >= window_end {
            return Err(ServiceError::Auth("enrolment window passed; the authenticator code is needed".into()));
        }
        let expires = (now + TOKEN_LIFETIME_SECS).min(window_end);
        Ok(crate::auth::issue_token(&key, &member, machine, expires))
    }

    /// Check a token from a machine and return the member it was issued to. When the team has a
    /// trust root, the machine must also hold a device key the current admin-signed list names:
    /// a revoked device stops here, however long its token would have run.
    pub fn authenticate(&self, space: &str, token: &str, machine: &str, now: u64) -> Result<String, ServiceError> {
        self.check_store()?;
        let key = self.store.load_token_key(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        let member = crate::auth::check_token(&key, token, machine, now)
            .map_err(|e| ServiceError::Auth(format!("{e:?}")))?;
        self.device_listed(space, machine)?;
        Ok(member)
    }

    /// The admin's view of a team: the current signed list, the member and machine bound to each
    /// listed key, and the open join requests. Refused for a team without a trust root.
    pub fn roster(&mut self, space: &str, now: u64) -> Result<Roster, ServiceError> {
        self.check_store()?;
        self.store.load_space(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        let bytes = self.store.load_trust_list(space)
            .ok_or_else(|| ServiceError::Invalid("this team has no admin-signed list".into()))?;
        let list = crate::membership::SignedList::from_bytes(&bytes)
            .map_err(|_| ServiceError::Crypto("trust root unreadable".into()))?;
        self.settle(space, &list, now);
        let bindings = self.store.devices(space);
        let listed = list.list.devices.iter().map(|entry| {
            let machine = bindings.iter().find(|(_, e)| e == entry).map(|(m, _)| m.clone());
            let member = machine.as_deref().and_then(|m| self.store.load_owner(space, m));
            ListedDevice { member, machine, entry: entry.clone() }
        }).collect();
        Ok(Roster { version: list.list.version, signed_list: bytes, listed, pending: self.store.requests(space) })
    }

    /// Decline a join request: its key is tombstoned, so it is refused at every later join, and
    /// the request is dropped. NoSpace when no such request is open, so a typo never reads as done.
    pub fn decline(&mut self, space: &str, member: &str, machine: &str, now: u64) -> Result<(), ServiceError> {
        self.check_store()?;
        if let Some(list) = self.current_list(space)? {
            self.settle(space, &list, now);
        }
        let request = self.store.requests(space).into_iter()
            .find(|r| r.machine == machine && r.member == member)
            .ok_or_else(|| ServiceError::NoSpace(format!("{space}/{member}/{machine}")))?;
        self.store.save_tombstone(space, &request.entry);
        self.store.remove_request(space, machine);
        Ok(())
    }

    /// When the team has a trust root, the machine's bound device must be in the current list.
    /// No trust root (a legacy team) keeps the old behaviour.
    fn device_listed(&self, space: &str, machine: &str) -> Result<(), ServiceError> {
        let Some(list) = self.store.load_trust_list(space) else { return Ok(()) };
        let entry = self.store.load_device(space, machine)
            .ok_or_else(|| ServiceError::NotMember(machine.into()))?;
        let signed = crate::membership::SignedList::from_bytes(&list)
            .map_err(|_| ServiceError::Crypto("trust root unreadable".into()))?;
        if signed.list.devices.iter().any(|e| e == &entry) {
            Ok(())
        } else {
            Err(ServiceError::NotMember(machine.into()))
        }
    }

    pub fn acquire(&mut self, space: &str, conv: &str, machine: &str, now: u64) -> Result<Lease, ServiceError> {
        self.check_store()?;
        self.store.load_space(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        let key = format!("{space}/{conv}");
        // After a restart this table has no grants. Continue from the sequence of the record
        // already stored, so the next version is above every sequence a reader may have accepted.
        if !self.leases.has_grant(&key) {
            if let Some(seq) = self.store.record(space, conv).and_then(|r| crate::signed_records::sequence_of(&r)) {
                self.leases.seed(&key, seq);
            }
        }
        Ok(self.leases.acquire(&key, machine, now)?)
    }

    pub fn release(&mut self, space: &str, conv: &str, machine: &str) -> Result<(), ServiceError> {
        Ok(self.leases.release(&format!("{space}/{conv}"), machine)?)
    }

    /// Store a sealed record. Only the holder of the live lease, under its current version,
    /// may write. The content is sealed with the space key before it is stored.
    pub fn put_record(&mut self, space: &str, conv: &str, machine: &str, version: u64,
                      plaintext: Vec<u8>, now: u64) -> Result<(), ServiceError> {
        let key = format!("{space}/{conv}");
        self.leases.check_write(&key, version, machine)?;
        let _ = now;
        let owner = self.store.load_owner(space, machine)
            .ok_or_else(|| ServiceError::NotMember(machine.into()))?;
        let mut s = self.store.load_space(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        let sealed = s.seal_for(&owner, &plaintext)?;
        self.store.save_space(s);
        self.store.put_record(space, conv, sealed);
        Ok(())
    }

    pub fn get_record(&self, space: &str, conv: &str, who: &str) -> Result<Vec<u8>, ServiceError> {
        self.check_store()?;
        self.member(space, who)?;
        let sealed = self.store.record(space, conv).ok_or_else(|| ServiceError::NoSpace(conv.into()))?;
        let s = self.store.load_space(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        Ok(s.open_for(who, &sealed)?)
    }

    /// Refuse with Unavailable when the store cannot answer, so an outage is never mistaken
    /// for a missing space or a refused code.
    fn check_store(&self) -> Result<(), ServiceError> {
        if self.store.ping() { Ok(()) } else { Err(ServiceError::Unavailable) }
    }

    /// Store the space's trust root: the admin-signed member list as bytes. The server uses it to
    /// accept client-sealed records, and cannot forge it (it is signed by the admin key).
    pub fn store_trust_list(&mut self, space: &str, signed_list: &[u8]) -> Result<(), ServiceError> {
        self.store.save_trust_list(space, signed_list.to_vec());
        Ok(())
    }

    /// The pinned admin public key for a space, if one was set.
    pub fn admin_key(&self, space: &str) -> Option<Vec<u8>> {
        self.store.load_admin_key(space)
    }

    /// The stored trust list for a space, if one was set.
    pub fn load_trust_list_for_test(&self, space: &str) -> Option<Vec<u8>> {
        self.store.load_trust_list(space)
    }

    /// The space's current admin-signed member list, exactly as stored, for members to fetch.
    /// The server does not interpret it here; a fetching client re-verifies the admin signature.
    pub fn trust_list(&self, space: &str) -> Option<Vec<u8>> {
        self.store.load_trust_list(space)
    }

    /// Publish a newer admin-signed member list. The list must be signed by the pinned admin key
    /// and its version must be strictly newer than the stored one, so revocation is monotone:
    /// an older or equal list can never be replayed over a newer one. Returns the new version.
    ///
    /// The list is the roster, so storing it applies the admin's decisions: a key the previous
    /// list named and this one does not is tombstoned and its machine unbound (its token stops at
    /// once and the key never joins again); a new key that answers a join request binds that
    /// request's machine to its member. `removed_members` leave the member set, so the code alone
    /// no longer lets them join, until the admin invites them again.
    pub fn publish_trust_list(&mut self, space: &str, bytes: &[u8], removed_members: &[String], now: u64)
        -> Result<u64, ServiceError>
    {
        self.check_store()?;
        let admin = self.store.load_admin_key(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        let signed = crate::membership::SignedList::from_bytes(bytes)
            .map_err(|_| ServiceError::Crypto("signed list is malformed".into()))?;
        let mut verifier = crate::membership::Verifier::new(admin);
        verifier.accept(&signed)
            .map_err(|_| ServiceError::Crypto("signed list is not signed by the admin key".into()))?;
        let previous = self.current_list(space)?;
        if let Some(prev) = &previous {
            if signed.list.version <= prev.list.version {
                return Err(ServiceError::Conflict(format!(
                    "list version {} is not newer than the stored {}", signed.list.version, prev.list.version)));
            }
        }
        let mut space_record = self.store.load_space(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        for m in removed_members {
            if space_record.seal_for_member_check(m).is_err() {
                return Err(ServiceError::Invalid(format!("'{m}' is not a member of this team")));
            }
        }
        if let Some(prev) = &previous {
            self.settle(space, prev, now);
        }
        let version = signed.list.version;
        self.store.save_trust_list(space, bytes.to_vec());
        let before: Vec<Vec<u8>> = previous.map(|p| p.list.devices).unwrap_or_default();
        let after = &signed.list.devices;
        for gone in before.iter().filter(|e| !after.contains(e)) {
            self.store.save_tombstone(space, gone);
            for (machine, entry) in self.store.devices(space) {
                if &entry == gone {
                    self.store.remove_device(space, &machine);
                }
            }
        }
        for added in after.iter().filter(|e| !before.contains(e)) {
            // Signing a key back in is an explicit admin decision: it lifts the tombstone.
            self.store.remove_tombstone(space, added);
            for r in self.store.requests(space) {
                if &r.entry == added {
                    self.store.save_device(space, &r.machine, r.entry.clone());
                    self.store.save_owner(space, &r.machine, &r.member);
                    self.store.remove_request(space, &r.machine);
                }
            }
        }
        if !removed_members.is_empty() {
            for m in removed_members {
                space_record.remove_member(m)?;
            }
            self.store.save_space(space_record);
            for r in self.store.requests(space) {
                if removed_members.contains(&r.member) {
                    self.store.remove_request(space, &r.machine);
                }
            }
            for (machine, _) in self.store.devices(space) {
                if self.store.load_owner(space, &machine).map_or(false, |o| removed_members.contains(&o)) {
                    self.store.remove_device(space, &machine);
                }
            }
        }
        Ok(version)
    }

    /// Pin the admin's public key for a space. Done once, when the team is created.
    pub fn pin_admin_key(&mut self, space: &str, admin_public: &[u8]) -> Result<(), ServiceError> {
        self.store.save_admin_key(space, admin_public.to_vec());
        Ok(())
    }

    /// Store a client-sealed record after checking it against the space's trust root. The server
    /// does not decrypt it: it verifies the author's signature and the list version, then stores
    /// the bytes exactly as sent.
    pub fn put_client_sealed(&mut self, space: &str, conv: &str, record: Vec<u8>) -> Result<(), ServiceError> {
        self.check_client_sealed(space, conv, &record)?;
        self.store.put_record(space, conv, record);
        Ok(())
    }

    /// The trust-root and binding checks for a client-sealed record, without storing it. Returns the
    /// record's sequence. Shared by the plain and the leased write, so both check the same things.
    fn check_client_sealed(&self, space: &str, conv: &str, record: &[u8]) -> Result<u64, ServiceError> {
        self.check_store()?;
        let list_bytes = self.store.load_trust_list(space).ok_or(ServiceError::NotMember(space.into()))?;
        let admin = self.store.load_admin_key(space).ok_or(ServiceError::NotMember(space.into()))?;
        let signed = crate::membership::SignedList::from_bytes(&list_bytes)
            .map_err(|_| ServiceError::Crypto("trust root unreadable".into()))?;
        // The verifier is anchored to the admin key pinned at team creation, never to a key the
        // list itself names: anyone who can write a list must not be able to choose the admin.
        let mut verifier = crate::membership::Verifier::new(admin);
        verifier.accept(&signed).map_err(|_| ServiceError::Crypto("trust root not admin-signed".into()))?;
        // The record must be bound to this space and conversation: a record sealed for another
        // one is refused here, so it cannot be moved into this conversation.
        let expect = crate::signed_records::Binding {
            space: space.to_string(),
            conversation: conv.to_string(),
            min_sequence: 0,
        };
        let opened = crate::signed_records::open_record(&verifier, &expect, record)
            .map_err(|e| ServiceError::NotMember(format!("{e:?}")))?;
        Ok(opened.sequence)
    }

    /// Store a client-sealed record for the conversation, but only from the machine holding its
    /// lease, under the version the lease granted. The lease is checked first, then the record's
    /// author against the trust root, and its sequence must be exactly the granted version: the
    /// writer cannot pick a sequence, so no one can raise a reader's floor past what was granted.
    /// Nothing is stored unless all of it passes.
    pub fn put_sealed_leased(&mut self, space: &str, conv: &str, machine: &str, version: u64,
                             record: Vec<u8>) -> Result<(), ServiceError> {
        let key = format!("{space}/{conv}");
        self.leases.check_write(&key, version, machine)?;
        // Exact match: a writer may write only under the version it was granted, never a later one.
        let current = self.leases.current_version(&key);
        if current != Some(version) {
            return Err(ServiceError::Lease(format!("version {version} was not granted")));
        }
        let sequence = self.check_client_sealed(space, conv, &record)?;
        if sequence != version {
            return Err(ServiceError::Lease(format!("record sequence {sequence} is not the granted version {version}")));
        }
        self.store.put_record(space, conv, record);
        Ok(())
    }

    /// Whether the store is reachable. The health check reports this, nothing else.
    pub fn healthy(&self) -> bool {
        self.store.ping()
    }

    /// The conversation ids in a space. Only a member may list them.
    pub fn list_conversations(&self, space: &str, who: &str) -> Result<Vec<String>, ServiceError> {
        self.check_store()?;
        self.member(space, who)?;
        Ok(self.store.list_conversations(space))
    }

    /// The conversation ids in a space, and for each one whose stored record is a signed record, the
    /// sequence it carries. Only a member may ask. The sequence is read from the stored bytes without
    /// checking the signature, so it is numbering for the caller's choice of what to fetch. A client
    /// never accepts a record on the strength of it.
    pub fn list_with_sequences(&self, space: &str, who: &str)
        -> Result<(Vec<String>, BTreeMap<String, u64>), ServiceError>
    {
        self.check_store()?;
        self.member(space, who)?;
        let ids = self.store.list_conversations(space);
        let sequences = ids.iter()
            .filter_map(|conv| {
                let seq = self.store.record(space, conv).and_then(|r| crate::signed_records::sequence_of(&r))?;
                Some((conv.clone(), seq))
            })
            .collect();
        Ok((ids, sequences))
    }

    /// The stored bytes, for checks that the store never holds plaintext.
    pub fn raw_stored(&self, space: &str, conv: &str) -> Option<Vec<u8>> {
        self.store.record(space, conv)
    }

    /// Save a backup policy. Refused if the policy is not valid.
    pub fn set_backup(&mut self, space: &str, enabled: bool, interval_hours: u32, copies: u32)
        -> Result<(), ServiceError> {
        let policy = BackupPolicy::new(enabled, interval_hours, Retention::Copies(copies))
            .map_err(|e| ServiceError::Policy(format!("{e:?}")))?;
        self.store.load_space(space).ok_or_else(|| ServiceError::NoSpace(space.into()))?;
        self.store.save_backup(space, policy);
        Ok(())
    }
}

