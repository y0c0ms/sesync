//! Signed records, format v2. A record is the author's device key, the member-list version it was
//! sealed under, the sequence it was sealed at, the space and conversation it belongs to, and the
//! sealed content. The author's signature covers every byte except the signature itself, so
//! moving a record to another space or conversation, or changing its sequence, breaks it.
//!
//! Layout (integers big-endian):
//!   "SR2" (3) || author (32, Ed25519 public key) || list_version (8) || sequence (8)
//!   || space_len (2) || space (utf8) || conv_len (2) || conversation (utf8)
//!   || sig (64) || content (rest)
//!
//! The sequence is the conversation's lease version, which only increases, so a reader that has
//! seen sequence N for a conversation refuses anything older. A reader accepts a record only when
//! it is bound to the expected space and conversation, the author is listed in the reader's
//! current member list, the list version is not older than that list, the signature verifies, and
//! the sequence is not below the reader's floor.
//!
//! v1 records (author || version || sig || content) are refused with `UnsupportedFormat`. There is
//! no migration: no v1 record was ever stored in production.

use crate::membership::{Device, Verifier};

const MAGIC: &[u8; 3] = b"SR2";
const AUTHOR_LEN: usize = 32;
const SIG_LEN: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub enum RecordError {
    Malformed,
    /// Not a v2 record (for example a v1 record). Refused, never converted.
    UnsupportedFormat,
    /// The record names a different space than the reader expects.
    WrongSpace,
    /// The record names a different conversation than the reader expects.
    WrongConversation,
    /// The author is not in the reader's current member list.
    UnknownAuthor,
    /// The record was sealed under a list older than the reader holds.
    StaleList { have: u64, got: u64 },
    /// The record's sequence is below the reader's floor for this conversation: an old record
    /// replayed to roll the session back.
    Rollback { have: u64, got: u64 },
    BadSignature,
}

/// What a reader expects a record to be: the space and conversation it was read from, and the
/// lowest sequence it will accept (the highest it has already seen, or 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub space: String,
    pub conversation: String,
    pub min_sequence: u64,
}

/// A record that passed every check: its content, and the sequence and list version it was sealed
/// under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub content: Vec<u8>,
    pub sequence: u64,
    pub list_version: u64,
}

/// Build a v2 signed record for `content`, written by `author` under list version `list_version`,
/// at `sequence`, for `space` and `conversation`.
pub fn seal_record(author: &Device, list_version: u64, sequence: u64, space: &str, conversation: &str,
                   content: &[u8]) -> Result<Vec<u8>, RecordError> {
    let space_len = u16::try_from(space.len()).map_err(|_| RecordError::Malformed)?;
    let conv_len = u16::try_from(conversation.len()).map_err(|_| RecordError::Malformed)?;
    let mut out = Vec::with_capacity(MAGIC.len() + AUTHOR_LEN + 16 + 4 + space.len() + conversation.len()
        + SIG_LEN + content.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&author.public_key());
    out.extend_from_slice(&list_version.to_be_bytes());
    out.extend_from_slice(&sequence.to_be_bytes());
    out.extend_from_slice(&space_len.to_be_bytes());
    out.extend_from_slice(space.as_bytes());
    out.extend_from_slice(&conv_len.to_be_bytes());
    out.extend_from_slice(conversation.as_bytes());
    let sig_at = out.len();
    out.extend_from_slice(&[0u8; SIG_LEN]);
    out.extend_from_slice(content);
    let sig = author.sign(&signed_part(&out, sig_at));
    out[sig_at..sig_at + SIG_LEN].copy_from_slice(&sig);
    Ok(out)
}

/// The sequence a v2 record claims, read without checking its signature. Only for numbering: the
/// server uses it to continue a conversation's lease versions after a restart. Never use it to
/// accept a record; `open_record` is the only check that does that.
pub fn sequence_of(rec: &[u8]) -> Option<u64> {
    if rec.len() < MAGIC.len() || &rec[..MAGIC.len()] != MAGIC {
        return None;
    }
    let mut cur = Cursor { bytes: rec, at: MAGIC.len() };
    cur.take(AUTHOR_LEN).ok()?;
    cur.u64().ok()?;
    cur.u64().ok()
}

/// The bytes the signature covers: everything before the signature field, then everything after it.
fn signed_part(rec: &[u8], sig_at: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(rec.len().saturating_sub(SIG_LEN));
    v.extend_from_slice(&rec[..sig_at]);
    v.extend_from_slice(&rec[sig_at + SIG_LEN..]);
    v
}

