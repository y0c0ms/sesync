//! Caller tokens. Every route needs a token issued to one (member, machine) pair, valid for
//! a bounded time and signed with a server key. A token for one machine does not work on
//! another, and a token from a different issuer is refused.
//! Format: <member>|<machine>|<expiry>|<hex HMAC-SHA256 over those fields>.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

fn sign(key: &[u8; 32], body: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(body.as_bytes());
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn issue_token(key: &[u8; 32], member: &str, machine: &str, expires_at: u64) -> String {
    let body = format!("{member}|{machine}|{expires_at}");
    format!("{body}|{}", sign(key, &body))
}

/// The expiry a token carries, read without checking its signature. For the holder only, to
/// renew ahead of time; the server never trusts this value without `check_token`.
pub fn expiry_of(token: &str) -> Option<u64> {
    let parts: Vec<&str> = token.split('|').collect();
    if parts.len() != 4 {
        return None;
    }
    parts[2].parse().ok()
}

/// Check a token's signature and machine, ignoring its expiry. Used only for renewal: a token
/// that expired a few minutes ago still proves the machine holds it, and the renewal checks the
/// enrolment window separately. Never use this to grant access on its own.
pub fn check_token_ignoring_expiry(key: &[u8; 32], token: &str, machine: &str) -> Result<String, AuthError> {
    let parts: Vec<&str> = token.split('|').collect();
    if parts.len() != 4 {
        return Err(AuthError::Malformed);
    }
    let body = format!("{}|{}|{}", parts[0], parts[1], parts[2]);
    if sign(key, &body) != parts[3] {
        return Err(AuthError::BadSignature);
    }
    if parts[1] != machine {
        return Err(AuthError::WrongMachine);
    }
    Ok(parts[0].to_string())
}

/// Check a token presented by `machine` at `now`. Returns the member on success.
pub fn check_token(key: &[u8; 32], token: &str, machine: &str, now: u64) -> Result<String, AuthError> {
    let parts: Vec<&str> = token.split('|').collect();
    if parts.len() != 4 {
        return Err(AuthError::Malformed);
    }
    let body = format!("{}|{}|{}", parts[0], parts[1], parts[2]);
    if sign(key, &body) != parts[3] {
        return Err(AuthError::BadSignature);
    }
    if parts[1] != machine {
        return Err(AuthError::WrongMachine);
    }
    let expires: u64 = parts[2].parse().map_err(|_| AuthError::Malformed)?;
    if now >= expires {
        return Err(AuthError::Expired);
    }
    Ok(parts[0].to_string())
}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    Malformed,
    BadSignature,
    WrongMachine,
    Expired,
}
