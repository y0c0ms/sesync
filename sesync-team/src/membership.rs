//! Device keys and the admin-signed member list.
//!
//! Each device has an Ed25519 key pair; the private key never leaves the device. The member list
//! names the devices allowed to read and write, is signed by the admin key, and carries a version.
//! A verifier accepts a list only if the admin signed it and its version is not older than the one
//! it already holds. A device signs its own records, so a reader knows who wrote them.

use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};

/// A device's keys: an Ed25519 signing key (for records and lists) and an X25519 box key (for
/// receiving wrapped session keys). Both private keys stay on the device.
pub struct Device {
    pair: Ed25519KeyPair,
    pkcs8: Vec<u8>,
    box_secret: crypto_box::SecretKey,
}

impl Device {
    pub fn generate() -> Device {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("system randomness is available");
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("a fresh key parses");
        let box_secret = crypto_box::SecretKey::generate(&mut crypto_box::aead::OsRng);
        Device { pair, pkcs8: pkcs8.as_ref().to_vec(), box_secret }
    }

    /// The X25519 public key others wrap session keys to.
    pub fn box_public(&self) -> Vec<u8> { self.box_secret.public_key().as_bytes().to_vec() }

    pub(crate) fn box_secret(&self) -> &crypto_box::SecretKey { &self.box_secret }

    /// The X25519 public key others wrap session keys to.

    /// The device's agreement private key, taken once to agree a wrapped key.
    pub(crate) fn agreement_private(&self) -> ring::agreement::EphemeralPrivateKey {
        // Re-derive from the stored seed is not possible for an ephemeral key, so the device keeps
        // its agreement private key in a form that can be used for each unwrap.
        unimplemented!("see agreement_key")
    }

    /// Rebuild a device from its stored signing key and box key. Both are stored on the device.
    pub fn from_keys(pkcs8: &[u8], box_secret: [u8; 32]) -> Result<Device, MembershipError> {
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| MembershipError::BadKey)?;
        Ok(Device { pair, pkcs8: pkcs8.to_vec(), box_secret: crypto_box::SecretKey::from(box_secret) })
    }

    pub fn box_secret_bytes(&self) -> [u8; 32] { self.box_secret.to_bytes() }

    pub fn private_pkcs8(&self) -> &[u8] { &self.pkcs8 }

    pub fn public_key(&self) -> Vec<u8> { self.pair.public_key().as_ref().to_vec() }

    pub fn sign(&self, msg: &[u8]) -> Vec<u8> { self.pair.sign(msg).as_ref().to_vec() }

    pub fn verify(public: &[u8], msg: &[u8], sig: &[u8]) -> bool {
        UnparsedPublicKey::new(&ED25519, public).verify(msg, sig).is_ok()
    }
}

/// The member list: a version and the public keys of the devices allowed in the space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberList {
    pub version: u64,
    /// Each entry is a device's signing public key followed by its box public key, both
    /// covered by the admin's signature, so the list is the one source of who may read.
    pub devices: Vec<Vec<u8>>,
}

/// One device entry: signing key and box key, concatenated. Both are 32 bytes.
pub fn device_entry(d: &Device) -> Vec<u8> {
    let mut e = d.public_key();
    e.extend_from_slice(&d.box_public());
    e
}

/// The short form of a device entry that people compare out of band before the admin approves a
/// join: the first 8 bytes of SHA-256 over the entry, as hex in four groups of four
/// (`a1b2-c3d4-e5f6-a7b8`). The server, the CLI and the TUI all show this one form.
pub fn fingerprint(entry: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, entry);
    let hex: String = digest.as_ref()[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}", &hex[0..4], &hex[4..8], &hex[8..12], &hex[12..16])
}

/// What a joining device signs with its own key, proving it holds the private half of the entry
/// it joins with. Knowing a listed machine's name and public keys (every member sees them in the
/// signed list) is then not enough to get a token for that machine. The authenticator code is
/// inside, so a captured request is worthless once its code is spent, and no field can be
/// swapped: each is length-prefixed.
pub fn enrol_message(space: &str, member: &str, machine: &str, code: &str, entry: &[u8]) -> Vec<u8> {
    let mut m = b"sesync enrol v1".to_vec();
    for part in [space.as_bytes(), member.as_bytes(), machine.as_bytes(), code.as_bytes(), entry] {
        m.extend_from_slice(&(part.len() as u32).to_be_bytes());
        m.extend_from_slice(part);
    }
    m
}

impl MemberList {
    /// Build a list from device entries (see `device_entry`).
    pub fn new(version: u64, devices: Vec<Vec<u8>>) -> MemberList {
        let mut devices = devices;
        devices.sort();
        devices.dedup();
        MemberList { version, devices }
    }

