//! Cross-machine turn lease.
//!
//! One lease per conversation. It is held by one machine, expires `LEASE_SECS` after the
//! holder's last heartbeat (or acquire), and every new grant increments a version. A version is
//! never granted twice for a conversation, even after a release. A write carries the version it
//! was made under; a write under an older version is refused, so a machine that slept through its
//! lease cannot overwrite the newer holder's work.
//! Times are seconds since the epoch, passed in, so the rules are testable.

use std::collections::HashMap;

/// A turn with no heartbeat for this long is over. 30 minutes, as agreed.
pub const LEASE_SECS: u64 = 30 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub conversation: String,
    pub holder: String,
    pub version: u64,
    pub last_seen: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LeaseError {
    /// Another machine holds a live lease.
    Held { holder: String, until: u64 },
    /// No lease is held by this machine.
    NotHolder,
    /// A write was made under a version that has since been superseded.
    Stale { current: u64, presented: u64 },
    /// No lease exists for the conversation.
    Missing,
}

#[derive(Default)]
pub struct LeaseTable {
    leases: HashMap<String, Lease>,
    /// The highest version ever granted per conversation. It outlives a release, so a version is
    /// never granted twice: a push that takes and releases the lease each time still gets a higher
    /// version, and a record's sequence only rises.
    granted: HashMap<String, u64>,
}

impl LeaseTable {
    fn live(l: &Lease, now: u64) -> bool {
        now < l.last_seen + LEASE_SECS
    }

    /// Take the conversation for `machine`. Refused while another machine's lease is live.
    /// A takeover of an expired lease, or a new lease, increments the version.
    pub fn acquire(&mut self, conversation: &str, machine: &str, now: u64) -> Result<Lease, LeaseError> {
        if let Some(cur) = self.leases.get(conversation) {
            if cur.holder != machine && Self::live(cur, now) {
                return Err(LeaseError::Held { holder: cur.holder.clone(), until: cur.last_seen + LEASE_SECS });
            }
            if cur.holder == machine && Self::live(cur, now) {
                // Re-acquire by the same machine within its own lease is a renewal.
                return self.heartbeat(conversation, machine, now);
            }
        }
        let version = self.granted.get(conversation).map_or(1, |v| v + 1);
        self.granted.insert(conversation.to_string(), version);
        let lease = Lease {
            conversation: conversation.to_string(),
            holder: machine.to_string(),
            version,
            last_seen: now,
        };
        self.leases.insert(conversation.to_string(), lease.clone());
        Ok(lease)
    }

    /// Renew the holder's lease: the expiry moves forward from `now`.
    pub fn heartbeat(&mut self, conversation: &str, machine: &str, now: u64) -> Result<Lease, LeaseError> {
        let cur = self.leases.get_mut(conversation).ok_or(LeaseError::Missing)?;
        if cur.holder != machine {
            return Err(LeaseError::NotHolder);
        }
        cur.last_seen = now;
        Ok(cur.clone())
    }

    /// Release the holder's lease at once. Only the holder can release.
    pub fn release(&mut self, conversation: &str, machine: &str) -> Result<(), LeaseError> {
        let cur = self.leases.get(conversation).ok_or(LeaseError::Missing)?;
        if cur.holder != machine {
            return Err(LeaseError::NotHolder);
        }
        self.leases.remove(conversation);
        Ok(())
    }

    /// Check a write made under `version` by `machine`. Refused if the version is stale, or
    /// the machine is no longer the holder.
    /// Whether this table has granted a version for the conversation before (live or released).
    pub fn has_grant(&self, conversation: &str) -> bool {
        self.granted.contains_key(conversation)
    }

    /// Continue a conversation's versions from `floor`, the highest version already written for it.
    /// Used after a restart, when the table has forgotten its grants: the next grant is then above
    /// anything written before. Never lowers a version already granted.
    pub fn seed(&mut self, conversation: &str, floor: u64) {
        let cur = self.granted.get(conversation).copied().unwrap_or(0);
        if floor > cur {
            self.granted.insert(conversation.to_string(), floor);
        }
    }

    /// The version of the live lease on a conversation, if one is held.
    pub fn current_version(&self, conversation: &str) -> Option<u64> {
        self.leases.get(conversation).map(|l| l.version)
    }

    pub fn check_write(&self, conversation: &str, version: u64, machine: &str) -> Result<(), LeaseError> {
        let cur = self.leases.get(conversation).ok_or(LeaseError::Missing)?;
        if version < cur.version {
            return Err(LeaseError::Stale { current: cur.version, presented: version });
        }
        if cur.holder != machine {
            return Err(LeaseError::NotHolder);
        }
        Ok(())
    }
}
