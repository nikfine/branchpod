//! Internals of branchpod (`bp`): a resource runtime for git worktrees.
//!
//! Not a public API. The library exists so that the binary stays thin and integration tests can call
//! the code directly; anything in it may change in any release. The contract of `bp` is its `--json`
//! output and event schema.

#[doc(hidden)]
pub mod cli;
mod commands;
mod config;
mod control;
mod down;
mod files;
mod fsutil;
mod inuse;
mod ledger;
mod pod;
mod podlock;
mod ports;
mod proc;
mod services;
mod supervisor;
#[cfg(test)]
pub(crate) mod test_support;
mod up;
mod vars;
mod worktree;
