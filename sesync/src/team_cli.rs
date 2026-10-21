//! `sesync team ...`: the admin commands for teams, run on the operator's machine.
//!
//!   sesync team create <name> [--member <m>]...                      create a team; prints the authenticator URI once
//!   sesync team invite <name> <member>                              add a member to an existing team
//!   sesync team roster <name>                                       listed devices and join requests, with fingerprints
//!   sesync team approve <name> <member> <machine> [--fingerprint <fp>]   sign a waiting device into the list
//!   sesync team decline <name> <member> <machine>                   refuse a waiting device for good
//!   sesync team revoke <name> <member> <machine>                    sign one device out of the list
//!   sesync team remove <name> <member>                              sign every device of a member out, and drop the member
//!
//! Creating a team generates an admin key on this machine and signs the first member list with
//! it; the server pins the key and can never forge a list. The admin-signed list is the roster:
//! `approve`, `revoke` and `remove` build version v+1 from the list v this machine fetched and
//! verified with its own admin key (never from what devices sent at join), sign it here and
//! publish it. A join with the team code only ever asks; nothing joins by being enrolled.
//!
//! Settings come from the environment, never the command line, so nothing secret is in shell
//! history or the process list:
//!   SESYNC_TEAM_URL          base URL of the team server, e.g. http://127.0.0.1:30500
//!   SESYNC_ADMIN_TOKEN_FILE  path to the admin token file (mode 600, owner only)
//!
//! The admin token is read from its file and sent only in the Authorization header. It is
//! never printed, logged or echoed in an error.

use std::path::Path;

use sesync_team::membership::{fingerprint, Device, MemberList, SignedList, Verifier};

use crate::device_keys::DeviceFile;

#[derive(Debug, PartialEq)]
pub enum Command {
    Create { name: String, members: Vec<String> },
    Invite { name: String, member: String },
    Roster { name: String },
    Approve { name: String, member: String, machine: String, fingerprint: Option<String> },
    Decline { name: String, member: String, machine: String },
    Revoke { name: String, member: String, machine: String },
    Remove { name: String, member: String },
}

const USAGE: &str = "usage: sesync team create <name> [--member <m>]... | invite <name> <member> | roster <name> \
| approve <name> <member> <machine> [--fingerprint <fp>] | decline <name> <member> <machine> \
| revoke <name> <member> <machine> | remove <name> <member>";

/// Parse the arguments after `team`. Returns a usage message on any mistake.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let usage = || USAGE.to_string();
    // The positional arguments after the verb, exactly `n` of them, none starting with "--".
    let positional = |n: usize| -> Result<Vec<String>, String> {
        let rest = &args[1..];
        if rest.len() != n || rest.iter().any(|a| a.starts_with("--")) {
            return Err(usage());
        }
        Ok(rest.to_vec())
    };
    match args.first().map(String::as_str) {
        Some("create") => {
            let name = args.get(1).filter(|n| !n.starts_with("--")).ok_or_else(usage)?.clone();
            let mut members = Vec::new();
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--member" {
                    let m = args.get(i + 1).ok_or_else(usage)?.clone();
                    members.push(m);
                    i += 2;
                } else {
                    return Err(usage());
                }
            }
            Ok(Command::Create { name, members })
        }
        Some("invite") => {
            let [name, member]: [String; 2] = positional(2)?.try_into().map_err(|_| usage())?;
            Ok(Command::Invite { name, member })
        }
        Some("roster") => {
            let [name]: [String; 1] = positional(1)?.try_into().map_err(|_| usage())?;
            Ok(Command::Roster { name })
        }
        Some("approve") => {
            let (rest, fingerprint) = match args.iter().position(|a| a == "--fingerprint") {
                Some(i) if i + 2 == args.len() => (&args[1..i], Some(args[i + 1].clone())),
                Some(_) => return Err(usage()),
                None => (&args[1..], None),
            };
            if rest.len() != 3 || rest.iter().any(|a| a.starts_with("--")) {
                return Err(usage());
            }
            Ok(Command::Approve { name: rest[0].clone(), member: rest[1].clone(), machine: rest[2].clone(), fingerprint })
        }
        Some("decline") => {
            let [name, member, machine]: [String; 3] = positional(3)?.try_into().map_err(|_| usage())?;
            Ok(Command::Decline { name, member, machine })
        }
        Some("revoke") => {
            let [name, member, machine]: [String; 3] = positional(3)?.try_into().map_err(|_| usage())?;
            Ok(Command::Revoke { name, member, machine })
        }
        Some("remove") => {
            let [name, member]: [String; 2] = positional(2)?.try_into().map_err(|_| usage())?;
            Ok(Command::Remove { name, member })
        }
        _ => Err(usage()),
    }
}

