//! Orca's legacy E2EE framing (`mobile/src/transport/e2ee.ts`, `mobile/scripts/mock-server-encryption.ts`).
//!
//! The first frame is plaintext `{"type":"e2ee_hello","publicKeyB64":…}`; the bridge answers
//! plaintext `{"type":"e2ee_ready"}`. Every later frame is `base64(nonce ‖ box)` under the shared key
//! `nacl.box.before(clientPublic, serverSecret)`, where `box` is NaCl's `secretbox` output: the
//! 16-byte Poly1305 tag followed by the ciphertext. The `crypto_box` crate emits that same layout,
//! which the known-answer test below pins against tweetnacl output byte for byte.
//!
//! Crypto note: X25519 + XSalsa20-Poly1305 is classical. It is acceptable for this bridge only
//! because it runs inside WireGuard, itself classical. The hybrid upgrade is agent-hub #6.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use crypto_box::aead::Aead;
use crypto_box::{Nonce, PublicKey, SalsaBox, SecretKey};
use ring::rand::{SecureRandom, SystemRandom};

const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

/// The shared key for one connection, held as a precomputed box.
pub struct Channel {
    boxed: SalsaBox,
}

impl Channel {
    pub fn new(client: &PublicKey, server: &SecretKey) -> Channel {
        Channel { boxed: SalsaBox::new(client, server) }
    }

    /// Encrypt `plaintext` under a fresh random nonce.
    pub fn seal(&self, plaintext: &str) -> Result<String, String> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce)
            .map_err(|_| "no system randomness for an E2EE frame".to_string())?;
        self.seal_with_nonce(&nonce, plaintext.as_bytes())
    }

    fn seal_with_nonce(&self, nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Result<String, String> {
        let boxed = self.boxed.encrypt(Nonce::from_slice(nonce), plaintext)
            .map_err(|_| "E2EE encryption failed".to_string())?;
        let mut frame = Vec::with_capacity(NONCE_LEN + boxed.len());
        frame.extend_from_slice(nonce);
        frame.extend_from_slice(&boxed);
        Ok(STANDARD.encode(frame))
    }

    /// Encrypt raw bytes for a binary WebSocket message: `nonce ‖ box`, not base64. This is the
    /// layout Orca's legacy channel reads with `openLegacyBinary` (`paired-e2ee-primitives.ts`).
    pub fn seal_bytes(&self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce)
            .map_err(|_| "no system randomness for an E2EE frame".to_string())?;
        let boxed = self.boxed.encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| "E2EE encryption failed".to_string())?;
        let mut frame = Vec::with_capacity(NONCE_LEN + boxed.len());
        frame.extend_from_slice(&nonce);
        frame.extend_from_slice(&boxed);
        Ok(frame)
    }

    /// Decrypt one inbound frame. None for anything that is not a valid frame under this key:
    /// malformed base64, too short, a failed tag, or non-UTF-8 plaintext.
    pub fn open(&self, frame: &str) -> Option<String> {
        let bytes = STANDARD.decode(frame).ok()?;
        if bytes.len() < NONCE_LEN + TAG_LEN {
            return None;
        }
        let (nonce, boxed) = bytes.split_at(NONCE_LEN);
        let plaintext = self.boxed.decrypt(Nonce::from_slice(nonce), boxed).ok()?;
        String::from_utf8(plaintext).ok()
    }
}

/// Parse the client's `publicKeyB64`. None unless it is standard base64 of exactly 32 bytes.
pub fn parse_client_key(b64: &str) -> Option<PublicKey> {
    let raw = STANDARD.decode(b64).ok()?;
    PublicKey::from_slice(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vector from tweetnacl (the library Orca uses), generated with:
    //   client secret = [1;32], server secret = [2;32], nonce = [3;24], plaintext = "hello bridge"
    //   frame = base64(nonce ‖ nacl.box.after(msg, nonce, nacl.box.before(serverPub, clientSecret)))
    // The server public key is nacl.box.keyPair.fromSecretKey([2;32]).publicKey.
    const SERVER_PUBLIC_B64: &str = "zo060cy2M+x7cMF4FKXHbs0CloUFDTRHRboFhw5YfVk=";
    const CLIENT_PUBLIC_B64: &str = "pOCSkrZRwni5dyxWn1+puxPZBrRqtoyd+dwrRAn4ogk=";
    const FRAME_B64: &str =
        "AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDezRpGhWR+3TP77WLo8YzQ4tOE2NENljcTWRBEg==";

    fn secret(byte: u8) -> SecretKey {
        SecretKey::from_slice(&[byte; 32]).unwrap()
    }

    #[test]
    fn the_server_key_matches_tweetnacl_derivation() {
        let server = secret(2);
        assert_eq!(STANDARD.encode(server.public_key().as_bytes()), SERVER_PUBLIC_B64);
    }

    #[test]
    fn a_tweetnacl_frame_opens_and_our_seal_reproduces_it() {
        let client_pub = parse_client_key(CLIENT_PUBLIC_B64).unwrap();
        // The bridge's side of the same shared key: client public against server secret.
        let channel = Channel::new(&client_pub, &secret(2));
        assert_eq!(channel.open(FRAME_B64).as_deref(), Some("hello bridge"));
        let nonce = [3u8; NONCE_LEN];
        assert_eq!(channel.seal_with_nonce(&nonce, b"hello bridge").unwrap(), FRAME_B64);
    }

    #[test]
    fn a_frame_that_fails_the_tag_is_refused() {
        let client_pub = parse_client_key(CLIENT_PUBLIC_B64).unwrap();
        let channel = Channel::new(&client_pub, &secret(2));
        let mut bytes = STANDARD.decode(FRAME_B64).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert_eq!(channel.open(&STANDARD.encode(bytes)), None);
        assert_eq!(channel.open("AAAA"), None, "shorter than nonce plus tag");
        assert_eq!(channel.open("!!"), None, "not base64");
    }

    #[test]
    fn a_client_key_must_be_32_bytes() {
        assert!(parse_client_key(&STANDARD.encode([0u8; 31])).is_none());
        assert!(parse_client_key(&STANDARD.encode([0u8; 33])).is_none());
        assert!(parse_client_key("not base64 !").is_none());
    }
}
