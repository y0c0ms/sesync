//! The client's seal and open for team records. It uses the same record format and crypto as the
//! team server (one implementation, shared by path), so a record made here verifies there and back.
//!
//! Seal: the session is compressed (zstd, or stored raw when that does not make it smaller), tagged
//! with the codec it used, then encrypted under a fresh data key, bound to its space, conversation
//! and sequence; that key is wrapped to each device in the team's admin-signed member list; the
//! result is signed by the writer, and the signature covers the space, conversation and sequence.
//! Open: the record must be bound to the space and conversation the reader expects, be signed by an
//! author that same verified list names, be at or above the reader's sequence floor, and the wrap
//! addressed to this device must open. Nothing is returned as text unless all of that holds.
//!
//! The trust root is explicit: a `Trust` holds the admin's public key and the signed list as the
//! admin sent it. Every use re-verifies the list against the admin key, so a forged or rolled-back
//! list can never widen the authors this device accepts.

use sesync_team::membership::Device;
use sesync_team::membership::{SignedList, Verifier};
use sesync_team::session_keys::{
    open_session_bound, seal_session_bound, session_aad, unwrap_for, wrap_for, DataKey, Wrap,
};
use sesync_team::signed_records::{open_record, seal_record, Binding, Opened, RecordError};

use crate::device_keys::DeviceFile;

#[derive(Debug, PartialEq, Eq)]
pub enum SealError {
    UnknownAuthor,
    BadSignature,
    NotForThisDevice,
    Malformed,
    /// The record is not in the current format (for example a v1 record). Refused, not converted.
    UnsupportedFormat,
    /// The record was sealed for a different space than the reader expects.
    WrongSpace,
    /// The record was sealed for a different conversation than the reader expects.
    WrongConversation,
    /// The record was sealed under a member list older than the reader holds.
    StaleList { have: u64, got: u64 },
    /// The record's sequence is below the reader's floor: an old record replayed.
    Rollback { have: u64, got: u64 },
    /// The session is larger than `MAX_SESSION_BYTES` once decompressed. Refused on seal and on open,
    /// so a crafted record cannot make the reader allocate without bound.
    TooLarge,
    Crypto(String),
}

/// Largest session, in bytes, before compression and after decompression.
pub const MAX_SESSION_BYTES: usize = 64 * 1024 * 1024;

/// The first byte of the body plaintext names how the session bytes that follow are encoded.
const CODEC_STORED: u8 = 0x00;
const CODEC_ZSTD: u8 = 0x01;
/// zstd level for sealing. The window is bounded on open, so the decoder's buffer is too.
const ZSTD_LEVEL: i32 = 3;
const ZSTD_WINDOW_LOG_MAX: u32 = 24;
/// Decompression works in chunks this size, so the output never grows past the limit.
const INFLATE_CHUNK: usize = 64 * 1024;

impl From<RecordError> for SealError {
    fn from(e: RecordError) -> SealError {
        match e {
            RecordError::Malformed => SealError::Malformed,
            RecordError::UnsupportedFormat => SealError::UnsupportedFormat,
            RecordError::WrongSpace => SealError::WrongSpace,
            RecordError::WrongConversation => SealError::WrongConversation,
            RecordError::UnknownAuthor => SealError::UnknownAuthor,
            RecordError::StaleList { have, got } => SealError::StaleList { have, got },
            RecordError::Rollback { have, got } => SealError::Rollback { have, got },
            RecordError::BadSignature => SealError::BadSignature,
        }
    }
}

/// The team's verified trust root: the admin's public key (pinned at join) and the admin-signed
/// member list. Nothing else can widen who a device accepts or wraps to.
#[derive(Clone, Debug, PartialEq)]
pub struct Trust {
    admin_public: Vec<u8>,
    signed_list: Vec<u8>,
}

impl Trust {
    pub fn new(admin_public: Vec<u8>, signed_list: Vec<u8>) -> Trust {
        Trust { admin_public, signed_list }
    }

    pub fn admin_public(&self) -> &[u8] { &self.admin_public }
    pub fn signed_list(&self) -> &[u8] { &self.signed_list }

    /// Parse the list and verify it against the pinned admin key. Result: a verifier that knows
    /// the listed devices. Called on every use; a list that does not verify is never used.
    pub fn verifier(&self) -> Result<Verifier, SealError> {
        let signed = SignedList::from_bytes(&self.signed_list).map_err(|_| SealError::Malformed)?;
        let mut v = Verifier::new(self.admin_public.clone());
        v.accept(&signed).map_err(|_| SealError::BadSignature)?;
        Ok(v)
    }

    /// The list version records must be sealed under.
    pub fn version(&self) -> Result<u64, SealError> {
        Ok(SignedList::from_bytes(&self.signed_list).map_err(|_| SealError::Malformed)?.list.version)
    }

    /// The X25519 box keys of the listed devices: who a new session key may be wrapped to.
    pub fn box_keys(&self) -> Result<Vec<Vec<u8>>, SealError> {
        Ok(self.verifier()?.box_keys())
    }
}

/// The team's device view for the device's own use: the same type the server uses.
fn as_device(d: &DeviceFile) -> Result<Device, SealError> {
    Device::from_keys(d.signing_pkcs8(), *d.box_secret()).map_err(|_| SealError::Malformed)
}

