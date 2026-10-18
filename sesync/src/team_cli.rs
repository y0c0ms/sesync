//! `sesync team ...`: the admin commands for teams, run on the operator's machine.
//!
//!   sesync team create <name> [--member <m>]...   create a team; prints the authenticator URI once
//!   sesync team invite <name> <member>           add a member to an existing team
//!   sesync team publish <name>                   sign and publish the current device list
//!
//! Creating a team generates an admin key on this machine and signs the first member list with
//! it; the server pins the key and can never forge a list. `publish` fetches the enrolled
//! devices from the server and signs a newer list over them — the only way a joining device ever
//! becomes a member, and the way a device is removed (it stops appearing in the fetched set).
//!
//! Settings come from the environment, never the command line, so nothing secret is in shell
//! history or the process list:
//!   SESYNC_TEAM_URL          base URL of the team server, e.g. http://127.0.0.1:30500
//!   SESYNC_ADMIN_TOKEN_FILE  path to the admin token file (mode 600, owner only)
//!
//! The admin token is read from its file and sent only in the Authorization header. It is
//! never printed, logged or echoed in an error.

use std::path::Path;

use sesync_team::membership::{Device, MemberList};

use crate::device_keys::DeviceFile;

#[derive(Debug, PartialEq)]
pub enum Command {
    Create { name: String, members: Vec<String> },
    Invite { name: String, member: String },
    Publish { name: String },
    Forget { name: String, machine: String },
}

/// Parse the arguments after `team`. Returns a usage message on any mistake.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let usage = "usage: sesync team create <name> [--member <m>]... | sesync team invite <name> <member> | sesync team publish <name> | sesync team forget <name> <machine>";
    match args.first().map(String::as_str) {
        Some("create") => {
            let name = args.get(1).filter(|n| !n.starts_with("--")).ok_or(usage)?.clone();
            let mut members = Vec::new();
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--member" {
                    let m = args.get(i + 1).ok_or(usage)?.clone();
                    members.push(m);
                    i += 2;
                } else {
                    return Err(usage.into());
                }
            }
            Ok(Command::Create { name, members })
        }
        Some("invite") => {
            let name = args.get(1).ok_or(usage)?.clone();
            let member = args.get(2).ok_or(usage)?.clone();
            if args.len() != 3 { return Err(usage.into()); }
            Ok(Command::Invite { name, member })
        }
        Some("publish") => {
            let name = args.get(1).ok_or(usage)?.clone();
            if args.len() != 2 { return Err(usage.into()); }
            Ok(Command::Publish { name })
        }
        Some("forget") => {
            let name = args.get(1).ok_or(usage)?.clone();
            let machine = args.get(2).ok_or(usage)?.clone();
            if args.len() != 3 { return Err(usage.into()); }
            Ok(Command::Forget { name, machine })
        }
        _ => Err(usage.into()),
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
/// never leaves the machine; `publish` reads it to sign the next list.
pub fn admin_key_path(config_dir: &Path) -> std::path::PathBuf {
    config_dir.join("admin").join("device.key")
}

fn device_of(file: &DeviceFile) -> Result<Device, String> {
    Device::from_keys(file.signing_pkcs8(), *file.box_secret())
        .map_err(|_| "the admin key on this machine is unreadable".to_string())
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
                 The admin key for this machine is in {}; keep it there. After members join,\n\
                 run `sesync team publish {name}` to sign them into the member list.\n",
                admin_key_path(config_dir).display());
            Ok((text, Some(uri)))
        }
        Command::Invite { name, member } => {
            let body = serde_json::json!({"space": name, "member": member});
            call(&agent, &format!("{base}/admin/invite"), &auth, body)?;
            Ok((format!("invited '{member}' to team '{name}'."), None))
        }
        Command::Publish { name } => {
            let path = admin_key_path(config_dir);
            let admin = DeviceFile::read(&path).map_err(|_| {
                "no admin key on this machine; create the team here first".to_string()
            })?;
            let admin_dev = device_of(&admin)?;
            let reply = call(&agent, &format!("{base}/admin/devices"), &auth,
                serde_json::json!({"space": name}))?;
            let version = reply["version"].as_u64().ok_or("server gave no list version")?;
            let mut entries = vec![[admin.public_signing(), admin.public_box()].concat()];
            let mut machines = 0usize;
            for d in reply["devices"].as_array().map(Vec::as_slice).unwrap_or_default() {
                if let Some(entry) = d["entry"].as_str().and_then(unhex) {
                    entries.push(entry);
                    machines += 1;
                }
            }
            let signed = MemberList::new(version + 1, entries).sign(&admin_dev);
            let reply = call(&agent, &format!("{base}/admin/list"), &auth,
                serde_json::json!({"space": name, "signed_list": hex(&signed.to_bytes())}))?;
            let published = reply["version"].as_u64().unwrap_or(version + 1);
            Ok((format!("published list v{published} for '{name}': {machines} enrolled device(s)."), None))
        }
        Command::Forget { name, machine } => {
            let reply = call(&agent, &format!("{base}/admin/forget"), &auth,
                serde_json::json!({"space": name, "machine": machine}))?;
            let _ = reply;
            Ok((format!("forgot '{machine}' in '{name}'; it is out at once. Run `sesync team publish {name}` to sign it out of the member list."), None))
        }
    }
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
