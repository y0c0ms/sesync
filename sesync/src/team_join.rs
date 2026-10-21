//! The member side: `sesync team join` on a machine, and the stored credential it leaves behind.
//!
//! The credential file holds the member's token for one team, the server it was joined on, and
//! the trust root the join pinned: the admin's public key and, once the admin has signed this
//! device in, the admin-signed member list, exactly as the server returned it. A join with a new
//! key waits for the admin: its credential has the admin key and no list until `check_approval`
//! finds the device approved. It is written owner-only (mode 600) and read back only by the owner.
//! Nothing here prints the token, and errors never echo its contents.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::sealed::Trust;

/// The stored credential for one team on one machine.
#[derive(Debug, PartialEq)]
pub struct Credential {
    pub space: String,
    pub member: String,
    pub machine: String,
    pub token: String,
    /// The pinned admin key and the signed list the join received, when the team has a trust
    /// root. Old credentials and pre-trust-root teams have neither.
    pub admin_public: Option<Vec<u8>>,
    pub signed_list: Option<Vec<u8>>,
    pub list_version: Option<u64>,
    /// The server the team was joined on, without a trailing slash. Credentials written before
    /// it was recorded have none and use SESYNC_TEAM_URL.
    pub server: Option<String>,
}

/// The trust root this credential pinned, if it pinned one.
pub fn trust_of(c: &Credential) -> Option<Trust> {
    Some(Trust::new(c.admin_public.clone()?, c.signed_list.clone()?))
}

/// Whether this credential's join is still waiting for the admin: it pinned the admin key but
/// holds no signed list yet. A legacy credential (no admin key) is never waiting.
pub fn is_waiting(c: &Credential) -> bool {
    c.admin_public.is_some() && c.signed_list.is_none()
}

/// One joined team as the Team tab shows it, read from this machine's credential. No network.
#[derive(Debug, Clone, PartialEq)]
pub struct TeamCard {
    pub space: String,
    pub member: String,
    pub machine: String,
    /// The server the team was joined on; empty for a credential older than that record.
    pub server: String,
    pub standing: Standing,
}

/// Where this machine stands in a team.
#[derive(Debug, Clone, PartialEq)]
pub enum Standing {
    /// The admin signed this device into the list; the version the credential holds.
    Listed { version: Option<u64> },
    /// Joined and waiting for the admin; the fingerprint the admin compares before approving.
    Waiting { fingerprint: String },
    /// The credential could not be read (the reason, for the user).
    Unreadable(String),
}

/// Every team this machine joined, by name, with where it stands.
pub fn team_cards(config_dir: &Path) -> Vec<TeamCard> {
    crate::team_watch::all_teams(config_dir).into_iter().map(|space| {
        match load(&credential_path(config_dir, &space)) {
            Ok(c) => {
                let standing = if is_waiting(&c) {
                    Standing::Waiting { fingerprint: own_fingerprint(config_dir).unwrap_or_default() }
                } else {
                    Standing::Listed { version: c.list_version }
                };
                TeamCard { space, member: c.member, machine: c.machine, server: c.server.unwrap_or_default(), standing }
            }
            Err(e) => TeamCard {
                space, member: String::new(), machine: String::new(), server: String::new(),
                standing: Standing::Unreadable(e),
            },
        }
    }).collect()
}

/// Where the credential for `space` lives: a file directly under `<config>/teams`, named after
/// the team (see `file_stem_of`).
pub fn credential_path(config_dir: &Path, space: &str) -> PathBuf {
    config_dir.join("teams").join(format!("{}.cred", file_stem_of(space)))
}

