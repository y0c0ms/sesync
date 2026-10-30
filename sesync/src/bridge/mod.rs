//! The phone bridge: a WebSocket server that speaks the part of Orca's mobile protocol the agent-hub
//! Android app needs to pair with this machine, list its sesync conversations, open their herdr panes
//! live and type prompts into them. The protocol subset, the mapping onto sesync and herdr, the
//! streams and the security notes are in docs/BRIDGE.md.
//!
//! Only `sesync bridge …` runs any of this. The daemon, the TUI and the team client never touch it.

pub mod confine;
pub mod files;
pub mod git;
pub mod pins;
#[cfg(test)]
pub mod testkit;
pub mod approvals;
pub mod catalog;
pub mod cli;
pub mod devices;
pub mod dictation;
pub mod e2ee;
pub mod hybrid;
pub mod herdr;
pub mod keys;
pub mod net;
pub mod offer;
pub mod rpc;
pub mod server;
pub mod stream;
pub mod terminals;
pub mod turns;
pub mod notify;
pub mod ntfy;
pub mod start;
pub mod transcript;
pub mod uploads;
pub mod usage;

/// The port Orca's desktop listens on, and so the port the app dials.
pub const DEFAULT_PORT: u16 = 6768;
/// The endpoint a pairing offer advertises unless `--endpoint` says otherwise: this machine's
/// WireGuard mesh address.
pub const DEFAULT_ENDPOINT: &str = "ws://10.10.20.3:6768";
/// The networks allowed to connect. A connection from anywhere else is closed before any frame
/// is read.
pub const DEFAULT_ALLOW: &str = "10.10.20.0/24,192.168.1.0/24,127.0.0.0/8";
/// The WebSocket close code Orca reads as "unauthorized" (`rpc-client-socket-session.ts`). Also
/// the answer to a v2 hello, which this bridge does not speak: the app then falls back to legacy.
pub const UNAUTHORIZED_CLOSE: u16 = 4001;

/// Create `dir` (and its parents) readable only by its owner.
pub fn ensure_owner_dir(dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    restrict(dir, 0o700)
}

/// Write `bytes` to `path` atomically, with mode 0600 from the moment the file exists. The rename
/// means a reader never sees a half-written key or device list.
pub fn write_owner_only(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        ensure_owner_dir(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = open_owner_only(&tmp)?;
    file.write_all(bytes).and_then(|_| file.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

#[cfg(unix)]
fn open_owner_only(path: &std::path::Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn open_owner_only(path: &std::path::Path) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))
}

/// Set `path` to `mode`. A no-op where the platform has no such permission bits.
#[cfg(unix)]
pub fn restrict(path: &std::path::Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("cannot restrict {}: {e}", path.display()))
}

#[cfg(not(unix))]
pub fn restrict(_path: &std::path::Path, _mode: u32) -> Result<(), String> {
    Ok(())
}
