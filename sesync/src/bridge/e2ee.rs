//! The bridge's encrypted channel, in one of two kinds.
//!
//! **Legacy** (the phone): Orca's framing (`mobile/src/transport/e2ee.ts`, `mobile/scripts/mock-server-encryption.ts`).
//! The first frame is plaintext `{"type":"e2ee_hello","publicKeyB64":…}`; the bridge answers
//! plaintext `{"type":"e2ee_ready"}`. Every later frame is `base64(nonce ‖ box)` under the shared key
//! `nacl.box.before(clientPublic, serverSecret)`, where `box` is NaCl's `secretbox` output: the
//! 16-byte Poly1305 tag followed by the ciphertext. The `crypto_box` crate emits that same layout,
//! which the known-answer test below pins against tweetnacl output byte for byte.
//! Crypto note: X25519 + XSalsa20-Poly1305 is classical. It is acceptable for the phone only because it runs
//! inside WireGuard, itself classical; the app has no post-quantum handshake yet.
//!
//! **Hybrid** (sesync clients, `hybrid.rs`): keys from X25519 and ML-KEM-768; every frame is
//! `counter ‖ AES-256-GCM(ciphertext ‖ tag)`, the 8-byte big-endian counter starting at 1 and also the nonce. A
//! frame whose counter is not above the last one opened is refused, so a frame cannot be replayed or reordered.

use super::hybrid;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use crypto_box::aead::Aead;
use crypto_box::{Nonce, PublicKey, SalsaBox, SecretKey};
use parking_lot::Mutex;
use ring::aead::{Aad, LessSafeKey, Nonce as GcmNonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};

const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const COUNTER_LEN: usize = 8;

/// The keys of one connection.
pub struct Channel {
    kind: Kind,
}

enum Kind {
    Legacy(SalsaBox),
    Hybrid(Hybrid),
}

/// Which end of a hybrid channel this is: each end seals with its own direction's key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Bridge,
    Client,
}

struct Hybrid {
    send: LessSafeKey,
    recv: LessSafeKey,
    /// The last counter sealed. Held while a sealed frame is handed on, so frames leave in counter order.
    sent: Mutex<u64>,
    /// The last counter opened.
    seen: Mutex<u64>,
}

impl Channel {
    /// The legacy channel: `client`'s public key against `server`'s secret (or the other way round).
    pub fn new(client: &PublicKey, server: &SecretKey) -> Channel {
        Channel { kind: Kind::Legacy(SalsaBox::new(client, server)) }
    }

    /// The hybrid channel for `side`, from a finished handshake's keys.
    pub fn hybrid(keys: &hybrid::Keys, side: Side) -> Channel {
        let key = |bytes: &[u8; 32]| LessSafeKey::new(UnboundKey::new(&AES_256_GCM, bytes).expect("a 32-byte AES-256 key"));
        let (send, recv) = match side {
            Side::Bridge => (&keys.bridge_to_client, &keys.client_to_bridge),
            Side::Client => (&keys.client_to_bridge, &keys.bridge_to_client),
        };
        Channel { kind: Kind::Hybrid(Hybrid { send: key(send), recv: key(recv), sent: Mutex::new(0), seen: Mutex::new(0) }) }
    }

    /// Seal `plaintext` into one frame's bytes and hand them to `deliver`. For the hybrid channel the counter stays
    /// taken until `deliver` returns, so frames sealed on different threads still leave in counter order.
    pub fn seal_then<T>(&self, plaintext: &[u8], deliver: impl FnOnce(Vec<u8>) -> T) -> Result<T, String> {
        match &self.kind {
            Kind::Legacy(boxed) => {
                let mut nonce = [0u8; NONCE_LEN];
                SystemRandom::new().fill(&mut nonce)
                    .map_err(|_| "no system randomness for an E2EE frame".to_string())?;
                Ok(deliver(legacy_frame(boxed, &nonce, plaintext)?))
            }
            Kind::Hybrid(h) => {
                let mut sent = h.sent.lock();
                let counter = *sent + 1;
                let mut frame = Vec::with_capacity(COUNTER_LEN + plaintext.len() + TAG_LEN);
                frame.extend_from_slice(&counter.to_be_bytes());
                let mut body = plaintext.to_vec();
                h.send.seal_in_place_append_tag(gcm_nonce(counter), Aad::empty(), &mut body)
                    .map_err(|_| "E2EE encryption failed".to_string())?;
                frame.extend_from_slice(&body);
                *sent = counter;
                Ok(deliver(frame))
            }
        }
    }

    /// Encrypt `plaintext` for a text WebSocket message: the frame's bytes in base64.
    pub fn seal(&self, plaintext: &str) -> Result<String, String> {
        self.seal_then(plaintext.as_bytes(), |frame| STANDARD.encode(frame))
    }