/// The team name as a file name that is safe on every platform: the name itself, except that a
/// path separator, a character Windows refuses (`: * ? " < > |`), a control character, `%`, a
/// leading dot and a trailing dot or space are percent-encoded. Ordinary names, spaces and
/// non-ASCII letters included, keep the file name they always had.
pub fn file_stem_of(space: &str) -> String {
    let mut out = String::with_capacity(space.len());
    for (i, ch) in space.char_indices() {
        let last = i + ch.len_utf8() == space.len();
        let encode = matches!(ch, '%' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            || ch.is_control()
            || (i == 0 && ch == '.')
            || (last && (ch == '.' || ch == ' '));
        if encode {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// The team name a credential file stem stands for: the inverse of `file_stem_of`.
pub fn space_of_file_stem(stem: &str) -> String {
    let b = stem.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| stem.to_string())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 { return None; }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Write the credential owner-only. Creates the directory with owner-only permissions too.
pub fn save(path: &Path, c: &Credential) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|_| "cannot create the team credential directory".to_string())?;
        set_owner_only(dir, 0o700)?;
    }
    let mut text = format!("space={}\nmember={}\nmachine={}\ntoken={}\n", c.space, c.member, c.machine, c.token);
    if let Some(k) = &c.admin_public { text.push_str(&format!("admin={}\n", hex(k))); }
    if let Some(l) = &c.signed_list { text.push_str(&format!("list={}\n", hex(l))); }
    if let Some(v) = c.list_version { text.push_str(&format!("list_version={v}\n")); }
    if let Some(s) = &c.server { text.push_str(&format!("server={s}\n")); }
    let tmp = path.with_extension("cred.tmp");
    {
        let mut f = open_owner_only(&tmp)?;
        f.write_all(text.as_bytes()).map_err(|_| "cannot write the team credential".to_string())?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path).map_err(|_| "cannot save the team credential".to_string())
}

/// Read a credential back. Refuses a file that other users can read, since the token is in it.
pub fn load(path: &Path) -> Result<Credential, String> {
    check_owner_only(path)?;
    let text = std::fs::read_to_string(path).map_err(|_| "no team credential for this team; run sesync team join".to_string())?;
    let mut space = None; let mut member = None; let mut machine = None; let mut token = None;
    let mut admin = None; let mut list = None; let mut list_version = None; let mut server = None;
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k {
                "space" => space = Some(v.to_string()),
                "member" => member = Some(v.to_string()),
                "machine" => machine = Some(v.to_string()),
                "token" => token = Some(v.to_string()),
                "admin" => admin = Some(unhex(v).ok_or("the team credential file is damaged; run sesync team join again")?),
                "list" => list = Some(unhex(v).ok_or("the team credential file is damaged; run sesync team join again")?),
                "list_version" => list_version = v.parse::<u64>().ok(),
                "server" => server = Some(v.to_string()).filter(|s| !s.is_empty()),
                _ => {}
            }
        }
    }
    match (space, member, machine, token) {
        (Some(space), Some(member), Some(machine), Some(token)) => Ok(Credential {
            space, member, machine, token, admin_public: admin, signed_list: list, list_version, server,
        }),
        _ => Err("the team credential file is damaged; run sesync team join again".into()),
    }
}

#[cfg(unix)]
fn open_owner_only(path: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)
        .map_err(|_| "cannot create the team credential".to_string())
}

#[cfg(not(unix))]
fn open_owner_only(path: &Path) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path)
        .map_err(|_| "cannot create the team credential".to_string())
}

#[cfg(unix)]
fn set_owner_only(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|_| "cannot restrict the team credential directory".to_string())
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path, _mode: u32) -> Result<(), String> { Ok(()) }

#[cfg(unix)]
fn check_owner_only(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|_| "no team credential for this team; run sesync team join".to_string())?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err("the team credential is readable by other users; refusing to use it".into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_owner_only(_path: &Path) -> Result<(), String> { Ok(()) }

/// What `join` did.
#[derive(Debug, PartialEq)]
pub struct Joined {
    /// Where the credential was saved.
    pub path: PathBuf,
    /// True when the key is new to the team: the join waits for the admin to approve it, and the
    /// credential holds the token and the admin key but no member list yet.
    pub pending: bool,
    /// This machine's device fingerprint, computed here from the key it sent. The member gives it
    /// to the admin out of band; the admin compares it before approving.
    pub fingerprint: String,
}

/// This machine's device fingerprint, as the admin sees it in the roster.
pub fn own_fingerprint(config_dir: &Path) -> Result<String, String> {
    let device = crate::device_keys::load_or_create(config_dir)?;
    Ok(sesync_team::membership::fingerprint(&[device.public_signing(), device.public_box()].concat()))
}

/// Join a team on this machine with the 6-digit code. Sends this machine's device public keys
/// with the call, then saves the token, the server address and the trust root the reply carried:
/// for a key the team already lists, the admin key and the signed list; for a new key, the admin
/// key alone (the join waits for the admin, see `check_approval`). The code is read by the caller
/// and passed in; it is never stored.
pub fn join(base: &str, space: &str, member: &str, machine: &str, code: &str, config_dir: &Path) -> Result<Joined, String> {
    // Authenticator apps show the code as two groups ("123 456"); accept it as typed.
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err("the code is 6 digits".into());
    }
    let device = crate::device_keys::load_or_create(config_dir)?;
    let entry = [device.public_signing(), device.public_box()].concat();
    // Proves this machine holds the private key of the entry it joins with, so a member who
    // knows another machine's public keys cannot join under its name.
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(device.signing_pkcs8())
        .map_err(|_| "the device key file is damaged".to_string())?;
    let proof = pair.sign(&sesync_team::membership::enrol_message(space, member, machine, &code, &entry));
    let body = serde_json::json!({
        "space": space, "member": member, "machine": machine, "code": code,
        "device": hex(&entry), "proof": hex(proof.as_ref()),
    });
    let reply = post(base, "/enrol", None, body)?;
    let token = reply["token"].as_str().ok_or("the server gave no token")?.to_string();
    let admin_public = reply["admin_public"].as_str().and_then(unhex);
    let pending = reply["status"].as_str() == Some("pending");
    let (signed_list, list_version) = if pending {
        if admin_public.is_none() {
            return Err("the server gave no admin key with the join request".into());
        }
        (None, None)
    } else {
        (reply["signed_list"].as_str().and_then(unhex), reply["list_version"].as_u64())
    };
    let path = credential_path(config_dir, space);
    save(&path, &Credential {
        space: space.into(), member: member.into(), machine: machine.into(), token,
        admin_public, signed_list, list_version,
        server: Some(base.trim_end_matches('/').to_string()),
    })?;
    Ok(Joined { path, pending, fingerprint: sesync_team::membership::fingerprint(&entry) })
}

