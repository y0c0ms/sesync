//! The machines this one may reach: `<config>/machines.json`, one entry per name, each a pairing that the
//! other machine's bridge made (`sesync bridge pair`). The device token is a credential, so the file is
//! written owner-only and atomically, as the bridge's own key files are.

use crate::bridge::offer::Offer;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    pub name: String,
    pub endpoint: String,
    pub device_token: String,
    pub public_key_b64: String,
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    machines: Vec<Machine>,
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

/// Save `offer` under `name`, replacing a machine of that name. True when one was replaced.
pub fn add(path: &Path, name: &str, offer: &Offer) -> Result<bool, String> {
    check_name(name)?;
    let mut machines = load(path)?;
    let before = machines.len();
    machines.retain(|m| m.name != name);
    let replaced = machines.len() != before;
    machines.push(Machine {
        name: name.to_string(),
        endpoint: offer.endpoint.clone(),
        device_token: offer.device_token.clone(),
        public_key_b64: offer.public_key_b64.clone(),
    });
    save(path, machines)?;
    Ok(replaced)
}

/// Forget `name`; returns what was removed. Its device stays paired on the other machine until revoked there.
pub fn remove(path: &Path, name: &str) -> Result<Machine, String> {
    let mut machines = load(path)?;
    let index = machines.iter().position(|m| m.name == name).ok_or_else(|| format!("no machine named {name:?}"))?;
    let removed = machines.remove(index);
    save(path, machines)?;
    Ok(removed)
}

/// A name is what the commands and the TUI show: 1 to 32 of letters, digits, `-`, `_` and `.`.
fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!("{name:?}: a machine name is 1 to 32 letters, digits, '-', '_' or '.'"))
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
        assert!(!add(&path, "work-pc", &offer("ws://10.10.20.4:6768")).unwrap());
        assert!(!add(&path, "laptop", &offer("ws://10.10.20.6:6768")).unwrap());
        assert!(add(&path, "work-pc", &offer("ws://10.10.20.9:6768")).unwrap(), "same name replaces");
        let names: Vec<_> = load(&path).unwrap().into_iter().map(|m| (m.name, m.endpoint)).collect();
        assert_eq!(names, [("laptop".to_string(), "ws://10.10.20.6:6768".to_string()), ("work-pc".into(), "ws://10.10.20.9:6768".into())]);
        assert_eq!(remove(&path, "laptop").unwrap().name, "laptop");
        assert!(remove(&path, "laptop").is_err());
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
}
