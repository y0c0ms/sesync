//! Team sync core. See the tests for the behaviour each module must keep.
//!
//! Module groups; [AGENTS.md](../AGENTS.md) carries the rules and invariants, and
//! [docs/ARCHITECTURE.md](../docs/ARCHITECTURE.md) the design and wire formats.

// Crypto and record formats: who is in a space, what a member may seal and how.
pub mod membership;
pub mod session_keys;
pub mod signed_records;
pub mod spaces;
pub mod wrap;

// Server rules: leases, tokens, join, backup policy, metrics.
pub mod auth;
pub mod backup;
pub mod lease;
pub mod metrics;
pub mod service;
pub mod totp;

// Server I/O: the HTTP surface, the process, and the Postgres store.
pub mod http;
pub mod pg;
pub mod server;
