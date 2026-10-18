//! HTTP surface for the team service. Each route is a thin call into `Service`, so every rule
//! is the one the service tests check. JSON in, JSON out. Errors map to status codes and
//! never include plaintext or key material.
//!
//! Every route except `POST /enrol` requires `Authorization: Bearer <token>`. The token must
//! have been issued to the same machine named in the request, and it must not be expired.

use crate::service::{Service, ServiceError, Store};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
pub struct EnrolBody { pub space: String, pub member: String, pub machine: String, pub code: String,
    /// The device entry (signing and box keys) hex-encoded. Required once the team has a trust
    /// root: the machine's later authority comes from this key being in the signed list.
    #[serde(default)]
    pub device: Option<String> }

#[derive(Deserialize)]
pub struct LeaseBody { pub space: String, pub conversation: String, pub machine: String }

/// The status code and JSON body for a service error. The message names the rule, not the
/// content.
pub fn error_response(e: &ServiceError) -> (u16, Value) {
    match e {
        ServiceError::Unavailable => (503, json!({"error": "service unavailable, retry later"})),
        ServiceError::Conflict(m) => (409, json!({"error": m})),
        ServiceError::NoSpace(_) => (404, json!({"error": "no such space"})),
        ServiceError::NotMember(_) => (403, json!({"error": "not a member"})),
        ServiceError::Auth(_) => (401, json!({"error": "authentication refused"})),
        ServiceError::Lease(m) if m.contains("Held") => (409, json!({"error": "conversation is in use"})),
        ServiceError::Lease(m) if m.contains("Stale") => (409, json!({"error": "stale write refused"})),
        ServiceError::Lease(_) => (409, json!({"error": "no live lease for this write"})),
        ServiceError::Crypto(_) => (400, json!({"error": "record refused"})),
        ServiceError::Policy(m) => (400, json!({"error": "invalid backup policy", "detail": m})),
    }
}

/// Check the bearer token for a request from `machine` in `space`. Returns the member on
/// success, or a 401 response.
pub fn require_token<S: Store>(svc: &Service<S>, space: &str, auth_header: Option<&str>,
                               machine: &str, now: u64) -> Result<String, (u16, Value)>
{
    let token = auth_header
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or((401, json!({"error": "missing bearer token"})))?;
    svc.authenticate(space, token, machine, now).map_err(|e| error_response(&e))
}

/// List the conversation ids in a space, and the sequence each stored signed record carries. The
/// caller must hold a token for its machine and must be a member of the space. `conversations` is
/// every id, as it always was; `sequences` maps an id to its record's sequence and leaves out any
/// conversation whose stored bytes carry none. Clients use `sequences` only to skip a fetch.
pub fn list_sessions<S: Store>(svc: &Service<S>, auth: Option<&str>, space: &str, machine: &str,
                               now: u64) -> (u16, Value)
{
    let member = match require_token(svc, space, auth, machine, now) {
        Ok(m) => m,
        Err(r) => return r,
    };
    match svc.list_with_sequences(space, &member) {
        Ok((ids, sequences)) => (200, json!({"space": space, "conversations": ids, "sequences": sequences})),
        Err(e) => error_response(&e),
    }
}

