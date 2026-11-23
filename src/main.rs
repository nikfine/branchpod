//! branchpod (`bp`): a resource runtime for git worktrees.

// The ledger, variables and pod store are used by `bp up` / `bp down`, which land next.
#![allow(dead_code)]

mod commands;
mod config;
mod files;
mod fsutil;
mod ledger;
mod pod;
mod ports;
mod vars;
mod worktree;

use std::ffi::OsString;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;

/// Version of the `--json` output. Part of the public API: bump on breaking changes.
const JSON_SCHEMA: u32 = 1;

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
    /// Show the pod of a worktree and the resources recorded for it.
    Status {
        /// Worktree path; defaults to the current directory.
        path: Option<OsString>,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bp: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Cmd::Config { path, json } => {
            let wt = worktree::Worktree::resolve(path.as_deref())?;
            let config = config::Config::load(&wt.root, wt.main_checkout.as_deref())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "schema": JSON_SCHEMA, "config": config })
                    )?
                );
            } else {
                print!("{}", toml::to_string(&config)?);
            }
        }
        Cmd::Status { path, json } => {
            let store = pod::Store::open()?;
            // Prefer a recorded pod: the worktree may already be gone.
            let target = match &path {
                Some(p) => std::path::absolute(p)?,
                None => std::env::current_dir()?,
            };
            worktree::reject_placeholder(target.as_os_str())?;
            let meta = match store.find_by_worktree(&target)? {
                Some(meta) => Some(meta),
                None => {
                    let wt = worktree::Worktree::resolve(Some(target.as_os_str()))?;
                    store.find_by_worktree(&wt.root)?
                }
            };
            let resources = match &meta {
                Some(m) => store.resources(&m.id)?,
                None => Vec::new(),
            };
            if json {
                let out = json!({ "schema": JSON_SCHEMA, "pod": meta, "resources": resources });
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                match &meta {
                    None => println!("no pod for {}", target.display()),
                    Some(m) => {
                        println!("pod {} ({})", m.id, m.worktree.display());
                        for r in &resources {
                            let status = serde_json::to_value(r.status)?;
                            let kind = serde_json::to_value(r.kind)?;
                            println!(
                                "  {:<8} {:<24} {}",
                                kind.as_str().unwrap_or_default(),
                                r.name,
                                status.as_str().unwrap_or_default()
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
