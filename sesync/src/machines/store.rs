//! The machines this one may reach: `<config>/machines.json`, one entry per name, each a pairing that the other
//! machine's bridge made: by hand (`sesync bridge pair`, then `sesync machine add`), or granted by the phone
//! (`machines.grant` there, `machines.accept` here, which also keeps the grant's device id). The device token is a
//! credential, so the file is written owner-only and atomically, as the bridge's own key files are. Writes made in
//! this process are serialised: the bridge's connections run on blocking threads at once.

use crate::bridge::offer::Offer;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The longest machine name.
const NAME_MAX: usize = 32;
/// What a machine is called when its own name leaves nothing usable.
const FALLBACK_NAME: &str = "machine";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    pub name: String,
    pub endpoint: String,
    pub device_token: String,
    pub public_key_b64: String,
    /// The id of the `machine` device the other bridge granted for this entry (`machines.grant`), so the phone can
    /// revoke it there. None for a pairing added by hand, and for a file written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_device_id: Option<String>,
    /// The address this machine dials instead of `endpoint`, set with `sesync machine endpoint`: for a machine that
    /// reaches the other one another way (for example through a local port forward). It outlives a grant that
    /// replaces the entry, since the phone only knows the address it dials itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_endpoint: Option<String>,
}

impl Machine {
    /// The address to dial: the local override when one is set, else the endpoint the pairing named.
    pub fn address(&self) -> &str {
        self.local_endpoint.as_deref().unwrap_or(&self.endpoint)
    }
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    machines: Vec<Machine>,
}

/// Serialises this process's read-modify-write of `machines.json`.
static WRITES: Mutex<()> = Mutex::new(());

fn writing() -> MutexGuard<'static, ()> {
    WRITES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every machine saved. A missing file is an empty list.
pub fn load(path: &Path) -> Result<Vec<Machine>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let file: File = serde_json::from_str(&text).map_err(|e| format!("{} is damaged: {e}", path.display()))?;
    Ok(file.machines)
}

fn save(path: &Path, machines: Vec<Machine>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&File { machines }).map_err(|e| e.to_string())?;
    crate::bridge::write_owner_only(path, text.as_bytes())
}

/// Save `machine` under its name, replacing a machine of that name. True when one was replaced. The name must be
/// one `check_name` accepts. A replaced entry's local endpoint is kept unless `machine` brings its own.
pub fn put(path: &Path, mut machine: Machine) -> Result<bool, String> {
    check_name(&machine.name)?;
    let _writing = writing();
    let mut machines = load(path)?;
    let old = machines.iter().position(|m| m.name == machine.name).map(|i| machines.remove(i));
    if machine.local_endpoint.is_none() {
        machine.local_endpoint = old.as_ref().and_then(|m| m.local_endpoint.clone());
    }
    machines.push(machine);
    save(path, machines)?;
    Ok(old.is_some())
}

/// Set (or with None, clear) the address `name` is dialled at from here; returns the updated entry.
pub fn set_local_endpoint(path: &Path, name: &str, endpoint: Option<&str>) -> Result<Machine, String> {
    if let Some(endpoint) = endpoint {
        crate::bridge::offer::check_endpoint(endpoint)?;
    }
    let _writing = writing();
    let mut machines = load(path)?;
    let machine = machines.iter_mut().find(|m| m.name == name).ok_or_else(|| format!("no machine named {name:?}"))?;
    machine.local_endpoint = endpoint.map(str::to_string);
    let updated = machine.clone();
    save(path, machines)?;
    Ok(updated)
}

/// Save `offer` under `name` as a pairing made by hand, replacing a machine of that name. True when one was replaced.
pub fn add(path: &Path, name: &str, offer: &Offer) -> Result<bool, String> {
    put(path, Machine {
        name: name.to_string(),
        endpoint: offer.endpoint.clone(),
        device_token: offer.device_token.clone(),
        public_key_b64: offer.public_key_b64.clone(),
        grant_device_id: None,
        local_endpoint: None,
    })
}

/// Forget the machine called `name` and return it, or None when no machine has that name. Its device stays paired on
/// the other machine until it is revoked there.
pub fn remove(path: &Path, name: &str) -> Result<Option<Machine>, String> {
    let _writing = writing();
    let mut machines = load(path)?;
    let Some(index) = machines.iter().position(|m| m.name == name) else {
        return Ok(None);
    };
    let removed = machines.remove(index);
    save(path, machines)?;
    Ok(Some(removed))
}

/// A machine name is what the commands, the TUI and the phone show: 1 to 32 of letters, digits, `-`, `_` and `.`.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= NAME_MAX
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!("{name:?}: a machine name is 1 to 32 letters, digits, '-', '_' or '.'"))
    }
}

