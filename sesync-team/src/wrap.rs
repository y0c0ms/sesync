//! Space key wrapping. A space (its members and its content keys) is serialized and sealed
//! under the master key, which comes from the cluster's sealed secret and never reaches the
//! database. The database stores only the wrapped bytes.

use crate::spaces::{seal, Space};

#[derive(Debug, PartialEq, Eq)]
pub enum WrapError {
    Refused,
    Format,
}

// Stored data, not a name: this label is bound into every wrapped space key. Changing it would
// make every existing wrapped key fail to open, so it keeps its original value.
const AAD: &[u8] = b"harness-sync space wrap v1";

/// Seal a space under the master key. The output is opaque bytes for the database.
pub fn wrap_space(space: &Space, master: &[u8; 32]) -> Result<Vec<u8>, WrapError> {
    let plain = space.to_bytes();
    seal(master, AAD, &plain).map_err(|_| WrapError::Refused)
}

/// Open a wrapped space with the master key. A wrong key or a changed byte is refused.
pub fn unwrap_space(wrapped: &[u8], master: &[u8; 32]) -> Result<Space, WrapError> {
    let plain = open_with_aad(master, wrapped)?;
    Space::from_bytes(&plain).ok_or(WrapError::Format)
}

fn open_with_aad(master: &[u8; 32], wrapped: &[u8]) -> Result<Vec<u8>, WrapError> {
    // `open` in spaces.rs uses a fixed label; the wrap uses its own AAD, so check it here.
    crate::spaces::open_bound(master, AAD, wrapped).map_err(|_| WrapError::Refused)
}
