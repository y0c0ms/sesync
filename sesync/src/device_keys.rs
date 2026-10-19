//! A device's keys: an Ed25519 signing key and an X25519 box key, kept in one owner-only file in
//! the config directory. Created once; reloaded on later runs so the device keeps its identity.
//!
//! The file holds the private keys in hex, one per line. It is never printed, and a file that
//! other users can read is refused rather than used.

use std::path::{Path, PathBuf};

use ring::signature::{Ed25519KeyPair, KeyPair};

const FILE: &str = "device.key";

/// A device's loaded keys.
pub struct DeviceFile {
    signing_pkcs8: Vec<u8>,
    box_secret: [u8; 32],
    signing_public: Vec<u8>,
    box_public: Vec<u8>,
}

impl DeviceFile {
    pub fn public_signing(&self) -> &[u8] { &self.signing_public }
    pub fn public_box(&self) -> &[u8] { &self.box_public }
    pub fn signing_pkcs8(&self) -> &[u8] { &self.signing_pkcs8 }
    pub fn box_secret(&self) -> &[u8; 32] { &self.box_secret }

    /// Read a device file. Refuses a file others can read, and a file that does not parse.
    pub fn read(path: &Path) -> Result<DeviceFile, String> {
        check_owner_only(path)?;
        let text = std::fs::read_to_string(path).map_err(|_| "cannot read the device key file".to_string())?;
        let mut signing = None;
        let mut boxk = None;
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k {
                    "signing" => signing = unhex(v),
                    "box" => boxk = unhex(v),
                    _ => {}
                }
            }
        }
        let (signing_pkcs8, box_bytes) = match (signing, boxk) {
            (Some(s), Some(b)) => (s, b),
            _ => return Err("the device key file is damaged".into()),
        };
        let box_secret: [u8; 32] = box_bytes.try_into().map_err(|_| "the device key file is damaged".to_string())?;
        let pair = Ed25519KeyPair::from_pkcs8(&signing_pkcs8).map_err(|_| "the device key file is damaged".to_string())?;
        let signing_public = pair.public_key().as_ref().to_vec();
        let box_public = box_public_from(&box_secret);
        Ok(DeviceFile { signing_pkcs8, box_secret, signing_public, box_public })
    }

    /// Write a device file owner-only, through a temporary file so a crash never leaves half a key.
    fn write(path: &Path, signing_pkcs8: &[u8], box_secret: &[u8; 32]) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|_| "cannot create the config directory".to_string())?;
            set_owner_only(dir, 0o700)?;
        }
        let text = format!("signing={}\nbox={}\n", hex(signing_pkcs8), hex(box_secret));
        let tmp: PathBuf = path.with_extension("key.tmp");
        {
            let mut f = open_owner_only(&tmp)?;
            use std::io::Write;
            f.write_all(text.as_bytes()).map_err(|_| "cannot write the device key".to_string())?;
            f.sync_all().ok();
        }
        std::fs::rename(&tmp, path).map_err(|_| "cannot save the device key".to_string())
    }
}

/// Load this device's keys from `dir`, creating them on the first run.
pub fn load_or_create(dir: &Path) -> Result<DeviceFile, String> {
    let path = dir.join(FILE);
    if path.exists() {
        return DeviceFile::read(&path);
    }
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| "cannot generate a device key".to_string())?;
    let mut box_secret = [0u8; 32];
    ring::rand::SecureRandom::fill(&rng, &mut box_secret).map_err(|_| "cannot generate a device key".to_string())?;
    DeviceFile::write(&path, pkcs8.as_ref(), &box_secret)?;
    DeviceFile::read(&path)
}

/// The X25519 public key for a box secret, via the standard scalar multiplication.
fn box_public_from(secret: &[u8; 32]) -> Vec<u8> {
    use crypto_box::SecretKey;
    SecretKey::from(*secret).public_key().as_bytes().to_vec()
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 { return None; }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

#[cfg(unix)]
fn open_owner_only(path: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)
        .map_err(|_| "cannot create the device key file".to_string())
}

#[cfg(not(unix))]
fn open_owner_only(path: &Path) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path)
        .map_err(|_| "cannot create the device key file".to_string())
}

#[cfg(unix)]
fn set_owner_only(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|_| "cannot restrict the config directory".to_string())
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path, _mode: u32) -> Result<(), String> { Ok(()) }

#[cfg(unix)]
fn check_owner_only(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|_| "no device key file; it is created on first use".to_string())?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err("the device key file is readable by other users; refusing to use it".into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_owner_only(_path: &Path) -> Result<(), String> { Ok(()) }