/// Read the admin token from a file that only its owner can read. Refuses a file that other
/// users can read, since the token would then be exposed.
pub fn read_admin_token(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|_| "cannot read the admin token file".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err("admin token file must be mode 600 (owner only)".into());
        }
    }
    let _ = meta;
    let token = std::fs::read_to_string(path).map_err(|_| "cannot read the admin token file".to_string())?;
    let token = token.trim().to_string();
    if token.len() < 32 {
        return Err("admin token is too short".into());
    }
    Ok(token)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 { return None; }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Where the admin key for this machine's teams lives. It is generated at `team create` and
/// never leaves the machine; every roster change reads it to verify list v and sign v+1.
pub fn admin_key_path(config_dir: &Path) -> std::path::PathBuf {
    config_dir.join("admin").join("device.key")
}

/// Whether this machine holds an admin key, so it can run the admin commands at all.
pub fn has_admin_key(config_dir: &Path) -> bool {
    admin_key_path(config_dir).is_file()
}

/// A team this machine administers, with the server it lives on and the admin token file it was
/// created with, so the TUI's admin dialogs need no environment. The token itself stays in its
/// own file; only the path is recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct AdminTeam {
    pub team: String,
    pub server: String,
    pub token_file: String,
}

fn administered_path(config_dir: &Path) -> std::path::PathBuf {
    config_dir.join("admin").join("teams.json")
}

/// The teams this machine administers, in the order they were recorded. Empty when none is, or
/// when the record cannot be read.
pub fn administered(config_dir: &Path) -> Vec<AdminTeam> {
    let Ok(text) = std::fs::read_to_string(administered_path(config_dir)) else { return Vec::new() };
    let Ok(serde_json::Value::Array(rows)) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    rows.iter().filter_map(|r| Some(AdminTeam {
        team: r["team"].as_str()?.to_string(),
        server: r["server"].as_str().unwrap_or("").to_string(),
        token_file: r["token_file"].as_str().unwrap_or("").to_string(),
    })).collect()
}

/// Record (or update) a team this machine administers. Written through a temporary file, so a
/// crash never leaves half a record.
pub fn remember_administered(config_dir: &Path, team: &AdminTeam) -> Result<(), String> {
    let mut all = administered(config_dir);
    match all.iter_mut().find(|t| t.team == team.team) {
        Some(t) => *t = team.clone(),
        None => all.push(team.clone()),
    }
    let rows: Vec<serde_json::Value> = all.iter()
        .map(|t| serde_json::json!({"team": t.team, "server": t.server, "token_file": t.token_file}))
        .collect();
    let path = administered_path(config_dir);
    let dir = path.parent().ok_or("no config directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&rows).unwrap_or_default())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot save {}: {e}", path.display()))
}

fn device_of(file: &DeviceFile) -> Result<Device, String> {
    Device::from_keys(file.signing_pkcs8(), *file.box_secret())
        .map_err(|_| "the admin key on this machine is unreadable".to_string())
}

/// The admin key on this machine, as a signing device.
fn admin_device(config_dir: &Path) -> Result<Device, String> {
    let admin = DeviceFile::read(&admin_key_path(config_dir))
        .map_err(|_| "no admin key on this machine; run this where the team was created".to_string())?;
    device_of(&admin)
}

/// A device in the roster, as the server names it. `entry` is checked against the verified list
/// before it is used for anything.
struct Named {
    member: Option<String>,
    machine: Option<String>,
    entry: Vec<u8>,
}

/// A join request, with the entry the device sent.
struct Request {
    member: String,
    machine: String,
    entry: Vec<u8>,
    age_secs: u64,
}

