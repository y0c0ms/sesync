//! Rename an OpenCode session through its HTTP API (PATCH /session/{id}).
//!
//! The CLI has no rename command and the SQLite store is not written directly. The rename
//! needs an `opencode serve`:
//!   * if one is already listening on the given port, it is used and left running;
//!   * otherwise a temporary one is started, and stopped afterwards, in every case.
//! A server we did not start is never stopped. Issue #9.
//! Port of daemon/hs_manage.py _rewrite_opencode_title.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum RenameError {
    BadId(String),
    BadTitle(String),
    NotFound(String),
    Server(String),
}

impl std::fmt::Display for RenameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenameError::BadId(s) => write!(f, "refusing unexpected session id: {s}"),
            RenameError::BadTitle(s) => write!(f, "title not accepted: {s}"),
            RenameError::NotFound(s) => write!(f, "opencode session not found: {s}"),
            RenameError::Server(s) => write!(f, "opencode server: {s}"),
        }
    }
}

impl std::error::Error for RenameError {}

/// Session ids are `ses_` followed by letters and digits only.
pub fn valid_session_id(id: &str) -> bool {
    id.starts_with("ses_") && id.len() > 4 && id[4..].bytes().all(|c| c.is_ascii_alphanumeric())
}

/// Titles are trimmed, 1 to 80 characters, with no control characters.
pub fn validate_title(title: &str) -> Result<String, RenameError> {
    let t = title.trim();
    if t.is_empty() {
        return Err(RenameError::BadTitle("title is empty".into()));
    }
    if t.chars().any(|c| (c as u32) < 32 || c as u32 == 127) {
        return Err(RenameError::BadTitle("title contains control characters".into()));
    }
    if t.chars().count() > 80 {
        return Err(RenameError::BadTitle("title is longer than 80 characters".into()));
    }
    Ok(t.to_string())
}

/// A started server that is stopped on drop. Servers we did not start are never wrapped.
struct Owned(Child);

impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One HTTP/1.1 request to 127.0.0.1:<port>. Returns (status, body).
fn http(port: u16, method: &str, path: &str, body: Option<&str>) -> std::io::Result<(u16, String)> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let b = body.unwrap_or("");
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len())?;
    let mut raw = String::new();
    s.read_to_string(&mut raw)?;
    let status = raw.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Ok((status, body))
}

fn is_up(port: u16) -> bool {
    matches!(http(port, "GET", "/session", None), Ok((s, _)) if s == 200)
}

fn free_port() -> std::io::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// Rename `session_id` to `title`. `port` selects a server to reuse; `None` picks a free port
/// for a temporary server. `opencode` is the binary to start when no server answers.
pub fn rename_session(
    session_id: &str,
    title: &str,
    port: Option<u16>,
    opencode: Option<&Path>,
) -> Result<String, RenameError> {
    if !valid_session_id(session_id) {
        return Err(RenameError::BadId(session_id.to_string()));
    }
    let new = validate_title(title)?;
    let body = serde_json::json!({ "title": new }).to_string();

    // Reuse a running server; never stop it.
    if let Some(p) = port {
        if is_up(p) {
            return patch(p, session_id, &body).map(|_| new);
        }
    }

    // Otherwise start one we own, and stop it when this function returns.
    let bin: PathBuf = opencode.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("opencode"));
    let p = match port {
        Some(p) => p,
        None => free_port().map_err(|e| RenameError::Server(e.to_string()))?,
    };
    let child = Command::new(&bin)
        .args(["serve", "--port", &p.to_string(), "--hostname", "127.0.0.1"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn()
        .map_err(|e| RenameError::Server(format!("cannot start {}: {e}", bin.display())))?;
    let owned = Owned(child);

    let deadline = Instant::now() + Duration::from_secs(60);
    while !is_up(p) {
        if Instant::now() > deadline {
            return Err(RenameError::Server("did not become ready within 60s".into()));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let result = patch(p, session_id, &body).map(|_| new);
    drop(owned);
    result
}

fn patch(port: u16, session_id: &str, body: &str) -> Result<(), RenameError> {
    match http(port, "PATCH", &format!("/session/{session_id}"), Some(body)) {
        Ok((200, _)) => Ok(()),
        Ok((404, _)) => Err(RenameError::NotFound(session_id.to_string())),
        Ok((code, _)) => Err(RenameError::Server(format!("rename failed: HTTP {code}"))),
        Err(e) => Err(RenameError::Server(e.to_string())),
    }
}
