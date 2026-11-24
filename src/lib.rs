//! Internals of branchpod (`bp`): a resource runtime for git worktrees.
//!
//! Not a public API: anything here may change in any release. The contract of `bp` is its `--json`
//! output and event schema.
//!
//! The library keeps the binary thin and lets integration tests call the code directly. Only `cli` is
//! public so far; other items become public once such a test needs them. `cli::up` starts the current
//! executable as the supervisor, so only the `bp` binary may call it: a test would start itself.

#[doc(hidden)]
pub mod cli;
mod commands;
mod config;
mod console;
mod control;
mod down;
mod files;
mod fsutil;
mod inuse;
mod ledger;
mod logs;
mod pod;
mod podlock;
mod ports;
mod proc;
mod saved;
mod services;
mod supervisor;
#[cfg(test)]
pub(crate) mod test_support;
mod up;
mod vars;
mod worktree;