/// The roster this command acts on: list v, verified here with this machine's admin key, and the
/// names and requests the server keeps beside it.
struct Roster {
    list: SignedList,
    listed: Vec<Named>,
    pending: Vec<Request>,
}

/// Fetch `/admin/roster` and verify the list with the admin's own public key. The next version is
/// built from this list and nothing else, so a server cannot slip a key into it.
fn fetch_roster(agent: &ureq::Agent, base: &str, auth: &str, name: &str, admin: &Device) -> Result<Roster, String> {
    let reply = call(agent, &format!("{base}/admin/roster"), auth, serde_json::json!({"space": name}))?;
    let bytes = reply["signed_list"].as_str().and_then(unhex).ok_or("the server sent no member list")?;
    let list = SignedList::from_bytes(&bytes).map_err(|_| "the server's member list is malformed".to_string())?;
    Verifier::new(admin.public_key()).accept(&list)
        .map_err(|_| "the server's member list is not signed by this machine's admin key; nothing was signed".to_string())?;
    let entry_of = |v: &serde_json::Value| v["entry"].as_str().and_then(unhex);
    let listed = reply["listed"].as_array().map(Vec::as_slice).unwrap_or_default().iter()
        .filter_map(|d| Some(Named {
            member: d["member"].as_str().map(str::to_string),
            machine: d["machine"].as_str().map(str::to_string),
            entry: entry_of(d)?,
        }))
        .filter(|n| list.list.devices.contains(&n.entry))
        .collect();
    let pending = reply["pending"].as_array().map(Vec::as_slice).unwrap_or_default().iter()
        .filter_map(|p| Some(Request {
            member: p["member"].as_str()?.to_string(),
            machine: p["machine"].as_str()?.to_string(),
            entry: entry_of(p)?,
            age_secs: p["age_secs"].as_u64().unwrap_or(0),
        }))
        .collect();
    Ok(Roster { list, listed, pending })
}

/// Sign `entries` as the next version after `roster` and publish it, with the members the change
/// removes from the team. Returns the published version.
fn publish_next(agent: &ureq::Agent, base: &str, auth: &str, name: &str, admin: &Device, roster: &Roster,
                entries: Vec<Vec<u8>>, removed_members: &[&str]) -> Result<u64, String> {
    let next = roster.list.list.version + 1;
    let signed = MemberList::new(next, entries).sign(admin);
    let reply = call(agent, &format!("{base}/admin/list"), auth, serde_json::json!({
        "space": name, "signed_list": hex(&signed.to_bytes()), "removed_members": removed_members,
    }))?;
    Ok(reply["version"].as_u64().unwrap_or(next))
}

/// Accept a fingerprint as people type it: case and separators do not matter.
fn normalize_fingerprint(fp: &str) -> String {
    fp.chars().filter(char::is_ascii_hexdigit).map(|c| c.to_ascii_lowercase()).collect()
}

pub fn age(secs: u64) -> String {
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 2 * 3600 => format!("{}m", s / 60),
        s if s < 2 * 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// One listed device of a team, as the admin's verified list names it.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceRow {
    pub member: Option<String>,
    pub machine: Option<String>,
    pub fingerprint: String,
    /// The key this machine's admin uses; it cannot be revoked or removed from here.
    pub admin: bool,
}

/// A join request waiting for an admin decision.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRow {
    pub member: String,
    pub machine: String,
    pub fingerprint: String,
    pub age_secs: u64,
}

/// The roster of one team, verified with this machine's admin key.
#[derive(Debug, Clone, PartialEq)]
pub struct RosterView {
    pub name: String,
    pub version: u64,
    pub devices: Vec<DeviceRow>,
    pub pending: Vec<PendingRow>,
}

