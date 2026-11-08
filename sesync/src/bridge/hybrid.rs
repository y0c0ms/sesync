//! The hybrid handshake sesync clients speak (protocol v3; `sesync machine` is the client). The phone keeps
//! Orca's legacy channel (`e2ee`), which has no post-quantum part.
//!
//! 1. Client → plaintext `{"type":"e2ee_hello","v":3,"publicKeyB64":X25519,"mlkemEncapsulationKeyB64":EK}`:
//!    a fresh X25519 key and a fresh ML-KEM-768 encapsulation key.
//! 2. Bridge → plaintext `{"type":"e2ee_ready","v":3,"mlkemCiphertextB64":CT}`: it encapsulates to EK.
//! 3. Both hold two secrets: X25519 between the client's key and the bridge's pinned key (the one the pairing
//!    offer carries), and the ML-KEM shared key. HKDF-SHA384 over both, salted with this protocol's label and
//!    bound to a SHA-384 of the transcript (both X25519 keys, EK, CT), gives one AES-256-GCM key per direction.
//!
//! A recorded session needs both secrets to read: the ML-KEM one is fresh per connection, so breaking X25519 later
//! is not enough. The bridge is still authenticated only by its X25519 key (pinned by the offer): a signature
//! with ML-DSA would be needed against an attacker who breaks X25519 while the connection is live.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use crypto_box::{PublicKey, SecretKey};
use curve25519_dalek::montgomery::MontgomeryPoint;
use ml_kem::kem::{Decapsulate, Encapsulate, Kem, KeyExport, TryKeyInit};
use ml_kem::{EncapsulationKey, MlKem768};
use ring::digest::{self, SHA384};
use ring::hkdf;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{json, Value};

/// The hello's `v` for this handshake.
pub const VERSION: u64 = 3;
/// The HKDF salt: names the protocol and its parts, so these keys can never equal another protocol's.
const LABEL: &[u8] = b"sesync bridge v3: X25519 + ML-KEM-768, HKDF-SHA384, AES-256-GCM";

/// The two traffic keys a handshake yields.
pub struct Keys {
    pub client_to_bridge: [u8; 32],
    pub bridge_to_client: [u8; 32],
}

/// The bridge's side: from the client's hello fields, the `e2ee_ready` reply and the keys.
pub fn accept(bridge: &SecretKey, client: &PublicKey, encapsulation_key_b64: &str) -> Result<(Value, Keys), String> {
    let ek_bytes = STANDARD.decode(encapsulation_key_b64).map_err(|_| "the ML-KEM key is not base64".to_string())?;
    let ek = EncapsulationKey::<MlKem768>::new_from_slice(&ek_bytes)
        .map_err(|_| "the ML-KEM key is not an ML-KEM-768 encapsulation key".to_string())?;
    let (ct, shared) = ek.encapsulate();
    let transcript = Transcript { client, bridge: &bridge.public_key(), ek: &ek_bytes, ct: ct.as_slice() };
    let keys = derive(bridge, client, &transcript, shared.as_slice())?;
    let ready = json!({ "type": "e2ee_ready", "v": VERSION, "mlkemCiphertextB64": STANDARD.encode(ct.as_slice()) });
    Ok((ready, keys))
}

/// The client's side between its hello and the bridge's reply.
pub struct Pending {
    secret: SecretKey,
    decapsulation: <MlKem768 as Kem>::DecapsulationKey,
    encapsulation_key: Vec<u8>,
}

/// Start a handshake: the hello to send and what `finish` needs.
pub fn begin() -> Result<(Value, Pending), String> {
    let mut secret = [0u8; 32];
    SystemRandom::new().fill(&mut secret).map_err(|_| "no randomness for a key".to_string())?;
    let secret = SecretKey::from_bytes(secret);
    let (decapsulation, encapsulation) = MlKem768::generate_keypair();
    let encapsulation_key = encapsulation.to_bytes().to_vec();
    let hello = json!({
        "type": "e2ee_hello",
        "v": VERSION,
        "publicKeyB64": STANDARD.encode(secret.public_key().as_bytes()),
        "mlkemEncapsulationKeyB64": STANDARD.encode(&encapsulation_key),
    });
    Ok((hello, Pending { secret, decapsulation, encapsulation_key }))
}

/// Finish a handshake with the bridge's pinned key and its `e2ee_ready` reply. A reply without this handshake's
/// ciphertext (a bridge that only speaks the legacy channel) is refused: there is no fallback to it.
pub fn finish(pending: Pending, bridge: &PublicKey, ready: &Value) -> Result<Keys, String> {
    if ready["type"] != "e2ee_ready" || ready["v"] != VERSION {
        return Err("that bridge does not speak the hybrid (ML-KEM) handshake; update sesync there".into());
    }
    let ct = ready["mlkemCiphertextB64"]
        .as_str()
        .and_then(|b64| STANDARD.decode(b64).ok())
        .ok_or("the bridge's reply has no ML-KEM ciphertext")?;
    let shared = pending
        .decapsulation
        .decapsulate_slice(&ct)
        .map_err(|_| "the bridge's ML-KEM ciphertext has the wrong length".to_string())?;
    let client = pending.secret.public_key();
    let transcript = Transcript { client: &client, bridge, ek: &pending.encapsulation_key, ct: &ct };
    derive(&pending.secret, bridge, &transcript, shared.as_slice())
}

/// What both sides sent, in role order: it binds the keys to this exchange.
struct Transcript<'a> {
    client: &'a PublicKey,
    bridge: &'a PublicKey,
    ek: &'a [u8],
    ct: &'a [u8],
}

