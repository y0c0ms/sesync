//! Authenticator join: RFC 6238 TOTP, 6 digits, 30-second steps, HMAC-SHA1.
//!
//! The check proves the joining user holds the authenticator. It is not the encryption key:
//! a 6-digit code has only a million values, so it cannot carry or derive a key. A code is
//! accepted once per machine, and five wrong codes lock joining for 15 minutes: on that machine,
//! and for that member whatever machine name the next attempt uses.

use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::collections::HashSet;

pub const STEP_SECS: u64 = 30;
pub const MAX_FAILURES: u32 = 5;
pub const LOCK_SECS: u64 = 15 * 60;

#[derive(Debug, PartialEq, Eq)]
pub enum JoinError {
    Wrong,
    AlreadyUsed,
    Locked { until: u64 },
}

/// The 6-digit TOTP code for `secret` at `time` (seconds since the epoch).
pub fn code_at(secret: &[u8], time: u64) -> String {
    let counter = (time / STEP_SECS).to_be_bytes();
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(&counter);
    let digest = mac.finalize().into_bytes();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let bin = ((digest[offset] as u32 & 0x7f) << 24)
        | ((digest[offset + 1] as u32) << 16)
        | ((digest[offset + 2] as u32) << 8)
        | (digest[offset + 3] as u32);
    format!("{:06}", bin % 1_000_000)
}

impl JoinGate {
    /// A copy of the gate's state, for loading from a store.
    pub fn clone_state(&self) -> JoinGate {
        JoinGate {
            secret: self.secret.clone(),
            used: self.used.clone(),
            failures: self.failures,
            locked_until: self.locked_until,
        }
    }
}

impl JoinGate {
    /// Serialize the gate state, sealed under the master key. The secret is included, so it
    /// is only ever stored wrapped.
    pub fn to_bytes(&self, master: &[u8; 32]) -> Vec<u8> {
        let mut plain = Vec::new();
        plain.extend_from_slice(&(self.secret.len() as u32).to_le_bytes());
        plain.extend_from_slice(&self.secret);
        plain.extend_from_slice(&self.failures.to_le_bytes());
        plain.extend_from_slice(&self.locked_until.to_le_bytes());
        let mut used: Vec<_> = self.used.iter().collect();
        used.sort();
        plain.extend_from_slice(&(used.len() as u32).to_le_bytes());
        for (step, code) in used {
            plain.extend_from_slice(&step.to_le_bytes());
            plain.extend_from_slice(&(code.len() as u32).to_le_bytes());
            plain.extend_from_slice(code.as_bytes());
        }
        crate::spaces::seal(master, b"join gate v1", &plain).unwrap_or_default()
    }

    /// Restore a gate from its sealed form. None if the master key is wrong or the bytes are damaged.
    pub fn from_bytes(sealed: &[u8], master: &[u8; 32]) -> Option<JoinGate> {
        let plain = crate::spaces::open_bound(master, b"join gate v1", sealed).ok()?;
        let mut c = 0usize;
        let take = |c: &mut usize, n: usize| -> Option<&[u8]> {
            let s = plain.get(*c..*c + n)?;
            *c += n;
            Some(s)
        };
        let n = u32::from_le_bytes(take(&mut c, 4)?.try_into().ok()?) as usize;
        let secret = take(&mut c, n)?.to_vec();
        let failures = u32::from_le_bytes(take(&mut c, 4)?.try_into().ok()?);
        let locked_until = u64::from_le_bytes(take(&mut c, 8)?.try_into().ok()?);
        let n_used = u32::from_le_bytes(take(&mut c, 4)?.try_into().ok()?) as usize;
        let mut used = std::collections::HashSet::new();
        for _ in 0..n_used {
            let step = u64::from_le_bytes(take(&mut c, 8)?.try_into().ok()?);
            let len = u32::from_le_bytes(take(&mut c, 4)?.try_into().ok()?) as usize;
            let code = String::from_utf8(take(&mut c, len)?.to_vec()).ok()?;
            used.insert((step, code));
        }
        Some(JoinGate { secret, used, failures, locked_until })
    }
}

/// Gate for one user's join attempts: single use per code, and a lock after repeated failures.
pub struct JoinGate {
    secret: Vec<u8>,
    used: HashSet<(u64, String)>,
    failures: u32,
    locked_until: u64,
}

impl JoinGate {
    pub fn new(secret: Vec<u8>) -> JoinGate {
        JoinGate { secret, used: HashSet::new(), failures: 0, locked_until: 0 }
    }

    /// Check a code presented at `now`. Accepts the current step (and the previous one, to
    /// allow for clock drift), refuses a code already used, and locks after MAX_FAILURES.
    pub fn verify(&mut self, code: &str, now: u64) -> Result<(), JoinError> {
        if now < self.locked_until {
            return Err(JoinError::Locked { until: self.locked_until });
        }
        let step = now / STEP_SECS;
        for s in [step, step.saturating_sub(1)] {
            let t = s * STEP_SECS;
            if code_at(&self.secret, t) == code {
                if self.used.contains(&(s, code.to_string())) {
                    return Err(JoinError::AlreadyUsed);
                }
                self.used.insert((s, code.to_string()));
                self.failures = 0;
                return Ok(());
            }
        }
        self.failures += 1;
        if self.failures >= MAX_FAILURES {
            self.locked_until = now + LOCK_SECS;
            self.failures = 0;
        }
        Err(JoinError::Wrong)
    }
}

/// Wrong codes counted per member, across every machine name. The gate above is per machine, and
/// the joining client names the machine, so without this a guesser could take five fresh guesses
/// under each new machine name. Five wrong codes for one member lock that member's joining for
/// LOCK_SECS, whatever machine the next attempt names; other members are not affected.
#[derive(Debug, Default, Clone)]
pub struct MemberAttempts {
    failures: u32,
    locked_until: u64,
}

impl MemberAttempts {
    /// Refused while the member is locked.
    pub fn check(&self, now: u64) -> Result<(), JoinError> {
        if now < self.locked_until { Err(JoinError::Locked { until: self.locked_until }) } else { Ok(()) }
    }

    /// Count one wrong code at `now`; the MAX_FAILURES-th locks the member for LOCK_SECS.
    pub fn wrong(&mut self, now: u64) {
        self.failures += 1;
        if self.failures >= MAX_FAILURES {
            self.locked_until = now + LOCK_SECS;
            self.failures = 0;
        }
    }
}

/// RFC 4648 base32 (no padding), the encoding authenticator apps expect for a TOTP secret.
pub fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in bytes {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// Percent-encode everything but RFC 3986 unreserved characters, so a team name with spaces,
/// `?`, `#`, `&`, `/`, `:` or non-ASCII letters stays one label an authenticator app can parse.
fn uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The otpauth URI an authenticator app imports: a 30-second, 6-digit, SHA-1 TOTP for the
/// space. Shown once at creation; the label carries the space name, never the secret. The name is
/// percent-encoded: the issuer prefix `sesync:` is the only raw colon in the label.
pub fn otpauth_uri(secret: &[u8], space: &str) -> String {
    format!("otpauth://totp/sesync:{}?secret={}&issuer=sesync&algorithm=SHA1&digits=6&period=30",
            uri_component(space), base32(secret))
}