    /// Encrypt raw bytes for a binary WebSocket message: the frame's bytes, not base64. For the legacy channel this
    /// is the layout Orca reads with `openLegacyBinary` (`paired-e2ee-primitives.ts`).
    pub fn seal_bytes(&self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        self.seal_then(plaintext, |frame| frame)
    }

    /// Decrypt one inbound text frame. None for anything that is not a valid frame under this key:
    /// malformed base64, too short, a failed tag, a replayed or reordered hybrid frame, or non-UTF-8 plaintext.
    pub fn open(&self, frame: &str) -> Option<String> {
        let bytes = STANDARD.decode(frame).ok()?;
        let plaintext = match &self.kind {
            Kind::Legacy(boxed) => {
                if bytes.len() < NONCE_LEN + TAG_LEN {
                    return None;
                }
                let (nonce, boxed_bytes) = bytes.split_at(NONCE_LEN);
                boxed.decrypt(Nonce::from_slice(nonce), boxed_bytes).ok()?
            }
            Kind::Hybrid(h) => {
                if bytes.len() < COUNTER_LEN + TAG_LEN {
                    return None;
                }
                let (head, body) = bytes.split_at(COUNTER_LEN);
                let counter = u64::from_be_bytes(head.try_into().ok()?);
                let mut seen = h.seen.lock();
                if counter <= *seen {
                    return None;
                }
                let mut body = body.to_vec();
                let plain = h.recv.open_in_place(gcm_nonce(counter), Aad::empty(), &mut body).ok()?.to_vec();
                *seen = counter;
                plain
            }
        };
        String::from_utf8(plaintext).ok()
    }
}

fn legacy_frame(boxed: &SalsaBox, nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let sealed = boxed.encrypt(Nonce::from_slice(nonce), plaintext).map_err(|_| "E2EE encryption failed".to_string())?;
    let mut frame = Vec::with_capacity(NONCE_LEN + sealed.len());
    frame.extend_from_slice(nonce);
    frame.extend_from_slice(&sealed);
    Ok(frame)
}

/// The 96-bit GCM nonce for a counter: four zero bytes, then the counter. Each direction has its own key, so a
/// counter is never used twice under one key.
fn gcm_nonce(counter: u64) -> GcmNonce {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    GcmNonce::assume_unique_for_key(nonce)
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
        let boxed = SalsaBox::new(&client_pub, &secret(2));
        assert_eq!(STANDARD.encode(legacy_frame(&boxed, &nonce, b"hello bridge").unwrap()), FRAME_B64);
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

    fn hybrid_pair() -> (Channel, Channel) {
        let keys = || hybrid::Keys { client_to_bridge: [7; 32], bridge_to_client: [9; 32] };
        (Channel::hybrid(&keys(), Side::Client), Channel::hybrid(&keys(), Side::Bridge))
    }

    #[test]
    fn hybrid_frames_open_on_the_other_side_only_and_never_twice_or_out_of_order() {
        let (client, bridge) = hybrid_pair();
        let first = client.seal("one").unwrap();
        let second = client.seal("two").unwrap();
        assert_eq!(client.open(&first), None, "a side cannot open its own direction");
        assert_eq!(bridge.open(&second).as_deref(), Some("two"));
        assert_eq!(bridge.open(&first), None, "an older counter is refused after a newer one");
        assert_eq!(bridge.open(&second), None, "a replay is refused");
        let reply = bridge.seal("back").unwrap();
        assert_eq!(client.open(&reply).as_deref(), Some("back"));
        let mut bytes = STANDARD.decode(client.seal("three").unwrap()).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert_eq!(bridge.open(&STANDARD.encode(&bytes)), None, "a changed tag is refused");
        bytes[last] ^= 1;
        assert_eq!(bridge.open(&STANDARD.encode(&bytes)).as_deref(), Some("three"), "a refused frame takes no counter");
    }

    #[test]
    fn hybrid_frames_sealed_on_many_threads_are_delivered_in_counter_order() {
        let (client, bridge) = hybrid_pair();
        let client = std::sync::Arc::new(client);
        let delivered = std::sync::Arc::new(Mutex::new(Vec::new()));
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let (client, delivered) = (client.clone(), delivered.clone());
                std::thread::spawn(move || {
                    for i in 0..50 {
                        client.seal_then(format!("{t}:{i}").as_bytes(), |frame| delivered.lock().push(STANDARD.encode(frame))).unwrap();
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let frames = delivered.lock().clone();
        assert_eq!(frames.len(), 400);
        assert!(frames.iter().all(|f| bridge.open(f).is_some()), "every frame opens in the order it was handed on");
    }
}
