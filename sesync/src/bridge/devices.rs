//! Paired phones. A device token is 256 random bits, printed once by `sesync bridge pair`. The
//! store keeps only its SHA-256, so reading `devices.json` cannot reveal a token that would work.
//! SHA-256 rather than a slow hash is deliberate: the input carries 256 bits of entropy, so there
//! is nothing to brute-force.
//!
//! Every authenticated frame re-reads the store, so revoking a device takes effect on its next
//! frame, with no restart. Any read failure denies access.

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub label: String,
    /// Seconds since the epoch.
    pub created: u64,
    /// Lower-case hex SHA-256 of the device token.
    pub token_sha256: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    devices: Vec<Device>,
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join("devices.json")
}

/// Every paired device. A missing file is an empty list; an unreadable one is an error.
pub fn list(dir: &Path) -> Result<Vec<Device>, String> {
    Ok(read(dir)?.devices)
}

/// Pair a new device. Returns the record and the token, which exists only in this return value.
pub fn create(dir: &Path, label: &str, now: u64) -> Result<(Device, String), String> {
    let mut store = read(dir)?;
    let token = new_token()?;
    let device = Device {
        id: random_hex(8)?,
        label: label.to_string(),
        created: now,
        token_sha256: token_hash(&token),
    };
    store.devices.push(device.clone());
    write(dir, &store)?;
    Ok((device, token))
}

/// Remove the device with exactly this id. Its token stops working on the next frame.
pub fn revoke(dir: &Path, id: &str) -> Result<Device, String> {
    let mut store = read(dir)?;
    let at = store.devices.iter().position(|d| d.id == id)
        .ok_or_else(|| format!("no paired device with id {id}"))?;
    let device = store.devices.remove(at);
    write(dir, &store)?;
    Ok(device)
}

/// True only when `token` matches a paired device. Any read failure is false.
pub fn verify(dir: &Path, token: &str) -> bool {
    let Ok(store) = read(dir) else { return false };
    let presented = token_hash(token);
    // Compare every entry, not until the first match, so timing does not reveal which device.
    store.devices.iter().fold(false, |found, d| found | constant_time_eq(presented.as_bytes(), d.token_sha256.as_bytes()))
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
    fn a_created_token_verifies_and_the_file_holds_only_its_hash() {
        let dir = sandbox("hash-only");
        let (device, token) = create(&dir, "phone", 1000).unwrap();
        assert!(verify(&dir, &token));
        assert!(!verify(&dir, "not-the-token"));
        let on_disk = std::fs::read_to_string(path(&dir)).unwrap();
        assert!(!on_disk.contains(&token), "the token itself must never be stored");
        assert!(on_disk.contains(&device.token_sha256));
        assert_eq!(token.len(), 43);
    }

    #[test]
    fn revoking_stops_the_token_and_only_that_device() {
        let dir = sandbox("revoke");
        let (a, token_a) = create(&dir, "a", 1).unwrap();
        let (_, token_b) = create(&dir, "b", 2).unwrap();
        revoke(&dir, &a.id).unwrap();
        assert!(!verify(&dir, &token_a), "a revoked token is refused on the next check");
        assert!(verify(&dir, &token_b));
        assert!(revoke(&dir, &a.id).is_err(), "revoking twice is an error, not a silent no-op");
    }

    #[test]
    fn a_damaged_store_denies_every_token() {
        let dir = sandbox("damaged");
        let (_, token) = create(&dir, "p", 1).unwrap();
        std::fs::write(path(&dir), "{ not json").unwrap();
        assert!(!verify(&dir, &token));
        assert!(create(&dir, "q", 2).is_err(), "a write never overwrites a store it cannot read");
    }
}