    /// The exact bytes the admin signs: version, then each key length-prefixed in sorted order.
    /// Fixed encoding, so the signature covers precisely these bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&(self.devices.len() as u32).to_be_bytes());
        for d in &self.devices {
            out.extend_from_slice(&(d.len() as u32).to_be_bytes());
            out.extend_from_slice(d);
        }
        out
    }

    pub fn sign(self, admin: &Device) -> SignedList {
        let sig = admin.sign(&self.encode());
        SignedList { list: self, signature: sig }
    }
}

/// A member list with the admin's signature over its encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedList {
    pub list: MemberList,
    pub signature: Vec<u8>,
}

impl SignedList {
    /// Length-prefixed: the encoded list, then the signature.
    pub fn to_bytes(&self) -> Vec<u8> {
        let body = self.list.encode();
        let mut out = Vec::new();
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&(self.signature.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.signature);
        out
    }

    pub fn from_bytes(b: &[u8]) -> Result<SignedList, MembershipError> {
        let mut i = 0usize;
        let take = |i: &mut usize, n: usize| -> Result<Vec<u8>, MembershipError> {
            let end = i.checked_add(n).ok_or(MembershipError::Malformed)?;
            let s = b.get(*i..end).ok_or(MembershipError::Malformed)?.to_vec();
            *i = end;
            Ok(s)
        };
        let u32_at = |i: &mut usize| -> Result<u32, MembershipError> {
            let v = take(i, 4)?;
            Ok(u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
        };
        let body_len = u32_at(&mut i)? as usize;
        let body = take(&mut i, body_len)?;
        let sig_len = u32_at(&mut i)? as usize;
        let signature = take(&mut i, sig_len)?;
        if i != b.len() { return Err(MembershipError::Malformed); }
        let list = decode(&body)?;
        Ok(SignedList { list, signature })
    }
}

fn decode(b: &[u8]) -> Result<MemberList, MembershipError> {
    let mut i = 0usize;
    let take = |i: &mut usize, n: usize| -> Result<&[u8], MembershipError> {
        let end = i.checked_add(n).ok_or(MembershipError::Malformed)?;
        let s = b.get(*i..end).ok_or(MembershipError::Malformed)?;
        *i = end;
        Ok(s)
    };
    let version = u64::from_be_bytes(take(&mut i, 8)?.try_into().map_err(|_| MembershipError::Malformed)?);
    let count = u32::from_be_bytes(take(&mut i, 4)?.try_into().map_err(|_| MembershipError::Malformed)?) as usize;
    let mut devices = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let len = u32::from_be_bytes(take(&mut i, 4)?.try_into().map_err(|_| MembershipError::Malformed)?) as usize;
        devices.push(take(&mut i, len)?.to_vec());
    }
    if i != b.len() { return Err(MembershipError::Malformed); }
    Ok(MemberList { version, devices })
}

#[derive(Debug, PartialEq, Eq)]
pub enum MembershipError {
    BadSignature,
    /// The list is older than one already held, so it is ignored.
    Stale { have: u64, got: u64 },
    Malformed,
    BadKey,
}

/// Holds the admin's public key and the newest list accepted. Only lists the admin signed, and
/// never older than the held version, are taken in. Nothing else admits a device: the admin's
/// signed list is the whole roster.
pub struct Verifier {
    admin: Vec<u8>,
    current: Option<MemberList>,
}

impl Verifier {
    pub fn new(admin_public: Vec<u8>) -> Verifier {
        Verifier { admin: admin_public, current: None }
    }

    pub fn accept(&mut self, signed: &SignedList) -> Result<(), MembershipError> {
        if !Device::verify(&self.admin, &signed.list.encode(), &signed.signature) {
            return Err(MembershipError::BadSignature);
        }
        if let Some(cur) = &self.current {
            if signed.list.version < cur.version {
                return Err(MembershipError::Stale { have: cur.version, got: signed.list.version });
            }
        }
        self.current = Some(signed.list.clone());
        Ok(())
    }

    /// The version and member keys of the list currently held, if any.
    pub fn current_view(&self) -> Option<(u64, Vec<Vec<u8>>)> {
        self.current.as_ref().map(|l| (l.version, l.devices.clone()))
    }

    /// The box public keys of the devices in the current list: who a new session is wrapped to.
    pub fn box_keys(&self) -> Vec<Vec<u8>> {
        self.current.as_ref().map_or(Vec::new(), |l| {
            l.devices.iter().filter(|e| e.len() == 64).map(|e| e[32..].to_vec()).collect()
        })
    }

    /// Whether a device is listed, matched by its signing key (the first half of its entry).
    pub fn is_member(&self, device_public: &[u8]) -> bool {
        self.current.as_ref().map_or(false, |l| {
            l.devices.iter().any(|e| e.len() >= 32 && &e[..32] == device_public)
        })
    }
}

