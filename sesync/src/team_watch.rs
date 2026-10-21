//! The team side of `--watch`: when a team is configured, each pass also pulls and verifies the
//! team's sealed records for display. It is off unless a joined team credential or the
//! environment names a team, so an install without a team behaves exactly as before.

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
    /// The trust root held here: the admin key pinned at join and the newest signed list kept
    /// since (`team_join::ratchet_list`).
    pub trust: Option<Trust>,
}

/// One team's pull and the floors it was pulled under: the floors are per (space,
/// conversation), so a record received under one team never raises another team's floor.
#[derive(Debug, Clone)]
pub struct TeamPull {
    pub space: String,
    pub out: PullOutcome,
}

/// One verified copy of a conversation, as one team's pull returned it. The Team list is built
/// from these, so a row still knows which teams hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamCopy {
    pub space: String,
    pub conversation: String,
    /// The record sequence the pull accepted for it.
    pub seq: u64,
    pub text: Vec<u8>,
}

/// One entry per conversation from verified copies: the highest sequence wins, so the newest copy
/// a team holds is the one shown. Ties keep the first copy in order.
pub fn newest_of(copies: &[TeamCopy]) -> Vec<(String, Vec<u8>)> {
    let mut best: BTreeMap<&str, &TeamCopy> = BTreeMap::new();
    for c in copies {
        if best.get(c.conversation.as_str()).map_or(true, |b| c.seq > b.seq) {
            best.insert(&c.conversation, c);
        }
    }
    best.into_iter().map(|(conv, c)| (conv.to_string(), c.text.clone())).collect()
}

