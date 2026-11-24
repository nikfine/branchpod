//! The `bp` binary: argument parsing and the call into the library.

use std::ffi::OsString;
use std::process::ExitCode;

use anyhow::Result;
use branchpod::cli;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bp", version, about = "A resource runtime for git worktrees")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the effective configuration for a worktree (project file + local overrides).
    Config {
        /// Worktree path; defaults to the current directory.
        path: Option<OsString>,
        /// Print JSON instead of TOML.
        #[arg(long)]
        json: bool,
    },
    /// Provision a worktree: ports, files, commands and services from its configuration.
    Up {
        /// Worktree path; defaults to the current directory.
        path: Option<OsString>,
    },
    /// Show the pod of a worktree and the resources recorded for it.
    Status {
        /// Worktree path; defaults to the current directory.
        path: Option<OsString>,
        #[arg(long)]
        json: bool,
    },
    /// Tear down everything recorded for a worktree's pod, in reverse order.
    ///
    /// Works when the worktree is already gone. Exits with 0 when nothing is left, 3 when some
    /// resources are left behind (run it again once their cause is fixed), 1 when teardown could
    /// not run.
    Down {
        /// Worktree path; defaults to the current directory.
        path: Option<OsString>,
    },
    /// Internal: the supervisor process of a pod, started by `bp up`.
    #[command(hide = true)]
    Supervise {
        /// Control channel to listen on.
        endpoint: String,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("bp: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Cmd::Config { path, json } => cli::config(path.as_deref(), json),
        Cmd::Up { path } => cli::up(path.as_deref()),
        Cmd::Status { path, json } => cli::status(path.as_deref(), json),
        Cmd::Down { path } => cli::down(path.as_deref()),
        Cmd::Supervise { endpoint } => cli::supervise(&endpoint),
    }
}
