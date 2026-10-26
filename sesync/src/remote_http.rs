//! The HTTP client that implements `RemoteSessions` against the team server. The server URL
//! and the caller token come from the environment, so no address or secret is in the code:
//!   HS_TEAM_URL    base URL of the team server, e.g. https://team.example:8443
//!   HS_TEAM_TOKEN  bearer token issued by the server at enrolment
//!   HS_TEAM_MACHINE the machine id the token was issued to

use crate::remote::{RemoteError, RemoteSessions, SpaceSession};
use serde_json::Value;
use std::time::Duration;

/// Waits between connection attempts, in milliseconds. Four attempts in total, about 3.5 s.
const RETRY_WAITS_MS: [u64; 4] = [200, 500, 1000, 1800];

pub struct HttpRemote {
    base: String,
    token: String,
    machine: String,
    agent: ureq::Agent,
}

impl HttpRemote {
    /// Build a client from the environment. Returns None if any setting is missing.
    pub fn from_env() -> Option<HttpRemote> {
        let base = std::env::var("HS_TEAM_URL").ok()?;
        // An unsafe URL yields no client, so nothing is ever sent with the token.
        crate::team_url::check_scheme(&base).ok()?;
        Some(HttpRemote::new(base, std::env::var("HS_TEAM_TOKEN").ok()?, std::env::var("HS_TEAM_MACHINE").ok()?))
    }

    pub fn new(base: String, token: String, machine: String) -> HttpRemote {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .build();
        HttpRemote { base: base.trim_end_matches('/').to_string(), token, machine, agent }
    }
}

impl RemoteSessions for HttpRemote {
    fn list(&self, space: &str) -> Result<Vec<SpaceSession>, RemoteError> {
        let url = format!("{}/sessions", self.base);
        let body = serde_json::json!({"space": space, "machine": self.machine});
        // Connection failures are retried with a short, growing wait: a Wi-Fi blip or a tunnel
        // wedge often clears in seconds. A refusal (e.g. 401) is final and never retried.
        let mut attempt = 0;
        let resp = loop {
            let r = self.agent.post(&url)
                .set("Authorization", &format!("Bearer {}", self.token))
                .send_json(body.clone());
            match r {
                Ok(r) => break r,
                Err(ureq::Error::Status(code, _)) => return Err(RemoteError::Refused(code)),
                Err(_) if attempt + 1 < RETRY_WAITS_MS.len() => {
                    std::thread::sleep(Duration::from_millis(RETRY_WAITS_MS[attempt]));
                    attempt += 1;
                }
                Err(_) => return Err(RemoteError::Unreachable),
            }
        };
        let json: Value = resp.into_json().map_err(|_| RemoteError::Unreachable)?;
        let ids = json["conversations"].as_array().ok_or(RemoteError::Unreachable)?;
        Ok(ids.iter()
            .filter_map(|v| v.as_str())
            .map(|id| SpaceSession { id: id.to_string(), title: id.to_string(), updated_at: 0 })
            .collect())
    }
}
