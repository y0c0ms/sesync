use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use crypto_box::{aead::OsRng, PublicKey, SecretKey};
use ring::rand::SystemRandom;

use crate::membership::Device;

/// A random 256-bit key that seals one session. Never printed: Debug shows only a placeholder.
pub struct DataKey([u8; 32]);

impl std::fmt::Debug for DataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DataKey(<redacted>)")
    }
}

impl DataKey {
    pub fn random() -> DataKey {
        let mut k = [0u8; 32];
        ring::rand::SecureRandom::fill(&SystemRandom::new(), &mut k).expect("system randomness");
        DataKey(k)
    }
    pub fn as_bytes(&self) -> &[u8; 32] { &self.0 }
    pub fn from_bytes(b: [u8; 32]) -> DataKey { DataKey(b) }
}

/// The data key wrapped to one device: a sealed box addressed to the device's box key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wrap {
    /// The device this wrap is for, by its box public key.
    pub device: Vec<u8>,
    /// The crypto_box sealed-box bytes (ephemeral key, nonce and ciphertext).
    pub sealed: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WrapError {
    NotForThisDevice,
    Failed,
}

/// Wrap a data key to each device in `devices`. Each wrap is a sealed box: only the device's box
/// private key can open it, and a fresh ephemeral key is used per wrap.
pub fn wrap_for(key: &DataKey, devices: &[Vec<u8>]) -> Result<Vec<Wrap>, WrapError> {
    let mut out = Vec::with_capacity(devices.len());
    for dev in devices {
        let pk_bytes: [u8; 32] = dev.as_slice().try_into().map_err(|_| WrapError::Failed)?;
        let pk = PublicKey::from(pk_bytes);
        let sealed = pk.seal(&mut OsRng, key.as_bytes()).map_err(|_| WrapError::Failed)?;
        out.push(Wrap { device: dev.clone(), sealed });
    }
    Ok(out)
}

/// Recover the data key for `device`, if a wrap is addressed to it and opens.
pub fn unwrap_for(device: &Device, wraps: &[Wrap]) -> Result<DataKey, WrapError> {
    let mine = device.box_public();
    let w = wraps.iter().find(|w| w.device == mine).ok_or(WrapError::NotForThisDevice)?;
    let secret: &SecretKey = device.box_secret();
    let plain = secret.unseal(&w.sealed).map_err(|_| WrapError::Failed)?;
    let bytes: [u8; 32] = plain.try_into().map_err(|_| WrapError::Failed)?;
    Ok(DataKey(bytes))
}

/// The associated data a session is sealed under: the space, the conversation and the sequence.
/// Each text is length-prefixed, so two different bindings never share an encoding. A session
/// sealed for one space, conversation or sequence does not open under another.
pub fn session_aad(space: &str, conversation: &str, sequence: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + space.len() + conversation.len() + 8);
    out.extend_from_slice(&(space.len() as u64).to_be_bytes());
    out.extend_from_slice(space.as_bytes());
    out.extend_from_slice(&(conversation.len() as u64).to_be_bytes());
    out.extend_from_slice(conversation.as_bytes());
    out.extend_from_slice(&sequence.to_be_bytes());
    out
}

/// Seal a session under its data key, bound to `aad`. Output: nonce || ciphertext-with-tag. The
/// tag covers `aad` without storing it, so the same `aad` must be given to `open_session_bound`.
pub fn seal_session_bound(key: &DataKey, aad: &[u8], plain: &[u8]) -> Result<Vec<u8>, WrapError> {
    let rng = SystemRandom::new();
    let mut nonce = [0u8; 12];
    ring::rand::SecureRandom::fill(&rng, &mut nonce).map_err(|_| WrapError::Failed)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key.as_bytes()));
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad })
        .map_err(|_| WrapError::Failed)?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a session sealed under `key` and bound to `aad`. Fails if the key or the `aad` differ from
/// the ones it was sealed with.
pub fn open_session_bound(key: &DataKey, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, WrapError> {
    if sealed.len() < 12 { return Err(WrapError::Failed); }
    let (nonce, ct) = sealed.split_at(12);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key.as_bytes()));
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| WrapError::Failed)
}