/// Read a team's roster as structured rows. Nothing is signed.
pub fn roster_view(base: &str, token: &str, name: &str, config_dir: &Path) -> Result<RosterView, String> {
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(15)).build();
    let base = base.trim_end_matches('/');
    let auth = format!("Bearer {token}");
    let admin = admin_device(config_dir)?;
    let roster = fetch_roster(&agent, base, &auth, name, &admin)?;
    let admin_entry = sesync_team::membership::device_entry(&admin);
    let devices = roster.list.list.devices.iter().map(|entry| {
        let named = roster.listed.iter().find(|n| &n.entry == entry);
        DeviceRow {
            member: named.and_then(|n| n.member.clone()),
            machine: named.and_then(|n| n.machine.clone()),
            fingerprint: fingerprint(entry),
            admin: entry == &admin_entry,
        }
    }).collect();
    let pending = roster.pending.iter().map(|p| PendingRow {
        member: p.member.clone(),
        machine: p.machine.clone(),
        fingerprint: fingerprint(&p.entry),
        age_secs: p.age_secs,
    }).collect();
    Ok(RosterView { name: name.to_string(), version: roster.list.list.version, devices, pending })
}

/// The roster as the `sesync team roster` command prints it.
pub fn render_roster(view: &RosterView) -> String {
    let mut text = format!("team '{}': member list v{}\nlisted:\n", view.name, view.version);
    for d in &view.devices {
        let who = match (d.admin, d.member.as_deref(), d.machine.as_deref()) {
            (true, _, _) => "(admin key)".to_string(),
            (false, Some(member), Some(machine)) => format!("{member} / {machine}"),
            _ => "(no machine has joined with this key)".to_string(),
        };
        text.push_str(&format!("  {who:<40} {}\n", d.fingerprint));
    }
    if view.pending.is_empty() {
        text.push_str("pending: none\n");
    } else {
        text.push_str("pending (approve only after comparing the fingerprint with the member):\n");
        for p in &view.pending {
            let who = format!("{} / {}", p.member, p.machine);
            text.push_str(&format!("  {who:<40} {}  asked {} ago\n", p.fingerprint, age(p.age_secs)));
        }
    }
    text
}

