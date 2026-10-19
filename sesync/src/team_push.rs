//! The push side of team sync: which local sessions may be sent to the team.
//!
//! The rule is fixed by the operator: only sessions the user marked are eligible. This module
//! decides the selection only; sealing and sending are separate, and both need a held lease.

use crate::state::State;

/// The sessions that may be pushed: those that exist locally and are marked. Order follows the
/// input, so the result is stable.
pub fn select_for_push(state: &State, local_sessions: &[String]) -> Vec<String> {
    local_sessions.iter().filter(|s| state.is_marked(s)).cloned().collect()
}

/// What a push needs from the team server. The HTTP client implements this; tests use a fake.
pub trait TeamRemote {
    /// Take the conversation's lease. Returns the version to write under.
    fn acquire(&self, space: &str, conversation: &str) -> Result<u64, PushError>;
    /// Write the sealed bytes under the granted version.
    fn put(&self, space: &str, conversation: &str, version: u64, bytes: &[u8]) -> Result<(), PushError>;
    /// Give the lease back. Called even when the write failed.
    fn release(&self, space: &str, conversation: &str) -> Result<(), PushError>;
}

#[derive(Debug, PartialEq)]
pub enum PushError {
    /// Another machine holds the conversation. Try again later; nothing was written.
    Held,
    /// The server could not be reached.
    Unreachable,
    /// The server refused for another reason.
    Refused(String),
}

/// Holds a lease and releases it when dropped, so a failed write can never leave the
/// conversation locked for other machines.
struct Lease<'a, R: TeamRemote> {
    remote: &'a R,
    space: String,
    conversation: String,
}

impl<R: TeamRemote> Drop for Lease<'_, R> {
    fn drop(&mut self) {
        let _ = self.remote.release(&self.space, &self.conversation);
    }
}

/// Push one marked session: take the lease, seal under the version it granted, write, release.
/// `seal` receives that version, which is the record's sequence, so the sealed bytes are bound to
/// the lease they were written under. Refuses to push a session that is not marked, so the rule
/// holds even if a caller forgets to check. A failed seal writes nothing and still releases.
pub fn push_marked_sealed<R: TeamRemote>(state: &State, remote: &R, space: &str, session_id: &str,
                                         seal: impl FnOnce(u64) -> Result<Vec<u8>, String>)
    -> Result<(), PushError>
{
    if !state.is_marked(session_id) {
        return Err(PushError::Refused("session is not marked for the team".into()));
    }
    let version = remote.acquire(space, session_id)?;
    let _guard = Lease { remote, space: space.to_string(), conversation: session_id.to_string() };
    let bytes = seal(version).map_err(PushError::Refused)?;
    remote.put(space, session_id, version, &bytes)
}

/// Push already-sealed bytes for a marked session under a lease. For callers whose bytes do not
/// depend on the lease version; the sealed path uses `push_marked_sealed`.
pub fn push_marked<R: TeamRemote>(state: &State, remote: &R, space: &str, session_id: &str, bytes: &[u8])
    -> Result<(), PushError>
{
    push_marked_sealed(state, remote, space, session_id, |_| Ok(bytes.to_vec()))
}

/// The team server over HTTP. Settings come from the caller: base URL, the member token, and
/// the machine id the token was issued to. The token is sent only in the Authorization header.
pub struct HttpTeam {
    pub base: String,
    pub token: String,
    pub machine: String,
}

impl HttpTeam {
    fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, PushError> {
        let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(20)).build();
        let url = format!("{}{path}", self.base.trim_end_matches('/'));
        match agent.post(&url).set("Authorization", &format!("Bearer {}", self.token)).send_json(body) {
            Ok(r) => r.into_json::<serde_json::Value>().map_err(|_| PushError::Refused("bad reply".into())),
            Err(ureq::Error::Status(409, _)) => Err(PushError::Held),
            Err(ureq::Error::Status(code, _)) => Err(PushError::Refused(format!("server refused ({code})"))),
            Err(_) => Err(PushError::Unreachable),
        }
    }
}

impl TeamRemote for HttpTeam {
    fn acquire(&self, space: &str, conversation: &str) -> Result<u64, PushError> {
        let reply = self.post("/lease/acquire", serde_json::json!({
            "space": space, "conversation": conversation, "machine": self.machine}))?;
        reply["version"].as_u64().ok_or_else(|| PushError::Refused("no lease version".into()))
    }

    fn put(&self, space: &str, conversation: &str, version: u64, bytes: &[u8]) -> Result<(), PushError> {
        // The sealed route is the only way new writes go out: the server never sees plaintext.
        self.put_to("/record/sealed", space, conversation, version, bytes)
    }

    fn release(&self, space: &str, conversation: &str) -> Result<(), PushError> {
        self.post("/lease/release", serde_json::json!({
            "space": space, "conversation": conversation, "machine": self.machine}))?;
        Ok(())
    }
}

impl HttpTeam {
    /// PUT-style write under a lease to `route`. New writes go to `/record/sealed` — a
    /// client-sealed, signed record the server stores without reading. `/record` is the legacy
    /// plaintext route, kept only until the sealed path is the only writer.
    pub fn put_to(&self, route: &str, space: &str, conversation: &str, version: u64, bytes: &[u8]) -> Result<(), PushError> {
        let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(60)).build();
        let url = format!("{}{route}?space={space}&conversation={conversation}&machine={}&version={version}",
                          self.base.trim_end_matches('/'), self.machine);
        match agent.post(&url).set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/octet-stream").send_bytes(bytes) {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(409, _)) => Err(PushError::Refused("stale lease".into())),
            Err(ureq::Error::Status(code, _)) => Err(PushError::Refused(format!("server refused ({code})"))),
            Err(_) => Err(PushError::Unreachable),
        }
    }
}
