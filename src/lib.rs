//! AI Factory v0.6: a daemon and a terminal UI that drive AI coding agents
//! against GitHub tickets.
//!
//! The modules below mirror the source layout in `docs/v0.5/SPEC.md`.
//! Later chunks fill them in.

pub mod config;
pub mod daemon;
pub mod decisions;
pub mod exec;
pub mod gates;
pub mod gh;
pub mod links;
pub mod mentions;
pub mod model;
pub mod poll;
pub mod proc;
pub mod prompts;
pub mod runner;
pub mod sched;
pub mod sock;
pub mod state;
pub mod tasks;
pub mod theory;
pub mod ticket;
pub mod trains;
pub mod tui;
pub mod worktree;
