//! Other machines, seen the way the phone sees them: this machine as a client of their phone bridges
//! (`sesync machine …`). It speaks the phone's protocol (hello, auth, RPCs; docs/BRIDGE.md), so it lists what
//! the phone lists and mirrors a conversation's pane in a terminal. `tab` is the TUI's Machines tab: its rows, the
//! refresh worker and the herdr attach. The phone deciding which machines see each other: agent-hub-notes/machines-plan-2026-10-30.md.

pub mod attach;
pub mod cli;
pub mod client;
pub mod store;
pub mod tab;
