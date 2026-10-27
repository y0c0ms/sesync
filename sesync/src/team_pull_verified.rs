//! The verified pull. It reads a team's sealed records, opens each one that a listed author signed
//! and a listed device can decrypt, and returns the texts for display. It writes nothing to disk;
//! a record that fails any check is dropped and counted, never shown.
//!
//! A record is also checked against the highest sequence this device has already accepted for its
//! conversation. A validly signed record older than that is a rollback: the server replayed an old
//! version of the session. It is dropped and counted as rolled back.
//!
//! The server also advertises, per conversation, the sequence its stored record carries. When that
//! advertised value is at or below the floor this device holds, the conversation is not fetched and
//! is counted as unchanged. The advertised value is never trusted for acceptance: it only decides
//! whether to fetch. A fetched record is verified in full, exactly as before.
//!
//! Importing a shown session into a harness stays a separate, confirmed action (the TUI's `i`).

use std::collections::BTreeMap;

use sesync_team::signed_records::Binding;

use crate::device_keys::DeviceFile;
use crate::sealed::{open_for_device, SealError, Trust};

/// Where the pull reads records from: the team server, or a fake in tests.
pub trait RecordSource {
    /// The space these records belong to. Every record must be bound to it.
    fn space(&self) -> &str;
    /// The conversation ids the team holds.
    fn list(&self) -> Result<Vec<String>, String>;
    /// The conversations the team holds, each with the sequence its stored record advertises, when
    /// the source advertises one. The advertised value is not verified: it only decides whether a
    /// conversation is fetched. The default advertises nothing, so every conversation is fetched.
    fn list_advertised(&self) -> Result<Vec<(String, Option<u64>)>, String> {
        Ok(self.list()?.into_iter().map(|c| (c, None)).collect())
    }
    /// The stored sealed record for a conversation, exactly as the server holds it.
    fn fetch(&self, conversation: &str) -> Result<Vec<u8>, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PulledRecord {
    pub conversation: String,
    pub text: Vec<u8>,
    /// Written by this device: its signature is this device's key (`signed_records::authorship`).
    pub mine: bool,
}

#[derive(Debug, Default, Clone)]
pub struct PullOutcome {
    /// Records that verified, ready for display.
    pub shown: Vec<PulledRecord>,
    /// Records that failed a check. Counted, never shown. Includes the rolled-back ones.
    pub dropped: usize,
    /// Records that verified but were older than the sequence already accepted for their
    /// conversation. A subset of `dropped`.
    pub rolled_back: usize,
    /// For each conversation shown in this pull, the highest sequence accepted. The caller keeps
    /// the maximum of this and what it held before, so the floor only ever rises.
    pub accepted: BTreeMap<String, u64>,
    /// Conversations not fetched because the source advertised a sequence at or below the floor this
    /// device already holds. Neither shown nor dropped: the floor already covers them.
    pub unchanged: usize,
    /// Why nothing could be listed: the server refused this device, or could not be reached. A
    /// pull that failed this way shows nothing, and says so instead of reading as an empty team.
    pub error: Option<String>,
    /// The member list this pull fetched and verified against the pinned admin key, when the
    /// server sent one at least as new as the list held. The caller keeps it
    /// (`team_watch::keep_newer_lists`), so the list held here only moves forward.
    pub list: Option<crate::sealed::Trust>,
}

/// Fetch and verify the records the team holds that may have changed. `trust` is the team's
/// admin-signed member list as this device verified it; every record must be signed by an author
/// that list names, and a wrap addressed to this device must open. Without a trust root nothing
/// verifies, so every record is dropped and counted. `last_seen` maps each conversation to the
/// highest sequence this device has already accepted; a record below it is refused as a rollback,
/// and a conversation the source advertises at or below that floor is not fetched at all (counted
/// as `unchanged`).
pub fn pull_verified(source: &dyn RecordSource, reader: &DeviceFile, trust: Option<&Trust>,
                     last_seen: &BTreeMap<String, u64>) -> PullOutcome {
    pull_with(source, reader, trust, last_seen, true)
}

/// As `pull_verified`, but every conversation is fetched and verified, including those the source
/// advertises as unchanged. For a caller that replaces its whole list with the result, which would
/// otherwise lose the skipped conversations from view.
pub fn pull_verified_all(source: &dyn RecordSource, reader: &DeviceFile, trust: Option<&Trust>,
                         last_seen: &BTreeMap<String, u64>) -> PullOutcome {
    pull_with(source, reader, trust, last_seen, false)
}

fn pull_with(source: &dyn RecordSource, reader: &DeviceFile, trust: Option<&Trust>,
             last_seen: &BTreeMap<String, u64>, skip_unchanged: bool) -> PullOutcome {
    let mut out = PullOutcome::default();
    let listing = match source.list_advertised() {
        Ok(c) => c,
        Err(e) => {
            out.error = Some(e);
            return out;
        }
    };
    for (conv, advertised) in listing {
        // Skip only with a trust root, a floor and an advertised value. Otherwise the conversation is
        // fetched as before, and with no trust root it is then dropped and counted.
        if skip_unchanged && trust.is_some() {
            if let (Some(adv), Some(floor)) = (advertised, last_seen.get(&conv)) {
                if adv <= *floor {
                    out.unchanged += 1;
                    continue;
                }
            }
        }
        let record = match source.fetch(&conv) {
            Ok(r) => r,
            Err(_) => { out.dropped += 1; continue; }
        };
        let Some(t) = trust else { out.dropped += 1; continue };
        let expect = Binding {
            space: source.space().to_string(),
            conversation: conv.clone(),
            min_sequence: last_seen.get(&conv).copied().unwrap_or(0),
        };
        match open_for_device(reader, t, &expect, &record) {
            Ok(opened) => {
                let best = out.accepted.entry(conv.clone()).or_insert(0);
                *best = (*best).max(opened.sequence);
                let mine = sesync_team::signed_records::authorship(&expect, &record)
                    .is_ok_and(|a| a.author == reader.public_signing());
                out.shown.push(PulledRecord { conversation: conv, text: opened.content, mine });
            }
            Err(SealError::Rollback { .. }) => {
                out.dropped += 1;
                out.rolled_back += 1;
            }
            Err(_) => out.dropped += 1,
        }
    }
    out
}
