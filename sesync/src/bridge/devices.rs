//! Paired devices. A device token is 256 random bits, printed once by `sesync bridge pair` (a phone) or handed
//! over by `machines.grant` (another machine). The store keeps only its SHA-256, so reading `devices.json` cannot
//! reveal a token that would work. SHA-256 rather than a slow hash is deliberate: the input carries 256 bits of
//! entropy, so there is nothing to brute-force.
//!
//! Every authenticated frame re-reads the store, so revoking a device takes effect on its next frame, with no
//! restart. Any read failure denies access. Writes made in this process are serialised: the bridge serves
//! connections on blocking threads at once, and a grant and a revoke must not overwrite each other.

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// What a paired device is. A `phone` is paired by `sesync bridge pair`, and so is every record written before the
/// field existed. A `machine` is granted by another machine's `machines.grant`: it authenticates and reads as a phone
/// does, but cannot change which machines see each other (`grants.rs`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Phone,
    Machine,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub label: String,
    /// Seconds since the epoch.
    pub created: u64,
    /// Lower-case hex SHA-256 of the device token.
    pub token_sha256: String,
    /// `phone` or `machine`. A record without the field is a phone.
    #[serde(default)]
    pub kind: Kind,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    devices: Vec<Device>,
}

/// Serialises this process's read-modify-write of `devices.json`.
static WRITES: Mutex<()> = Mutex::new(());

fn writing() -> MutexGuard<'static, ()> {
    WRITES.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join("devices.json")
}

/// Every paired device. A missing file is an empty list; an unreadable one is an error.
pub fn list(dir: &Path) -> Result<Vec<Device>, String> {
    Ok(read(dir)?.devices)
}

/// Pair a new device of `kind`. Returns the record and the token, which exists only in this return value.
pub fn create(dir: &Path, label: &str, kind: Kind, now: u64) -> Result<(Device, String), String> {
    let _writing = writing();
    let mut store = read(dir)?;
    let token = new_token()?;
    let device = Device {
        id: random_hex(8)?,
        label: label.to_string(),
        created: now,
        token_sha256: token_hash(&token),
        kind,
    };
    store.devices.push(device.clone());
    write(dir, &store)?;
    Ok((device, token))
}

/// Remove the device with exactly this id, of any kind (`sesync bridge revoke`). Its token stops working on the
/// next frame.
pub fn revoke(dir: &Path, id: &str) -> Result<Device, String> {
    let _writing = writing();
    let mut store = read(dir)?;
    let at = store.devices.iter().position(|d| d.id == id)
        .ok_or_else(|| format!("no paired device with id {id}"))?;
    let device = store.devices.remove(at);
    write(dir, &store)?;
    Ok(device)
}

/// Why `revoke_machine` refused a device.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No paired device has this id.
    Unknown,
    /// The device is a phone: only `sesync bridge revoke` removes one.
    Phone,
    /// The store could not be read or written.
    Failed(String),
}

/// Remove the `machine` device with exactly this id (the `devices.revoke` RPC). A phone is refused.
pub fn revoke_machine(dir: &Path, id: &str) -> Result<Device, Refusal> {
    let _writing = writing();
    let mut store = read(dir).map_err(Refusal::Failed)?;
    let at = store.devices.iter().position(|d| d.id == id).ok_or(Refusal::Unknown)?;
    if store.devices[at].kind != Kind::Machine {
        return Err(Refusal::Phone);
    }
    let device = store.devices.remove(at);
    write(dir, &store).map_err(Refusal::Failed)?;
    Ok(device)
}

/// The kind of the device `token` belongs to, or None when no paired device has it. Any read failure is None.
pub fn authenticate(dir: &Path, token: &str) -> Option<Kind> {
    let store = read(dir).ok()?;
    let presented = token_hash(token);
    // Compare every entry, not until the first match, so timing does not reveal which device.
    store.devices.iter().fold(None, |found, d| {
        if constant_time_eq(presented.as_bytes(), d.token_sha256.as_bytes()) {
            Some(d.kind)
        } else {
            found
        }
    })
}