/// Where a waiting join stands.
#[derive(Debug, PartialEq)]
pub enum Approval {
    /// The admin signed this device in: the list it now gets verified against the pinned admin
    /// key and is stored in the credential.
    Approved { version: u64 },
    /// Still waiting for the admin, who compares this fingerprint before approving.
    Waiting { machine: String, fingerprint: String },
}

impl Approval {
    /// The one line the daemon logs and the TUI shows for this team.
    pub fn describe(&self, space: &str) -> String {
        match self {
            Approval::Approved { version } =>
                format!("team [{space}]: approved by the admin; member list v{version} stored, syncing starts"),
            Approval::Waiting { machine, fingerprint } =>
                format!("team [{space}]: waiting for the admin to approve {machine} (fingerprint {fingerprint})"),
        }
    }
}

/// For a credential whose join waits for the admin (see `is_waiting`), ask the server whether the
/// admin approved it: the member list is served only to a listed device. On approval the list is
/// verified with the pinned admin key and stored, so the credential works like any other from
/// then on. A token that ran out while waiting is renewed first (renewal works once the device is
/// listed). Ok(None) for a credential that is not waiting.
pub fn check_approval(fallback: Option<&str>, config_dir: &Path, space: &str) -> Result<Option<Approval>, String> {
    let path = credential_path(config_dir, space);
    let mut c = load(&path)?;
    if !is_waiting(&c) {
        return Ok(None);
    }
    let base = c.server.clone().or_else(|| fallback.map(str::to_string))
        .ok_or("no server for this team; set SESYNC_TEAM_URL")?;
    let waiting = Approval::Waiting { machine: c.machine.clone(), fingerprint: own_fingerprint(config_dir)? };
    let body = serde_json::json!({"space": space, "machine": c.machine});
    let (mut status, mut reply) = post_status(&base, "/list", Some(&c.token), body.clone())?;
    if status == 401 {
        let (st, r) = post_status(&base, "/renew", Some(&c.token), body.clone())?;
        match st {
            200 => {
                c.token = r["token"].as_str().ok_or("the server gave no token")?.to_string();
                save(&path, &c)?;
                (status, reply) = post_status(&base, "/list", Some(&c.token), body)?;
            }
            403 => return Ok(Some(waiting)),
            _ => return Err(format!("the join request can no longer be confirmed ({st}); join again with the code")),
        }
    }
    match status {
        200 => {
            let list = reply["signed_list"].as_str().and_then(unhex).ok_or("the server sent no member list")?;
            let admin = c.admin_public.clone().ok_or("no pinned admin key")?;
            let trust = Trust::new(admin, list.clone());
            trust.verifier().map_err(|_| "the member list does not verify against the pinned admin key; not stored".to_string())?;
            let version = trust.version().map_err(|_| "the member list is malformed; not stored".to_string())?;
            c.signed_list = Some(list);
            c.list_version = Some(version);
            save(&path, &c)?;
            Ok(Some(Approval::Approved { version }))
        }
        403 => Ok(Some(waiting)),
        st => Err(format!("server refused ({st}): {}", reply["error"].as_str().unwrap_or_default())),
    }
}

/// `check_approval` for every waiting credential on this machine (or only SESYNC_TEAM_SPACE when
/// it names one), with SESYNC_TEAM_URL as the fallback server. The daemon's team step and the
/// TUI's pull run this first, so an approved device starts syncing without a new join.
pub fn check_all_waiting(config_dir: &Path) -> Vec<(String, Result<Approval, String>)> {
    let fallback = std::env::var("SESYNC_TEAM_URL").ok();
    waiting_teams(config_dir).into_iter()
        .filter_map(|s| match check_approval(fallback.as_deref(), config_dir, &s) {
            Ok(None) => None,
            Ok(Some(a)) => Some((s, Ok(a))),
            Err(e) => Some((s, Err(e))),
        })
        .collect()
}

