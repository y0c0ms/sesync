//! sesync daemon, Rust port. See daemon/sesync.py for the Python original.
//!
//! Module groups; [AGENTS.md](../AGENTS.md) carries the rules and invariants, and
//! [docs/ARCHITECTURE.md](../docs/ARCHITECTURE.md) the design and wire formats.

// Shared persistent state (local sources and team floors/push records).
pub mod state;

// The harnesses, one module each behind one interface; everything else asks the registry.
pub mod harness;

// Local pipeline: change detection, mirrors and locks.
pub mod archive;
pub mod archive_store;
pub mod divergence;
pub mod lock;
pub mod mirror;
pub mod mirror_cleanup;
pub mod mirror_write;
pub mod pass;
pub mod prompts;
pub mod scan;
pub mod turn;

// Manage TUI: state, key handling, rendering, dialogs.
pub mod app;
pub mod clipboard;
pub mod form;
pub mod keys;
pub mod manage;
pub mod palette;
pub mod preview;
pub mod qr;
pub mod remote;
pub mod remote_http;
pub mod session_cols;
pub mod theme;
pub mod title_worker;
pub mod tui;
pub mod ui;

// Team client: admin CLI, join, sealing, push/pull, daemon pass.
// team_conflict and team_pull are test-only today (see AGENTS.md).
pub mod device_keys;
pub mod members;
pub mod sealed;
pub mod team_actions;
pub mod team_backoff;
pub mod team_cli;
pub mod team_conflict;
pub mod team_import;
pub mod team_join;
pub mod team_pass;
pub mod team_pull;
pub mod team_pull_verified;
pub mod team_push;
pub mod team_sync;
pub mod team_watch;