/// The name a machine has for another, from the first machine's `machineName` (`status.get`). Each character outside
/// `[A-Za-z0-9._-]` becomes `-`; the result is cut to 32 characters, then its leading and trailing `-` are trimmed; a
/// name with nothing left is `machine`. The result always passes `check_name`. The phone mirrors this rule in TS.
pub fn sanitize_name(machine_name: &str) -> String {
    let mapped: String = machine_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .take(NAME_MAX)
        .collect();
    let trimmed = mapped.trim_matches('-');
    if trimmed.is_empty() {
        FALLBACK_NAME.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(endpoint: &str) -> Offer {
        Offer {
            endpoint: endpoint.into(),
            device_token: "tok".into(),
            public_key_b64: "zo060cy2M+x7cMF4FKXHbs0CloUFDTRHRboFhw5YfVk=".into(),
        }
    }

    #[test]
    fn adding_a_name_again_replaces_it_and_remove_forgets_only_that_one() {
        let dir = std::env::temp_dir().join(format!("sesync-machines-{}", std::process::id()));
        let path = dir.join("machines.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load(&path).unwrap().is_empty(), "no file is no machines");
        assert!(!add(&path, "work-pc", &offer("ws://10.0.0.4:6768")).unwrap());
        assert!(!add(&path, "laptop", &offer("ws://10.0.0.6:6768")).unwrap());
        assert!(add(&path, "work-pc", &offer("ws://10.0.0.9:6768")).unwrap(), "same name replaces");
        let names: Vec<_> = load(&path).unwrap().into_iter().map(|m| (m.name, m.endpoint)).collect();
        assert_eq!(names, [("laptop".to_string(), "ws://10.0.0.6:6768".to_string()), ("work-pc".into(), "ws://10.0.0.9:6768".into())]);
        assert_eq!(remove(&path, "laptop").unwrap().unwrap().name, "laptop");
        assert!(remove(&path, "laptop").unwrap().is_none());
        assert_eq!(load(&path).unwrap().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the tokens are readable only by their owner");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_are_short_and_plain() {
        assert!(check_name("work-pc_2.lan").is_ok());
        for bad in ["", "has space", "a/b", &"x".repeat(33)] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_grant_id_is_kept_and_an_older_file_without_one_loads() {
        let dir = std::env::temp_dir().join(format!("sesync-machines-grant-{}", std::process::id()));
        let path = dir.join("machines.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"machines":[{"name":"old","endpoint":"ws://h:6768","deviceToken":"t","publicKeyB64":"k"}]}"#).unwrap();
        assert_eq!(load(&path).unwrap()[0].grant_device_id, None, "no field is a hand-made pairing");
        put(&path, Machine {
            name: "granted".into(),
            endpoint: "ws://g:6768".into(),
            device_token: "t2".into(),
            public_key_b64: "k2".into(),
            grant_device_id: Some("0a1b2c".into()),
            local_endpoint: None,
        })
        .unwrap();
        let granted = load(&path).unwrap().into_iter().find(|m| m.name == "granted").unwrap();
        assert_eq!(granted.grant_device_id.as_deref(), Some("0a1b2c"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"grantDeviceId\": \"0a1b2c\""), "{text}");
        assert!(!text.contains("\"grantDeviceId\": null"), "a hand-made entry writes no grant field: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_local_endpoint_is_dialled_and_outlives_a_grant_that_replaces_the_entry() {
        let dir = std::env::temp_dir().join(format!("sesync-machines-local-{}", std::process::id()));
        let path = dir.join("machines.json");
        let _ = std::fs::remove_dir_all(&dir);
        add(&path, "desktop", &offer("ws://10.0.0.3:6768")).unwrap();
        assert!(set_local_endpoint(&path, "desktop", Some("http://127.0.0.1:16768")).is_err(), "not a ws address");
        assert!(set_local_endpoint(&path, "nobody", Some("ws://127.0.0.1:1")).is_err());
        let set = set_local_endpoint(&path, "desktop", Some("ws://127.0.0.1:16768")).unwrap();
        assert_eq!(set.address(), "ws://127.0.0.1:16768");
        // The phone grants again: a new token and the endpoint it dials, no override.
        put(&path, Machine {
            name: "desktop".into(),
            endpoint: "ws://10.0.0.3:6768".into(),
            device_token: "new".into(),
            public_key_b64: "k".into(),
            grant_device_id: Some("g2".into()),
            local_endpoint: None,
        })
        .unwrap();
        let after = load(&path).unwrap().remove(0);
        assert_eq!((after.device_token.as_str(), after.address()), ("new", "ws://127.0.0.1:16768"));
        let cleared = set_local_endpoint(&path, "desktop", None).unwrap();
        assert_eq!(cleared.address(), "ws://10.0.0.3:6768");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sanitised_name_always_passes_the_name_rule() {
        let cases = [
            ("work-pc", "work-pc"),
            ("  my laptop  ", "my-laptop"),
            ("héllo wörld", "h-llo-w-rld"),
            ("", "machine"),
            ("---", "machine"),
            ("ok.name_1", "ok.name_1"),
        ];
        for (raw, want) in cases {
            let got = sanitize_name(raw);
            assert_eq!(got, want, "{raw:?}");
            assert!(check_name(&got).is_ok(), "{got:?}");
        }
        let long = sanitize_name(&format!("{}-tail", "x".repeat(31)));
        assert_eq!(long, "x".repeat(31), "cut to 32, then the trailing '-' is trimmed");
        assert!(check_name(&sanitize_name(&"é".repeat(40))).is_ok());
    }
}
