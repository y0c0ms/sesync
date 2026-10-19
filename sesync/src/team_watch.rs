//! The team side of `--watch`: when a team is configured, each pass also pulls and verifies the
//! team's sealed records for display. It is off unless the environment names a team server and a
//! device, so an install without a team behaves exactly as before.

use std::collections::BTreeMap;

use crate::device_keys::{load_or_create, DeviceFile};
use crate::sealed::Trust;
use crate::team_pull_verified::{pull_verified, pull_verified_all, PullOutcome, RecordSource};

/// Settings for the team side. Built from the environment; `None` means no team is configured.
pub struct TeamWatch {
    pub base: String,
    pub token: String,
    pub machine: String,
    pub space: String,
    pub device: DeviceFile,
    /// The trust root pinned at join: the admin key and the signed list as it stood then.
    pub trust: Option<Trust>,
}

impl TeamWatch {
    /// Configured when the server address is set and the rest comes from either the
    /// environment or the single credential the machine joined with. Missing any of them,
    /// the team side stays off.
    pub fn from_env(config_dir: &std::path::Path) -> Option<TeamWatch> {
        let base = std::env::var("SESYNC_TEAM_URL").ok()?;
        let token = std::env::var("SESYNC_TEAM_TOKEN").ok();
        let machine = std::env::var("SESYNC_TEAM_MACHINE").ok();
        let space = std::env::var("SESYNC_TEAM_SPACE").ok();
        let (token, machine, space, trust) = fill_from_credential(config_dir, token, machine, space)?;
        let device = load_or_create(config_dir).ok()?;
        Some(TeamWatch { base, token, machine, space, device, trust })
    }

    /// One verified pull, incremental: a conversation the server advertises at or below the floor
    /// already held here is not fetched again. Writes nothing; returns what to display. The member
    /// list is refreshed from the server first: a list that verifies against the pinned admin key
    /// and is not older than the one held is used, anything else falls back to the list held, so a
    /// hostile or stale server can never roll membership backwards or widen it. `last_seen` is the
    /// highest sequence already accepted per conversation; older records are refused as rollbacks.
    pub fn pull(&self, last_seen: &BTreeMap<String, u64>) -> PullOutcome {
        self.pull_with(last_seen, pull_verified)
    }

    /// As `pull`, but every conversation is fetched and verified, so the result is the complete
    /// set of what this device may show. For a caller that replaces its whole list with the result.
    pub fn pull_all(&self, last_seen: &BTreeMap<String, u64>) -> PullOutcome {
        self.pull_with(last_seen, pull_verified_all)
    }

    fn pull_with(&self, last_seen: &BTreeMap<String, u64>,
                 pull: fn(&dyn RecordSource, &DeviceFile, Option<&Trust>, &BTreeMap<String, u64>) -> PullOutcome)
        -> PullOutcome
    {
        let src = HttpRecords { base: self.base.clone(), token: self.token.clone(),
                                machine: self.machine.clone(), space: self.space.clone() };
        let fresh = self.refreshed_trust(&src);
        let active = fresh.as_ref().or(self.trust.as_ref());
        pull(&src, &self.device, active, last_seen)
    }

    /// Fetch the current list and accept it only if it verifies against the pinned admin key and
    /// its version is at least the held one.
    fn refreshed_trust(&self, src: &HttpRecords) -> Option<Trust> {
        let pinned = self.trust.as_ref()?;
        let (signed_list, version) = src.fetch_list()?;
        fresh_if_newer(pinned, signed_list, version)
    }
}

/// The client's own list refresh, also used by callers about to seal: fetch the current list and
/// accept it only if it verifies against `pinned` and is not older. `None` falls back to what is
/// already held.
pub fn fetch_and_verify(base: &str, token: &str, machine: &str, space: &str, pinned: &Trust) -> Option<Trust> {
    let src = HttpRecords { base: base.to_string(), token: token.to_string(),
                            machine: machine.to_string(), space: space.to_string() };
    let (signed_list, version) = src.fetch_list()?;
    fresh_if_newer(pinned, signed_list, version)
}