/// Equal bytes compare equal without an early exit on the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Lower-case hex SHA-256 of `token`.
pub fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// 32 random bytes as unpadded base64url: 43 characters, the form Orca's pairing schema expects.
pub fn new_token() -> Result<String, String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let mut bytes = [0u8; 32];
    SystemRandom::new().fill(&mut bytes)
        .map_err(|_| "no system randomness for a device token".to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Seconds since the epoch, for a device's `created`; 0 when the clock is before the epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn random_hex(len: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; len];
    SystemRandom::new().fill(&mut bytes)
        .map_err(|_| "no system randomness for a device id".to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn read(dir: &Path) -> Result<Store, String> {
    let file = path(dir);
    if !file.exists() {
        return Ok(Store::default());
    }
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    serde_json::from_str(&text).map_err(|_| format!("{} is not a device list", file.display()))
}

fn write(dir: &Path, store: &Store) -> Result<(), String> {
    let json = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    super::write_owner_only(&path(dir), json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sesync-bridge-devices-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_created_token_authenticates_and_the_file_holds_only_its_hash() {
        let dir = sandbox("hash-only");
        let (device, token) = create(&dir, "phone", Kind::Phone, 1000).unwrap();
        assert_eq!(authenticate(&dir, &token), Some(Kind::Phone));
        assert_eq!(authenticate(&dir, "not-the-token"), None);
        let on_disk = std::fs::read_to_string(path(&dir)).unwrap();
        assert!(!on_disk.contains(&token), "the token itself must never be stored");
        assert!(on_disk.contains(&device.token_sha256));
        assert_eq!(token.len(), 43);
    }

    #[test]
    fn revoking_stops_the_token_and_only_that_device() {
        let dir = sandbox("revoke");
        let (a, token_a) = create(&dir, "a", Kind::Phone, 1).unwrap();
        let (_, token_b) = create(&dir, "b", Kind::Phone, 2).unwrap();
        revoke(&dir, &a.id).unwrap();
        assert_eq!(authenticate(&dir, &token_a), None, "a revoked token is refused on the next check");
        assert_eq!(authenticate(&dir, &token_b), Some(Kind::Phone));
        assert!(revoke(&dir, &a.id).is_err(), "revoking twice is an error, not a silent no-op");
    }

    #[test]
    fn a_damaged_store_denies_every_token() {
        let dir = sandbox("damaged");
        let (_, token) = create(&dir, "p", Kind::Phone, 1).unwrap();
        std::fs::write(path(&dir), "{ not json").unwrap();
        assert_eq!(authenticate(&dir, &token), None);
        assert!(create(&dir, "q", Kind::Phone, 2).is_err(), "a write never overwrites a store it cannot read");
    }

    #[test]
    fn an_unknown_kind_is_a_damaged_store_and_denies_every_token() {
        let dir = sandbox("unknown-kind");
        let (_, token) = create(&dir, "p", Kind::Phone, 1).unwrap();
        let text = std::fs::read_to_string(path(&dir)).unwrap().replace("\"phone\"", "\"tablet\"");
        std::fs::write(path(&dir), text).unwrap();
        assert_eq!(authenticate(&dir, &token), None, "a kind this build does not know fails closed");
    }

    #[test]
    fn a_record_without_a_kind_is_a_phone_and_a_machine_grant_round_trips() {
        let dir = sandbox("kind");
        std::fs::create_dir_all(&dir).unwrap();
        let old_token = "an-old-phone-token";
        let old = format!(
            r#"{{"devices":[{{"id":"0a0b","label":"old phone","created":5,"token_sha256":"{}"}}]}}"#,
            token_hash(old_token)
        );
        std::fs::write(path(&dir), old).unwrap();
        assert_eq!(list(&dir).unwrap()[0].kind, Kind::Phone, "a record from before the field is a phone");
        assert_eq!(authenticate(&dir, old_token), Some(Kind::Phone));

        let (grant, grant_token) = create(&dir, "A", Kind::Machine, 9).unwrap();
        assert_eq!(authenticate(&dir, &grant_token), Some(Kind::Machine));
        let kinds: Vec<_> = list(&dir).unwrap().into_iter().map(|d| (d.id, d.kind)).collect();
        assert_eq!(kinds[0], ("0a0b".to_string(), Kind::Phone), "rewriting the store keeps the old phone a phone");
        assert_eq!(kinds[1], (grant.id.clone(), Kind::Machine));
        let on_disk = std::fs::read_to_string(path(&dir)).unwrap();
        assert!(on_disk.contains("\"kind\": \"machine\""), "{on_disk}");
        assert!(on_disk.contains("\"kind\": \"phone\""), "{on_disk}");
    }

    #[test]
    fn a_machine_revoke_removes_only_a_machine_and_refuses_a_phone_or_an_unknown_id() {
        let dir = sandbox("machine-revoke");
        let (phone, phone_token) = create(&dir, "phone", Kind::Phone, 1).unwrap();
        let (grant, grant_token) = create(&dir, "A", Kind::Machine, 2).unwrap();
        assert_eq!(revoke_machine(&dir, &phone.id), Err(Refusal::Phone));
        assert_eq!(revoke_machine(&dir, "no-such-device"), Err(Refusal::Unknown));
        assert_eq!(authenticate(&dir, &phone_token), Some(Kind::Phone), "a refused revoke removes nothing");
        assert_eq!(revoke_machine(&dir, &grant.id).unwrap().id, grant.id);
        assert_eq!(authenticate(&dir, &grant_token), None);
        assert_eq!(authenticate(&dir, &phone_token), Some(Kind::Phone));
    }
}