impl TeamWatch {
    /// The first configured team, for callers that act on a single one (the TUI's manual
    /// refresh uses all of them: see `teams_from_env`).
    pub fn from_env(config_dir: &std::path::Path) -> Option<TeamWatch> {
        teams_from_env(config_dir).into_iter().next()
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

    /// Pull every team this machine belongs to. One conversation shared into several teams is
    /// fetched from each team that advertises it above that team's own floor, but the floors
    /// stay per team, and the caller merges the results so the local view holds one entry per
    /// conversation. Order is by space name, so it is deterministic.
    pub fn pull_each(teams: &[TeamWatch], floors: &BTreeMap<String, BTreeMap<String, u64>>) -> Vec<TeamPull> {
        let empty = BTreeMap::new();
        teams.iter().map(|t| {
            let own = floors.get(&t.space).unwrap_or(&empty);
            TeamPull { space: t.space.clone(), out: t.pull_all(own) }
        }).collect()
    }

    /// Keep every member list these pulls verified that is newer than the one a team's credential
    /// holds (`team_join::ratchet_list`): the list held here only moves forward, so after a revoke
    /// no server can hand this machine the older list back. Returns each team whose list moved,
    /// with its new version, or why it could not be kept.
    pub fn keep_newer_lists(config_dir: &std::path::Path, pulls: &[TeamPull]) -> Vec<(String, Result<u64, String>)> {
        pulls.iter().filter_map(|p| {
            let list = p.out.list.as_ref()?;
            match crate::team_join::ratchet_list(config_dir, &p.space, list) {
                Ok(Some(version)) => Some((p.space.clone(), Ok(version))),
                Ok(None) => None,
                Err(e) => Some((p.space.clone(), Err(e))),
            }
        }).collect()
    }

    /// Every verified copy the pulls returned, one per team and conversation, in pull order.
    pub fn copies(pulls: &[TeamPull]) -> Vec<TeamCopy> {
        pulls.iter().flat_map(|p| p.out.shown.iter().map(move |r| TeamCopy {
            space: p.space.clone(),
            conversation: r.conversation.clone(),
            seq: p.out.accepted.get(&r.conversation).copied().unwrap_or(0),
            text: r.text.clone(),
        })).collect()
    }

    /// Every conversation the given pulls can show, one entry each: when a conversation was
    /// pulled from more than one team the highest accepted sequence wins, so the newest copy a
    /// team holds is the one shown. Ties keep the first team in order.
    pub fn newest_per_conversation(pulls: &[TeamPull]) -> Vec<(String, Vec<u8>)> {
        newest_of(&TeamWatch::copies(pulls))
    }

    fn pull_with(&self, last_seen: &BTreeMap<String, u64>,
                 pull: fn(&dyn RecordSource, &DeviceFile, Option<&Trust>, &BTreeMap<String, u64>) -> PullOutcome)
        -> PullOutcome
    {
        let src = HttpRecords { base: self.base.clone(), token: self.token.clone(),
                                machine: self.machine.clone(), space: self.space.clone() };
        let fresh = self.refreshed_trust(&src);
        let active = fresh.as_ref().or(self.trust.as_ref());
        let mut out = pull(&src, &self.device, active, last_seen);
        out.list = fresh;
        out
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

/// Every team this machine belongs to. A credential under the configuration directory names each
/// team's space and the server it was joined on; `SESYNC_TEAM_URL` serves credentials written
/// before the server was recorded, and a team configured only by the environment. A token and
/// machine may come from the environment instead when there is exactly one team (or the space is
/// named with them); the environment's server then goes with them. `SESYNC_TEAM_SPACE` narrows
/// everything to that one team (the pre-multi-team behaviour). Sorted by space.
pub fn teams_from_env(config_dir: &std::path::Path) -> Vec<TeamWatch> {
    use crate::team_join;
    let env_base = std::env::var("SESYNC_TEAM_URL").ok();
    let token = std::env::var("SESYNC_TEAM_TOKEN").ok();
    let machine = std::env::var("SESYNC_TEAM_MACHINE").ok();
    let named = std::env::var("SESYNC_TEAM_SPACE").ok();
    let mut spaces: Vec<String> = match &named {
        Some(s) => vec![s.clone()],
        None => all_teams(config_dir),
    };
    // No credential at all: the environment may still name one team entirely.
    if spaces.is_empty() {
        if let (Some(base), Some(t), Some(m)) = (env_base, token, machine) {
            let Ok(device) = load_or_create(config_dir) else { return Vec::new() };
            return vec![TeamWatch { base, token: t, machine: m, space: String::new(), device, trust: None }];
        }
        return Vec::new();
    }
    // Environment token/machine apply only when they cannot be ambiguous (one team) or when they
    // were named together with the space.
    let env_applies = spaces.len() == 1 || named.is_some();
    spaces.sort();
    spaces.dedup();
    spaces.into_iter().filter_map(|space| {
        let cred = team_join::load(&team_join::credential_path(config_dir, &space)).ok();
        // A join still waiting for the admin has nothing to pull or push yet: every data route
        // refuses it. `team_join::check_approval` turns it into a working team once approved.
        if cred.as_ref().map_or(false, team_join::is_waiting) {
            return None;
        }
        let env_token = token.clone().filter(|_| env_applies);
        // An environment token goes with the environment's server; a credential's own token goes
        // with the server it was joined on.
        let joined_on = cred.as_ref().and_then(|c| c.server.clone());
        let base = if env_token.is_some() { env_base.clone().or(joined_on) } else { joined_on.or_else(|| env_base.clone()) }?;
        let token = env_token.or_else(|| cred.as_ref().map(|c| c.token.clone()))?;
        let machine = machine.clone().filter(|_| env_applies).or_else(|| cred.as_ref().map(|c| c.machine.clone()))?;
        if token.is_empty() || machine.is_empty() { return None; }
        let device = load_or_create(config_dir).ok()?;
        let trust = cred.as_ref().and_then(team_join::trust_of);
        Some(TeamWatch { base, token, machine, space, device, trust })
    }).collect()
}

/// Every credential under the configuration directory, as space names (the file names decoded,
/// see `team_join::file_stem_of`).
pub fn all_teams(config_dir: &std::path::Path) -> Vec<String> {
    let dir = config_dir.join("teams");
    let mut out: Vec<String> = std::fs::read_dir(dir).ok().into_iter().flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension()? == "cred" {
                p.file_stem().map(|s| crate::team_join::space_of_file_stem(&s.to_string_lossy()))
            } else {
                None
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
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
        let reply: serde_json::Value = match agent.post(&url).set("Authorization", &format!("Bearer {}", self.token)).send_json(body) {
            Ok(r) => r.into_json().map_err(|_| "the team server sent a listing it cannot read".to_string())?,
            // The server's own reason: a device the admin revoked or removed is refused here, and
            // a bare "cannot list" would hide that from the member.
            Err(ureq::Error::Status(code, r)) => {
                let why = r.into_json::<serde_json::Value>().ok()
                    .and_then(|v| v["error"].as_str().map(str::to_string))
                    .unwrap_or_default();
                return Err(format!("the team server refused this device ({code}{}{why})", if why.is_empty() { "" } else { ": " }));
            }
            Err(e) => return Err(format!("cannot reach the team server: {e}")),
        };
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
