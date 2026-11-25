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
    /// Show the pod of a worktree, its resources and what its services are doing now.
    ///
    /// Only looks. For a service that is not running it shows the end of its log, which may hold
    /// whatever the service printed, secrets included. Exits with 0 when the pod is healthy or there
    /// is none, 4 when it is degraded (a hint says what to do), 5 when it is busy (`bp up` or
    /// `bp down` is at work: wait), 1 when its state cannot be read.
    Status {
        /// Root of the worktree; by default the one the current directory is in.
        path: Option<OsString>,
        #[arg(long)]
        json: bool,
    },
    /// Tear down everything recorded for a worktree's pod, in reverse order.
    ///
    /// A copied file that was edited since `bp up` is moved to `saved` in the `bp` home instead of being
    /// deleted, and its new path is printed.
    ///
    /// Works when the worktree is already gone. Exits with 0 when nothing is left, 3 when some
    /// resources are left behind (run it again once their cause is fixed), 1 when teardown could
    /// not run.
    Down {
        /// Root of the worktree; by default the one the current directory is in.
        path: Option<OsString>,
    },
    /// Print the output of a service.
    ///
    /// The bytes are printed as the service wrote them, stdout and stderr together. Reading never
    /// affects the service. With --follow, ends with 0 once the service has exited or its pod is
    /// torn down.
    Logs {
        /// Name of the service.
        service: String,
        /// Root of the worktree; by default the one the current directory is in.
        path: Option<OsString>,
        /// Keep printing what the service writes.
        #[arg(short, long)]
        follow: bool,
        /// How many of the last lines to print, or `all` [default: 200].
        #[arg(short = 'n', long, value_name = "N")]
        lines: Option<cli::Lines>,
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
        Cmd::Logs {
            service,
            path,
            follow,
            lines,
        } => cli::logs(&service, path.as_deref(), follow, lines),
        Cmd::Supervise { endpoint } => cli::supervise(&endpoint),
    }
}