/// The teams on this machine whose join waits for the admin (or only SESYNC_TEAM_SPACE when it
/// names one), sorted.
pub fn waiting_teams(config_dir: &Path) -> Vec<String> {
    let spaces = match std::env::var("SESYNC_TEAM_SPACE") {
        Ok(s) => vec![s],
        Err(_) => crate::team_watch::all_teams(config_dir),
    };
    spaces.into_iter()
        .filter(|s| load(&credential_path(config_dir, s)).map_or(false, |c| is_waiting(&c)))
        .collect()
}

/// Renew the stored token silently, inside the 7-day window. Writes the new token back. The
/// request goes to the server the team was joined on; `fallback` (SESYNC_TEAM_URL) only serves a
/// credential that predates recording it.
pub fn renew(fallback: Option<&str>, config_dir: &Path, space: &str) -> Result<(), String> {
    let path = credential_path(config_dir, space);
    let mut c = load(&path)?;
    let base = c.server.clone().or_else(|| fallback.map(str::to_string))
        .ok_or("no server for this team; set SESYNC_TEAM_URL")?;
    let body = serde_json::json!({"space": space, "machine": c.machine});
    let reply = post(&base, "/renew", Some(&c.token), body)?;
    c.token = reply["token"].as_str().ok_or("the server gave no token")?.to_string();
    save(&path, &c)
}

/// How long before its expiry a token is renewed. Tokens last a day; the daemon's team step runs
/// at most a minute apart, so an hour leaves room for an outage.
pub const RENEW_AHEAD_SECS: u64 = 60 * 60;

/// Renew the stored token when it runs out within RENEW_AHEAD_SECS of `now`, or already has.
/// Ok(true) when renewed, Ok(false) when the token is still good. Past the 7-day window the server
/// refuses, and the error says to join again with the code. A join still waiting for the admin is
/// left alone (Ok(false)): `check_approval` renews it once the admin approved it.
pub fn renew_if_due(fallback: Option<&str>, config_dir: &Path, space: &str, now: u64) -> Result<bool, String> {
    let c = load(&credential_path(config_dir, space))?;
    if is_waiting(&c) {
        return Ok(false);
    }
    if let Some(expiry) = sesync_team::auth::expiry_of(&c.token) {
        if expiry > now.saturating_add(RENEW_AHEAD_SECS) {
            return Ok(false);
        }
    }
    renew(fallback, config_dir, space).map(|()| true)
}

/// `renew_if_due` for every team credential on this machine (or only SESYNC_TEAM_SPACE when it
/// names one), with SESYNC_TEAM_URL as the fallback server. The daemon runs this before each team
/// step and the TUI before a manual pull, so a team keeps working past its first day.
pub fn renew_all_due(config_dir: &Path, now: u64) -> Vec<(String, Result<bool, String>)> {
    let fallback = std::env::var("SESYNC_TEAM_URL").ok();
    let spaces = match std::env::var("SESYNC_TEAM_SPACE") {
        Ok(s) => vec![s],
        Err(_) => crate::team_watch::all_teams(config_dir),
    };
    spaces.into_iter()
        .filter(|s| credential_path(config_dir, s).exists())
        .map(|s| {
            let r = renew_if_due(fallback.as_deref(), config_dir, &s, now);
            (s, r)
        })
        .collect()
}

fn post(base: &str, path: &str, token: Option<&str>, body: serde_json::Value) -> Result<serde_json::Value, String> {
    match post_status(base, path, token, body)? {
        (code, reply) if (200..300).contains(&code) => Ok(reply),
        (code, reply) => Err(format!("server refused ({code}): {}", reply["error"].as_str().unwrap_or_default())),
    }
}

/// POST and return the status with the JSON body (Null when the body is not JSON). Err only when
/// the server cannot be reached or a success reply is not JSON.
fn post_status(base: &str, path: &str, token: Option<&str>, body: serde_json::Value) -> Result<(u16, serde_json::Value), String> {
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(15)).build();
    let mut req = agent.post(&format!("{}{path}", base.trim_end_matches('/')));
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.send_json(body) {
        Ok(r) => {
            let code = r.status();
            let reply = r.into_json::<serde_json::Value>().map_err(|_| "server reply was not JSON".to_string())?;
            Ok((code, reply))
        }
        Err(ureq::Error::Status(code, r)) => Ok((code, r.into_json::<serde_json::Value>().unwrap_or_default())),
        Err(_) => Err("cannot reach the team server".into()),
    }
}
