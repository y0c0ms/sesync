//! sesync daemon, Rust port. See daemon/sesync.py for the Python original.
//!
//! Module groups; [AGENTS.md](../AGENTS.md) carries the rules and invariants, and
//! [docs/ARCHITECTURE.md](../docs/ARCHITECTURE.md) the design and wire formats.

// Shared persistent state (local sources and team floors/push records).
pub mod state;

// Local pipeline: discovery, change detection, mirrors and locks.
pub mod archive;
pub mod archive_store;
pub mod claude_source;
pub mod discovery;
pub mod divergence;
pub mod install;
pub mod lock;
pub mod mirror;
pub mod mirror_cleanup;
pub mod mirror_write;
pub mod opencode;
pub mod opencode_rename;
pub mod pass;
pub mod scan;
pub mod titles;

// Manage TUI: state, key handling, rendering, dialogs.
pub mod app;
pub mod clipboard;
pub mod form;
pub mod keys;
pub mod manage;
pub mod palette;
pub mod qr;
pub mod remote;
pub mod remote_http;
pub mod session_cols;
pub mod theme;
pub mod tui;
pub mod ui;

// Team client: admin CLI, join, sealing, push/pull, daemon pass.
// team_conflict and team_pull are test-only today (see AGENTS.md).
pub mod device_keys;
pub mod sealed;
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