/// Run a parsed command against the team server. Returns the human text, and for a created
/// team the one-time authenticator URI (nothing else ever returns it).
pub fn run_detailed(cmd: &Command, base: &str, token: &str, config_dir: &Path) -> Result<(String, Option<String>), String> {
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(15)).build();
    let base = base.trim_end_matches('/');
    let auth = format!("Bearer {token}");
    match cmd {
        Command::Create { name, members } => {
            // The admin key is generated here and pinned by the server: it never reaches the
            // server, so the server can verify lists but never make one.
            let admin = crate::device_keys::load_or_create(&config_dir.join("admin"))?;
            let admin_dev = device_of(&admin)?;
            let entry = [admin.public_signing(), admin.public_box()].concat();
            let signed = MemberList::new(1, vec![entry.clone()]).sign(&admin_dev);
            let body = serde_json::json!({
                "space": name, "members": members,
                "admin_public": hex(admin.public_signing()),
                "devices": [hex(&entry)],
                "signed_list": hex(&signed.to_bytes()),
            });
            let resp = call(&agent, &format!("{base}/admin/team"), &auth, body)?;
            let uri = resp["otpauth_uri"].as_str().ok_or("server gave no authenticator URI")?.to_string();
            let text = format!(
                "team '{name}' created.\n\
                 Add it to your authenticator app NOW. It is shown once and cannot be read back:\n\
                 {uri}\n\
                 The admin key for this machine is in {}; keep it there. Each device that joins\n\
                 waits for you: `sesync team roster {name}` lists the requests, and\n\
                 `sesync team approve {name} <member> <machine> --fingerprint <fp>` signs one in.\n",
                admin_key_path(config_dir).display());
            Ok((text, Some(uri)))
        }
        Command::Invite { name, member } => {
            let body = serde_json::json!({"space": name, "member": member});
            call(&agent, &format!("{base}/admin/invite"), &auth, body)?;
            Ok((format!("invited '{member}' to team '{name}'."), None))
        }
        Command::Roster { name } => Ok((render_roster(&roster_view(base, token, name, config_dir)?), None)),
        Command::Approve { name, member, machine, fingerprint: want } => {
            let admin = admin_device(config_dir)?;
            let roster = fetch_roster(&agent, base, &auth, name, &admin)?;
            let request = roster.pending.iter().find(|p| &p.member == member && &p.machine == machine)
                .ok_or_else(|| format!("no join request from '{member}' for machine '{machine}' in team '{name}'; nothing was signed"))?;
            // Computed here from the entry that will be signed, never taken from the server.
            let fp = fingerprint(&request.entry);
            if let Some(want) = want {
                if normalize_fingerprint(want) != normalize_fingerprint(&fp) {
                    return Err(format!(
                        "the request from '{member}' for '{machine}' has fingerprint {fp}, not {want}; nothing was signed"));
                }
            }
            let mut entries = roster.list.list.devices.clone();
            entries.push(request.entry.clone());
            let version = publish_next(&agent, base, &auth, name, &admin, &roster, entries, &[])?;
            let checked = if want.is_some() { "checked" } else { "not checked: compare it with the member" };
            Ok((format!("approved '{machine}' of '{member}' (fingerprint {fp}, {checked}) into list v{version} for '{name}'."), None))
        }
        Command::Decline { name, member, machine } => {
            call(&agent, &format!("{base}/admin/decline"), &auth,
                serde_json::json!({"space": name, "member": member, "machine": machine}))?;
            Ok((format!("declined '{machine}' of '{member}' in '{name}'; that key is refused from now on."), None))
        }
        Command::Revoke { name, member, machine } => {
            let admin = admin_device(config_dir)?;
            let roster = fetch_roster(&agent, base, &auth, name, &admin)?;
            let target = roster.listed.iter()
                .find(|n| n.member.as_deref() == Some(member.as_str()) && n.machine.as_deref() == Some(machine.as_str()))
                .ok_or_else(|| format!("no listed device '{machine}' of '{member}' in team '{name}'; nothing was signed"))?;
            refuse_admin_entry(&admin, &[&target.entry])?;
            let entries: Vec<Vec<u8>> = roster.list.list.devices.iter().filter(|e| **e != target.entry).cloned().collect();
            let version = publish_next(&agent, base, &auth, name, &admin, &roster, entries, &[])?;
            Ok((format!("revoked '{machine}' of '{member}' in '{name}' (list v{version}); it is out at once and its key never joins again."), None))
        }
        Command::Remove { name, member } => {
            let admin = admin_device(config_dir)?;
            let roster = fetch_roster(&agent, base, &auth, name, &admin)?;
            let theirs: Vec<&Vec<u8>> = roster.listed.iter()
                .filter(|n| n.member.as_deref() == Some(member.as_str()))
                .map(|n| &n.entry)
                .collect();
            refuse_admin_entry(&admin, &theirs)?;
            let entries: Vec<Vec<u8>> = roster.list.list.devices.iter().filter(|e| !theirs.contains(e)).cloned().collect();
            let version = publish_next(&agent, base, &auth, name, &admin, &roster, entries, &[member.as_str()])?;
            Ok((format!("removed '{member}' from '{name}' (list v{version}): {} device(s) signed out; \
                         the member is refused until invited again.", theirs.len()), None))
        }
    }
}

/// The admin's own key signs every list; a list without it would leave the team with no device
/// the admin can read from. Refused whatever name the server binds it to.
fn refuse_admin_entry(admin: &Device, entries: &[&Vec<u8>]) -> Result<(), String> {
    let own = sesync_team::membership::device_entry(admin);
    if entries.iter().any(|e| **e == own) {
        return Err("that would remove the admin's own key from the list; nothing was signed".into());
    }
    Ok(())
}

/// Run a parsed command and return only the text to print.
pub fn run(cmd: &Command, base: &str, token: &str, config_dir: &Path) -> Result<String, String> {
    run_detailed(cmd, base, token, config_dir).map(|(text, _)| text)
}

fn call(agent: &ureq::Agent, url: &str, auth: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
    match agent.post(url).set("Authorization", auth).send_json(body) {
        Ok(r) => r.into_json::<serde_json::Value>().map_err(|_| "server reply was not JSON".to_string()),
        Err(ureq::Error::Status(code, r)) => {
            let msg = r.into_json::<serde_json::Value>().ok()
                .and_then(|j| j["error"].as_str().map(str::to_string))
                .unwrap_or_default();
            Err(format!("server refused ({code}): {msg}"))
        }
        Err(_) => Err("cannot reach the team server".into()),
    }
}