/// HKDF-SHA384 over the X25519 secret (this side's `secret` with the `peer`'s public key) and the ML-KEM one.
fn derive(secret: &SecretKey, peer: &PublicKey, transcript: &Transcript, ml_kem: &[u8]) -> Result<Keys, String> {
    let x25519 = MontgomeryPoint(*peer.as_bytes()).mul_clamped(secret.to_bytes()).to_bytes();
    if x25519 == [0u8; 32] {
        return Err("the peer's X25519 key is a low-order point".into());
    }
    let mut bound = Vec::with_capacity(64 + transcript.ek.len() + transcript.ct.len());
    for part in [transcript.client.as_bytes().as_slice(), transcript.bridge.as_bytes(), transcript.ek, transcript.ct] {
        bound.extend_from_slice(part);
    }
    let hash = digest::digest(&SHA384, &bound);
    let mut ikm = Vec::with_capacity(32 + ml_kem.len());
    ikm.extend_from_slice(&x25519);
    ikm.extend_from_slice(ml_kem);
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA384, LABEL).extract(&ikm);
    let expand = |direction: &[u8]| -> Result<[u8; 32], String> {
        let mut key = [0u8; 32];
        prk.expand(&[direction, hash.as_ref()], Len32)
            .and_then(|okm| okm.fill(&mut key))
            .map_err(|_| "key derivation failed".to_string())?;
        Ok(key)
    };
    Ok(Keys { client_to_bridge: expand(b"client to bridge")?, bridge_to_client: expand(b"bridge to client")? })
}

struct Len32;

impl hkdf::KeyType for Len32 {
    fn len(&self) -> usize {
        32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::e2ee::{self, Channel, Side};

    fn bridge_key() -> SecretKey {
        SecretKey::from_bytes([2; 32])
    }

    /// A full handshake: the hello a client sends, the bridge's reply, and the keys each side ends with.
    fn handshake() -> (Keys, Keys, Value) {
        let bridge = bridge_key();
        let (hello, pending) = begin().unwrap();
        let client = e2ee::parse_client_key(hello["publicKeyB64"].as_str().unwrap()).unwrap();
        let (ready, bridge_keys) = accept(&bridge, &client, hello["mlkemEncapsulationKeyB64"].as_str().unwrap()).unwrap();
        let client_keys = finish(pending, &bridge.public_key(), &ready).unwrap();
        (client_keys, bridge_keys, ready)
    }

    #[test]
    fn both_sides_derive_the_same_two_keys_and_each_connection_gets_new_ones() {
        let (client, bridge, _) = handshake();
        assert_eq!(client.client_to_bridge, bridge.client_to_bridge);
        assert_eq!(client.bridge_to_client, bridge.bridge_to_client);
        assert_ne!(client.client_to_bridge, client.bridge_to_client, "each direction has its own key");
        let (again, _, _) = handshake();
        assert_ne!(again.client_to_bridge, client.client_to_bridge, "fresh keys per connection");
        let (c, b) = (Channel::hybrid(&client, Side::Client), Channel::hybrid(&bridge, Side::Bridge));
        assert_eq!(b.open(&c.seal("status.get").unwrap()).as_deref(), Some("status.get"));
    }

    #[test]
    fn a_client_that_pinned_another_bridge_key_or_got_a_changed_ciphertext_shares_no_key() {
        let bridge = bridge_key();
        let (hello, pending) = begin().unwrap();
        let client = e2ee::parse_client_key(hello["publicKeyB64"].as_str().unwrap()).unwrap();
        let (ready, bridge_keys) = accept(&bridge, &client, hello["mlkemEncapsulationKeyB64"].as_str().unwrap()).unwrap();
        let impostor = SecretKey::from_bytes([5; 32]).public_key();
        let pinned_other = finish(pending, &impostor, &ready).unwrap();
        assert_ne!(pinned_other.client_to_bridge, bridge_keys.client_to_bridge, "the X25519 half binds the pinned key");

        let (hello, pending) = begin().unwrap();
        let client = e2ee::parse_client_key(hello["publicKeyB64"].as_str().unwrap()).unwrap();
        let (mut ready, bridge_keys) = accept(&bridge, &client, hello["mlkemEncapsulationKeyB64"].as_str().unwrap()).unwrap();
        let mut ct = STANDARD.decode(ready["mlkemCiphertextB64"].as_str().unwrap()).unwrap();
        ct[0] ^= 1;
        ready["mlkemCiphertextB64"] = json!(STANDARD.encode(&ct));
        let changed = finish(pending, &bridge.public_key(), &ready).unwrap();
        assert_ne!(changed.client_to_bridge, bridge_keys.client_to_bridge, "the ML-KEM half binds the ciphertext");
    }

    #[test]
    fn a_legacy_ready_or_a_broken_hello_is_refused_with_no_fallback() {
        let (_, pending) = begin().unwrap();
        let legacy = json!({ "type": "e2ee_ready" });
        let err = finish(pending, &bridge_key().public_key(), &legacy).err().unwrap();
        assert!(err.contains("hybrid"), "{err}");
        let (_, pending) = begin().unwrap();
        let short = json!({ "type": "e2ee_ready", "v": VERSION, "mlkemCiphertextB64": STANDARD.encode([0u8; 10]) });
        assert!(finish(pending, &bridge_key().public_key(), &short).is_err());
        let client = SecretKey::from_bytes([4; 32]).public_key();
        assert!(accept(&bridge_key(), &client, &STANDARD.encode([0u8; 100])).is_err(), "not an ML-KEM-768 key");
        assert!(accept(&bridge_key(), &client, "!!").is_err());
        let low_order = PublicKey::from_bytes([0; 32]);
        let (hello, _) = begin().unwrap();
        assert!(accept(&bridge_key(), &low_order, hello["mlkemEncapsulationKeyB64"].as_str().unwrap()).is_err());
    }
}
