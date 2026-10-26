//! The bridge's Curve25519 identity. The phone pins its public half from the pairing offer, so the
//! file must never be silently replaced: a new key would strand every paired phone. `server.key`
//! holds the 32-byte secret as base64, owner-only; the public key is derived from it, never stored.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use crypto_box::{PublicKey, SecretKey};
use ring::rand::{SecureRandom, SystemRandom};
use std::path::Path;

pub struct ServerKeys {
    pub secret: SecretKey,
    pub public: PublicKey,
}

impl ServerKeys {
    pub fn public_key_b64(&self) -> String {
        STANDARD.encode(self.public.as_bytes())
    }
}

/// Load the key at `path`, creating it on first use. A file that does not hold a 32-byte key is an
/// error: the message never includes the file's contents.
pub fn load_or_create(path: &Path) -> Result<ServerKeys, String> {
    let secret_bytes: [u8; 32] = if path.exists() {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let raw = STANDARD.decode(text.trim())
            .map_err(|_| format!("{} is not a base64 key", path.display()))?;
        let bytes = <[u8; 32]>::try_from(raw.as_slice())
            .map_err(|_| format!("{} does not hold a 32-byte key", path.display()))?;
        super::restrict(path, 0o600)?;
        bytes
    } else {
        let mut bytes = [0u8; 32];
        SystemRandom::new().fill(&mut bytes)
            .map_err(|_| "no system randomness for the bridge key".to_string())?;
        super::write_owner_only(path, STANDARD.encode(bytes).as_bytes())?;
        bytes
    };
    let secret = SecretKey::from_slice(&secret_bytes)
        .map_err(|_| format!("{} does not hold a usable key", path.display()))?;
    let public = secret.public_key();
    Ok(ServerKeys { secret, public })
}