fn fresh_if_newer(pinned: &Trust, signed_list: Vec<u8>, version: u64) -> Option<Trust> {
    let fresh = Trust::new(pinned.admin_public().to_vec(), signed_list);
    if fresh.verifier().is_err() { return None; }
    if version >= pinned.version().ok()? { Some(fresh) } else { None }
}

/// Whatever the environment did not name, take from the credential on disk — but only when
/// there is exactly one, so there is no question of which team this machine belongs to.
fn fill_from_credential(
    config_dir: &std::path::Path,
    token: Option<String>,
    machine: Option<String>,
    space: Option<String>,
) -> Option<(String, String, String, Option<Trust>)> {
    use crate::team_join;
    let space = space.or_else(|| single_team(config_dir))?;
    let path = team_join::credential_path(config_dir, &space);
    let cred = team_join::load(&path).ok();
    let token = token.or_else(|| cred.as_ref().map(|c| c.token.clone()))?;
    let machine = machine.or_else(|| cred.as_ref().map(|c| c.machine.clone()))?;
    let trust = cred.as_ref().and_then(team_join::trust_of);
    if token.is_empty() || machine.is_empty() { return None; }
    Some((token, machine, space, trust))
}

/// The only team credential on disk, or None when there is none or more than one.
fn single_team(config_dir: &std::path::Path) -> Option<String> {
    let dir = config_dir.join("teams");
    let entries: Vec<String> = std::fs::read_dir(dir).ok()?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension()? == "cred" { p.file_stem().map(|s| s.to_string_lossy().to_string()) } else { None }
        })
        .collect();
    if entries.len() == 1 { entries.into_iter().next() } else { None }
}

/// The team server's record listing and fetch, over HTTP.
struct HttpRecords { base: String, token: String, machine: String, space: String }

impl HttpRecords {
    /// The current signed list as the server holds it: the raw signed-list bytes, nothing parsed.
    /// Verification happens where the pinned admin key lives, never here.
    fn fetch_list(&self) -> Option<(Vec<u8>, u64)> {
        let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(15)).build();
        let url = format!("{}/list", self.base.trim_end_matches('/'));
        let body = serde_json::json!({"space": self.space, "machine": self.machine});
        let reply: serde_json::Value = agent.post(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(body).ok()?
            .into_json().ok()?;
        let version = reply["version"].as_u64()?;
        let hex = reply["signed_list"].as_str()?;
        if hex.len() % 2 != 0 { return None; }
        let bytes: Vec<u8> = (0..hex.len()).step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
            .collect::<Option<Vec<u8>>>()?;
        Some((bytes, version))
    }
}

impl RecordSource for HttpRecords {
    fn space(&self) -> &str { &self.space }

    fn list(&self) -> Result<Vec<String>, String> {
        Ok(self.list_advertised()?.into_iter().map(|(conv, _)| conv).collect())
    }

    /// The listing: every conversation id, and the sequence its stored record advertises when the
    /// server gives one. An older server sends no "sequences", so every id comes back unadvertised.
    fn list_advertised(&self) -> Result<Vec<(String, Option<u64>)>, String> {
        let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(15)).build();
        let url = format!("{}/sessions", self.base.trim_end_matches('/'));
        let body = serde_json::json!({"space": self.space, "machine": self.machine});
        let reply: serde_json::Value = agent.post(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(body).map_err(|_| "cannot list team sessions".to_string())?
            .into_json().map_err(|_| "bad listing".to_string())?;
        let sequences = reply["sequences"].as_object();
        Ok(reply["conversations"].as_array().map(|a| a.iter()
            .filter_map(|v| v.as_str().map(|id| {
                let seq = sequences.and_then(|m| m.get(id)).and_then(serde_json::Value::as_u64);
                (id.to_string(), seq)
            })).collect()).unwrap_or_default())
    }

    fn fetch(&self, conversation: &str) -> Result<Vec<u8>, String> {
        let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(30)).build();
        let url = format!("{}/record/sealed/read", self.base.trim_end_matches('/'));
        let body = serde_json::json!({"space": self.space, "conversation": conversation, "machine": self.machine});
        let resp = agent.post(&url).set("Authorization", &format!("Bearer {}", self.token))
            .send_json(body).map_err(|_| "cannot read a team record".to_string())?;
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf).map_err(|_| "cannot read a team record".to_string())?;
        Ok(buf)
    }
}
