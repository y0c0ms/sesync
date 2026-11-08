//! This machine's bridge settings: `<bridge dir>/settings.json` (by default `~/.config/sesync/bridge/settings.json`).
//! Every field is optional. A command-line flag wins over the file, and for dictation an environment variable
//! does too. With no file and no flag the bridge uses generic defaults: no pairing endpoint (`bridge pair` asks
//! for `--endpoint`), loopback and private networks allowed, no ntfy push, no dictation.
//!
//! ```json
//! { "endpoint": "ws://10.0.0.5:6768", "allow": "10.0.0.0/24,127.0.0.0/8",
//!   "ntfyUrl": "https://ntfy.example", "ntfyTopic": "agents", "ntfyTokenCommand": "cat ~/.ntfy-token",
//!   "whisperUrl": "http://10.0.0.5:8080/v1/audio/transcriptions", "whisperModel": "whisper-1", "whisperLanguage": "auto" }
//! ```

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    /// The address a pairing offer tells the phone to dial (`ws://HOST:PORT`).
    pub endpoint: Option<String>,
    /// The networks allowed to connect, comma-separated CIDRs.
    pub allow: Option<String>,
    pub ntfy_url: Option<String>,
    pub ntfy_topic: Option<String>,
    /// A shell command that prints the ntfy topic's publish token. Run once at startup; the token is never logged.
    pub ntfy_token_command: Option<String>,
    pub whisper_url: Option<String>,
    pub whisper_model: Option<String>,
    pub whisper_language: Option<String>,
}

/// The settings file of the bridge whose state lives in `dir`.
pub fn path(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

/// The settings in `dir`, or the defaults when there is no file. A file that does not parse is an error that
/// names it, so a typo never silently falls back to the defaults.
pub fn load(dir: &Path) -> Result<Settings, String> {
    let file = path(dir);
    match std::fs::read_to_string(&file) {
        Ok(text) => {
            let settings: Settings = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
            Ok(settings.trimmed())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(format!("cannot read {}: {e}", file.display())),
    }
}

impl Settings {
    /// Blank values count as absent.
    fn trimmed(self) -> Settings {
        let keep = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Settings {
            endpoint: keep(self.endpoint),
            allow: keep(self.allow),
            ntfy_url: keep(self.ntfy_url),
            ntfy_topic: keep(self.ntfy_topic),
            ntfy_token_command: keep(self.ntfy_token_command),
            whisper_url: keep(self.whisper_url),
            whisper_model: keep(self.whisper_model),
            whisper_language: keep(self.whisper_language),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sesync-bridge-settings-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn no_file_is_the_generic_defaults() {
        assert_eq!(load(&dir("none")).unwrap(), Settings::default());
    }

    #[test]
    fn a_file_sets_each_field_and_blank_values_count_as_absent() {
        let d = dir("some");
        std::fs::write(path(&d), r#"{"endpoint":" ws://10.0.0.5:6768 ","allow":"","ntfyUrl":"https://n.example"}"#).unwrap();
        let s = load(&d).unwrap();
        assert_eq!(s.endpoint.as_deref(), Some("ws://10.0.0.5:6768"));
        assert_eq!(s.allow, None);
        assert_eq!(s.ntfy_url.as_deref(), Some("https://n.example"));
    }

    #[test]
    fn a_misspelt_field_or_broken_file_is_an_error_naming_the_file() {
        let d = dir("bad");
        std::fs::write(path(&d), r#"{"endpiont":"ws://x:1"}"#).unwrap();
        let err = load(&d).unwrap_err();
        assert!(err.contains("settings.json") && err.contains("endpiont"), "{err}");
        std::fs::write(path(&d), "{").unwrap();
        assert!(load(&d).is_err());
    }
}