/// Bounds-checked reader over the record header. Every read either succeeds in full or is Malformed.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], RecordError> {
        let end = self.at.checked_add(n).ok_or(RecordError::Malformed)?;
        let s = self.bytes.get(self.at..end).ok_or(RecordError::Malformed)?;
        self.at = end;
        Ok(s)
    }

    fn u16(&mut self) -> Result<u16, RecordError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| RecordError::Malformed)?))
    }

    fn u64(&mut self) -> Result<u64, RecordError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| RecordError::Malformed)?))
    }

    /// A u16 length, then that many bytes of UTF-8 text.
    fn text(&mut self) -> Result<String, RecordError> {
        let n = self.u16()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| RecordError::Malformed)
    }
}

/// A record's fields, split at their offsets. Nothing in it has been checked yet.
struct Parsed<'a> {
    author: &'a [u8],
    list_version: u64,
    sequence: u64,
    space: String,
    conversation: String,
    sig_at: usize,
    sig: &'a [u8],
    content: &'a [u8],
}

fn parse(rec: &[u8]) -> Result<Parsed<'_>, RecordError> {
    if rec.len() < MAGIC.len() || &rec[..MAGIC.len()] != MAGIC {
        return Err(RecordError::UnsupportedFormat);
    }
    let mut cur = Cursor { bytes: rec, at: MAGIC.len() };
    let author = cur.take(AUTHOR_LEN)?;
    let list_version = cur.u64()?;
    let sequence = cur.u64()?;
    let space = cur.text()?;
    let conversation = cur.text()?;
    let sig_at = cur.at;
    let sig = cur.take(SIG_LEN)?;
    Ok(Parsed { author, list_version, sequence, space, conversation, sig_at, sig, content: &rec[cur.at..] })
}

/// Check a record against the reader's list and the expected binding, and return its content with
/// its sequence. Checks run in this order: format, parse, space, conversation, list version, author,
/// signature, sequence floor.
pub fn open_record(verifier: &Verifier, expect: &Binding, rec: &[u8]) -> Result<Opened, RecordError> {
    let p = parse(rec)?;
    if p.space != expect.space {
        return Err(RecordError::WrongSpace);
    }
    if p.conversation != expect.conversation {
        return Err(RecordError::WrongConversation);
    }

    let (have, listed) = verifier.current_view().ok_or(RecordError::UnknownAuthor)?;
    if p.list_version < have {
        return Err(RecordError::StaleList { have, got: p.list_version });
    }
    if !listed.iter().any(|e| e.len() >= AUTHOR_LEN && &e[..AUTHOR_LEN] == p.author) {
        return Err(RecordError::UnknownAuthor);
    }
    if !Device::verify(p.author, &signed_part(rec, p.sig_at), p.sig) {
        return Err(RecordError::BadSignature);
    }
    if p.sequence < expect.min_sequence {
        return Err(RecordError::Rollback { have: expect.min_sequence, got: p.sequence });
    }
    Ok(Opened { content: p.content.to_vec(), sequence: p.sequence, list_version: p.list_version })
}

/// Who sealed a record, under which list version and at which sequence, proven by the record's own
/// signature. For a writer asking whether the record a server stores is still its own, which needs
/// no member list: the record may be sealed under a list this writer no longer holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorship {
    pub author: Vec<u8>,
    pub list_version: u64,
    pub sequence: u64,
}

/// Check a record's binding, its signature against the author it names, and its sequence floor,
/// but not its list: a valid signature proves who wrote it, not that its author is still a member.
/// Never use this to accept a record's content; `open_record` is the only check that does that.
pub fn authorship(expect: &Binding, rec: &[u8]) -> Result<Authorship, RecordError> {
    let p = parse(rec)?;
    if p.space != expect.space {
        return Err(RecordError::WrongSpace);
    }
    if p.conversation != expect.conversation {
        return Err(RecordError::WrongConversation);
    }
    if !Device::verify(p.author, &signed_part(rec, p.sig_at), p.sig) {
        return Err(RecordError::BadSignature);
    }
    if p.sequence < expect.min_sequence {
        return Err(RecordError::Rollback { have: expect.min_sequence, got: p.sequence });
    }
    Ok(Authorship { author: p.author.to_vec(), list_version: p.list_version, sequence: p.sequence })
}