/// Seal `plain` for the devices in the team's verified list and sign it as `writer`. The record is
/// for `space` and `conversation` at `sequence` (the conversation's lease version), and all three
/// are bound into both the body's encryption and the signature. The recipients come from the
/// verified list alone: revocation narrows them.
pub fn seal_for_devices(writer: &DeviceFile, trust: &Trust, space: &str, conversation: &str,
                        sequence: u64, plain: &[u8]) -> Result<Vec<u8>, SealError> {
    let key = DataKey::random();
    let aad = session_aad(space, conversation, sequence);
    let framed = encode_body(plain)?;
    let body = seal_session_bound(&key, &aad, &framed).map_err(|e| SealError::Crypto(format!("{e:?}")))?;
    let boxes = trust.box_keys()?;
    let wraps = wrap_for(&key, &boxes).map_err(|e| SealError::Crypto(format!("{e:?}")))?;
    let payload = encode_payload(&wraps, &body);
    let author = as_device(writer)?;
    let version = trust.version()?;
    seal_record(&author, version, sequence, space, conversation, &payload).map_err(SealError::from)
}

/// Open a record for `reader` against the team's verified list. The record must be bound to
/// `expect` (its space, conversation and sequence floor), signed by a listed author, and wrapped to
/// this device. Returns the session text with the record's sequence.
pub fn open_for_device(reader: &DeviceFile, trust: &Trust, expect: &Binding, record: &[u8])
    -> Result<Opened, SealError>
{
    let verifier = trust.verifier()?;
    let opened = open_record(&verifier, expect, record)?;
    let (wraps, body) = decode_payload(&opened.content).ok_or(SealError::Malformed)?;
    let me = as_device(reader)?;
    let key = unwrap_for(&me, &wraps).map_err(|_| SealError::NotForThisDevice)?;
    // open_record has checked the record's space and conversation against `expect`, so the body's
    // associated data is built from the same values the writer used, with the record's sequence.
    let aad = session_aad(&expect.space, &expect.conversation, opened.sequence);
    let framed = open_session_bound(&key, &aad, &body).map_err(|e| SealError::Crypto(format!("{e:?}")))?;
    let text = decode_body(&framed)?;
    Ok(Opened { content: text, sequence: opened.sequence, list_version: opened.list_version })
}

/// Frame the session for the body AEAD: one codec byte, then the bytes. zstd is used when it makes
/// the session smaller; otherwise the bytes are stored as they are.
fn encode_body(plain: &[u8]) -> Result<Vec<u8>, SealError> {
    if plain.len() > MAX_SESSION_BYTES {
        return Err(SealError::TooLarge);
    }
    let packed = zstd::stream::encode_all(plain, ZSTD_LEVEL)
        .map_err(|e| SealError::Crypto(format!("zstd encode: {e}")))?;
    let (codec, data): (u8, &[u8]) = if packed.len() < plain.len() {
        (CODEC_ZSTD, &packed)
    } else {
        (CODEC_STORED, plain)
    };
    let mut out = Vec::with_capacity(1 + data.len());
    out.push(codec);
    out.extend_from_slice(data);
    Ok(out)
}

/// Undo `encode_body`. An unknown codec tag is malformed. A zstd body is inflated with a hard
/// limit of `MAX_SESSION_BYTES` on the output and a bounded window, so a crafted record cannot
/// make the reader allocate without limit.
fn decode_body(framed: &[u8]) -> Result<Vec<u8>, SealError> {
    let (&codec, data) = framed.split_first().ok_or(SealError::Malformed)?;
    match codec {
        CODEC_STORED => {
            if data.len() > MAX_SESSION_BYTES {
                return Err(SealError::TooLarge);
            }
            Ok(data.to_vec())
        }
        CODEC_ZSTD => inflate_bounded(data),
        _ => Err(SealError::Malformed),
    }
}

fn inflate_bounded(data: &[u8]) -> Result<Vec<u8>, SealError> {
    use std::io::Read;
    let mut d = zstd::stream::read::Decoder::new(data).map_err(|_| SealError::Malformed)?;
    d.window_log_max(ZSTD_WINDOW_LOG_MAX).map_err(|_| SealError::Malformed)?;
    let mut out = Vec::new();
    let mut chunk = vec![0u8; INFLATE_CHUNK];
    loop {
        let n = match d.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(SealError::Malformed),
        };
        if out.len() + n > MAX_SESSION_BYTES {
            return Err(SealError::TooLarge);
        }
        out.extend_from_slice(&chunk[..n]);
    }
    Ok(out)
}

fn encode_payload(wraps: &[Wrap], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(wraps.len() as u32).to_be_bytes());
    for w in wraps {
        out.extend_from_slice(&(w.device.len() as u32).to_be_bytes());
        out.extend_from_slice(&w.device);
        out.extend_from_slice(&(w.sealed.len() as u32).to_be_bytes());
        out.extend_from_slice(&w.sealed);
    }
    out.extend_from_slice(body);
    out
}

fn decode_payload(b: &[u8]) -> Option<(Vec<Wrap>, Vec<u8>)> {
    let mut i = 0usize;
    let rd = |i: &mut usize| -> Option<usize> {
        let n = u32::from_be_bytes(b.get(*i..*i + 4)?.try_into().ok()?) as usize;
        *i += 4;
        Some(n)
    };
    let count = rd(&mut i)?;
    let mut wraps = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let dl = rd(&mut i)?;
        let device = b.get(i..i + dl)?.to_vec(); i += dl;
        let sl = rd(&mut i)?;
        let sealed = b.get(i..i + sl)?.to_vec(); i += sl;
        wraps.push(Wrap { device, sealed });
    }
    Some((wraps, b.get(i..)?.to_vec()))
}