/// Fetch the current admin-signed member list. Any current member may read it: it names the
/// devices allowed in the team, and the fetching client re-verifies the admin signature
/// against the key pinned at join, so the server cannot lie about membership.
pub fn fetch_trust_list<S: Store>(svc: &Service<S>, auth: Option<&str>, space: &str, machine: &str,
                                  now: u64) -> (u16, Value)
{
    if let Err(r) = require_token(svc, space, auth, machine, now) {
        return r;
    }
    match svc.trust_list(space) {
        Some(bytes) => {
            let version = crate::membership::SignedList::from_bytes(&bytes)
                .map(|l| l.list.version)
                .unwrap_or(0);
            (200, json!({"space": space, "version": version, "signed_list": hex(&bytes)}))
        }
        None => (404, json!({"error": "no trust root for this space"})),
    }
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

/// Health: 200 when the store answers, 503 when it does not. No auth, no data: it only says
/// whether the service can do its job, so a monitor can catch a wedge.
pub fn health<S: Store>(svc: &Service<S>) -> (u16, Value) {
    if svc.healthy() {
        (200, json!({"status": "ok"}))
    } else {
        (503, json!({"status": "database unreachable"}))
    }
}

/// Renew a member token. The old token is sent in the Authorization header; the new one is
/// returned. Refused once the 7-day enrolment window has passed.
pub fn renew<S: Store>(svc: &mut Service<S>, auth: Option<&str>, space: &str, machine: &str, now: u64) -> (u16, Value) {
    let token = match auth.and_then(|h| h.strip_prefix("Bearer ")) {
        Some(t) => t,
        None => return (401, json!({"error": "missing bearer token"})),
    };
    match svc.renew(space, token, machine, now) {
        Ok(new) => (200, json!({"token": new, "expires_in": crate::service::TOKEN_LIFETIME_SECS})),
        Err(e) => error_response(&e),
    }
}

/// Enrol a machine. Confirms the authenticator code, then returns a token bound to the machine,
/// together with the trust root (admin key and signed member list) when the team has one, so the
/// joining device can pin the admin key and never accept lists the admin did not sign.
pub fn enrol<S: Store>(svc: &mut Service<S>, body: &EnrolBody, now: u64) -> (u16, Value) {
    let device = match body.device.as_deref() {
        Some(d) => match from_hex(d) {
            Some(bytes) => Some(bytes),
            None => return (400, json!({"error": "the device entry is not hex"})),
        },
        None => None,
    };
    if device.is_none() && svc.trust_list(&body.space).is_some() {
        return (400, json!({"error": "this team needs the device key; upgrade the client"}));
    }
    match svc.enrol(&body.space, &body.member, &body.machine, &body.code, now) {
        Ok(token) => {
            if let Some(entry) = device {
                svc.bind_device(&body.space, &body.machine, entry);
            }
            let mut reply = json!({"token": token, "expires_in": crate::service::TOKEN_LIFETIME_SECS});
            if let Some(list) = svc.trust_list(&body.space) {
                if let Some(admin) = svc.load_admin_key_for_test(&body.space) {
                    let version = crate::membership::SignedList::from_bytes(&list)
                        .map(|l| l.list.version).unwrap_or(0);
                    reply["admin_public"] = json!(hex(&admin));
                    reply["signed_list"] = json!(hex(&list));
                    reply["list_version"] = json!(version);
                }
            }
            (200, reply)
        }
        Err(e) => error_response(&e),
    }
}

/// The enrolled devices for a space, with the current list version, for the admin to build the
/// next list. Admin only: the entries are public keys, but a member has no business enumerating
/// the team's machines.
pub fn list_devices<S: Store>(svc: &Service<S>, space: &str) -> (u16, Value) {
    let version = svc.trust_list(space)
        .and_then(|l| crate::membership::SignedList::from_bytes(&l).ok())
        .map(|l| l.list.version).unwrap_or(0);
    let devices: Vec<Value> = svc.devices(space).iter()
        .map(|(m, entry)| json!({"machine": m, "entry": hex(entry)}))
        .collect();
    (200, json!({"space": space, "version": version, "devices": devices}))
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 { return None; }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Acquire a conversation lease. The caller must hold a token for its machine.
pub fn acquire<S: Store>(svc: &mut Service<S>, auth: Option<&str>, body: &LeaseBody, now: u64) -> (u16, Value) {
    if let Err(r) = require_token(svc, &body.space, auth, &body.machine, now) {
        return r;
    }
    match svc.acquire(&body.space, &body.conversation, &body.machine, now) {
        Ok(lease) => (200, json!({"version": lease.version})),
        Err(e) => error_response(&e),
    }
}

/// Release a conversation lease. The caller must hold a token for its machine.
pub fn release<S: Store>(svc: &mut Service<S>, auth: Option<&str>, body: &LeaseBody, now: u64) -> (u16, Value) {
    if let Err(r) = require_token(svc, &body.space, auth, &body.machine, now) {
        return r;
    }
    match svc.release(&body.space, &body.conversation, &body.machine) {
        Ok(()) => (200, json!({"released": true})),
        Err(e) => error_response(&e),
    }
}

/// Store a session under the caller's live lease. The caller must hold a token for its machine.
pub fn write_record<S: Store>(svc: &mut Service<S>, auth: Option<&str>, space: &str, conv: &str,
                              machine: &str, version: u64, plain: Vec<u8>, now: u64) -> (u16, Value)
{
    if let Err(r) = require_token(svc, space, auth, machine, now) {
        return r;
    }
    match svc.put_record(space, conv, machine, version, plain, now) {
        Ok(()) => (200, json!({"stored": true})),
        Err(e) => error_response(&e),
    }
}

/// Read a session. Returns the plaintext on success, or (status, error body).
pub fn read_record<S: Store>(svc: &Service<S>, auth: Option<&str>, space: &str, conv: &str,
                             machine: &str, now: u64) -> (u16, Result<Vec<u8>, Value>)
{
    let member = match require_token(svc, space, auth, machine, now) {
        Ok(m) => m,
        Err((status, body)) => return (status, Err(body)),
    };
    match svc.get_record(space, conv, &member) {
        Ok(plain) => (200, Ok(plain)),
        Err(e) => {
            let (status, body) = error_response(&e);
            (status, Err(body))
        }
    }
}
