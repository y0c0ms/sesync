//! The user's home directory: the root every default path (state, harness stores, team
//! credentials) is built from.
//!
//! `HOME` wins, as it always has on Unix. Windows sets `USERPROFILE` and normally no `HOME`, and
//! a daemon started at logon (the HKCU Run key) has neither a shell nor a profile to inherit one
//! from. Without this fallback every default became a relative path under the working directory.
//! On Windows a `HOME` that is not a drive or UNC path (an MSYS shell's `/c/Users/...`) is also
//! skipped, because it names nothing a Windows process can open.

use std::ffi::OsString;
use std::path::PathBuf;

/// The home directory: `HOME` when usable, else `USERPROFILE` on Windows, else empty.
pub fn home_dir() -> PathBuf {
    home_from(&|k| std::env::var_os(k), cfg!(windows))
}

/// [`home_dir`] with the environment passed in and the platform named, so the rules are testable
/// on every platform.
pub fn home_from(var: &dyn Fn(&str) -> Option<OsString>, windows: bool) -> PathBuf {
    let usable = |key: &str| var(key).filter(|v| usable_home(v, windows));
    usable("HOME")
        .or_else(|| if windows { usable("USERPROFILE") } else { None })
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Non-empty, and on Windows an absolute drive (`C:\`, `C:/`) or UNC (`\\server\share`) path.
fn usable_home(value: &std::ffi::OsStr, windows: bool) -> bool {
    if value.is_empty() {
        return false;
    }
    !windows || has_windows_prefix(&value.to_string_lossy())
}

fn has_windows_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    let unc = s.starts_with("\\\\") || s.starts_with("//");
    drive || unc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, OsString> {
        pairs.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect()
    }

    fn lookup(map: &HashMap<String, OsString>) -> impl Fn(&str) -> Option<OsString> + '_ {
        move |k| map.get(k).cloned()
    }

    #[test]
    fn unix_uses_home_only() {
        let e = env(&[("HOME", "/home/u"), ("USERPROFILE", "C:\\Users\\u")]);
        assert_eq!(home_from(&lookup(&e), false), PathBuf::from("/home/u"));
        assert_eq!(home_from(&lookup(&env(&[])), false), PathBuf::new(), "neither set: empty, as before");
        assert_eq!(home_from(&lookup(&env(&[("HOME", "")])), false), PathBuf::new(), "an empty HOME is unset");
    }

    #[test]
    fn windows_without_home_uses_userprofile() {
        // The Run-key case: no HOME at all. Before this, every default was relative to the cwd.
        let e = env(&[("USERPROFILE", "C:\\Users\\jordan")]);
        assert_eq!(home_from(&lookup(&e), true), PathBuf::from("C:\\Users\\jordan"));
    }

    #[test]
    fn windows_skips_a_home_that_is_not_a_windows_path() {
        // Git Bash sets HOME=/c/Users/jordan; a Windows process cannot use that path.
        let e = env(&[("HOME", "/c/Users/jordan"), ("USERPROFILE", "C:\\Users\\jordan")]);
        assert_eq!(home_from(&lookup(&e), true), PathBuf::from("C:\\Users\\jordan"));
        let drive = env(&[("HOME", "D:/home/jordan"), ("USERPROFILE", "C:\\Users\\jordan")]);
        assert_eq!(home_from(&lookup(&drive), true), PathBuf::from("D:/home/jordan"), "a drive path is absolute");
    }

    #[test]
    fn windows_home_wins_when_it_is_absolute() {
        let e = env(&[("HOME", "C:\\Users\\other"), ("USERPROFILE", "C:\\Users\\jordan")]);
        assert_eq!(home_from(&lookup(&e), true), PathBuf::from("C:\\Users\\other"));
        let unc = env(&[("HOME", "\\\\server\\share\\u")]);
        assert_eq!(home_from(&lookup(&unc), true), PathBuf::from("\\\\server\\share\\u"));
    }

    #[test]
    fn windows_nothing_usable_is_empty() {
        assert_eq!(home_from(&lookup(&env(&[("HOME", "/x")])), true), PathBuf::new());
        assert_eq!(home_from(&lookup(&env(&[("USERPROFILE", "relative\\dir")])), true), PathBuf::new());
    }
}
