//! branchpod (`bp`): a resource runtime for git worktrees.

// Some helpers are only used by tests so far and by `bp gc`, which lands later.
#![allow(dead_code)]

mod commands;
mod config;
mod down;
mod files;
mod fsutil;
mod inuse;
mod ledger;
mod pod;
mod podlock;
mod ports;
mod up;
mod vars;
mod worktree;

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;

/// Version of the `--json` output. Part of the public API: bump on breaking changes.
const JSON_SCHEMA: u32 = 1;

/// Exit code of `bp down` when resources are left behind. 1 means teardown could not run at all;
/// 2 is taken by usage errors.
const EXIT_LEFTOVERS: u8 = 3;

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
    /// Provision a worktree: ports, files and commands from its configuration.
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
        Cmd::Up { path } => {
            let wt = worktree::Worktree::resolve(path.as_deref())?;
            let config = config::Config::load(&wt.root, wt.main_checkout.as_deref())?;
            let store = pod::Store::open()?;
            let report = up::up(&store, &wt, &config, |holder| match holder {
                Some(holder) => eprintln!("bp: waiting for {holder} to finish"),
                None => eprintln!("bp: waiting for another bp process on this worktree"),
            })?;
            println!(
                "pod {} is up ({})",
                report.pod.id,
                report.pod.worktree.display()
            );
            for item in &report.items {
                println!("  {:<8} {:<24} {}", item.kind, item.name, item.detail);
            }
        }
        Cmd::Status { path, json } => {
            let store = pod::Store::open()?;
            let (target, meta) = find_pod(&store, path.as_deref())?;
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
                            println!("  {:<8} {:<24} {}", r.kind, r.name, r.status);
                        }
                    }
                }
            }
        }
        Cmd::Down { path } => return down(path.as_deref()),
    }
    Ok(ExitCode::SUCCESS)
}

fn down(path: Option<&OsStr>) -> Result<ExitCode> {
    let store = pod::Store::open()?;
    let (target, meta) = find_pod(&store, path)?;
    let Some(meta) = meta else {
        println!("no pod for {}", target.display());
        return Ok(ExitCode::SUCCESS);
    };
    let _lock = store.lock_pod(&meta.id, "down", |holder| match holder {
        Some(h) => eprintln!("bp: pod {} is busy: waiting for {h}", meta.id),
        None => eprintln!("bp: pod {} is busy: waiting", meta.id),
    })?;
    // Whoever held the lock may have torn the pod down already.
    let Some(meta) = store.find_by_worktree(&meta.worktree)? else {
        println!("no pod for {}", target.display());
        return Ok(ExitCode::SUCCESS);
    };

    println!("pod {} ({})", meta.id, meta.worktree.display());
    let report = down::teardown(&store, &meta)?;
    for s in &report.steps {
        let outcome = if s.error.is_none() {
            "destroyed"
        } else {
            "left"
        };
        println!("  {:<9} {:<8} {}", outcome, s.kind, s.name);
    }
    for s in report.leftovers() {
        eprintln!(
            "bp: {} {}: {}",
            s.kind,
            s.name,
            s.error.as_deref().unwrap_or_default()
        );
    }
    if let Some(e) = &report.state_error {
        eprintln!("bp: {e}");
    }
    if report.is_clean() {
        println!("clean");
        Ok(ExitCode::SUCCESS)
    } else {
        let left = report.leftovers().count();
        if left > 0 {
            eprintln!("bp: {left} resource(s) left behind; fix the cause and run `bp down` again");
        }
        Ok(ExitCode::from(EXIT_LEFTOVERS))
    }
}

/// Finds the pod of `path` (default: the current directory). A recorded pod is preferred, so a
/// worktree that is already gone is still found, however the caller spells its path (recorded
/// paths are canonical); a path that does not exist has no other pod.
fn find_pod(store: &pod::Store, path: Option<&OsStr>) -> Result<(PathBuf, Option<pod::PodMeta>)> {
    let target = match path {
        Some(p) => {
            worktree::reject_placeholder(p)?;
            std::path::absolute(p)?
        }
        None => std::env::current_dir()?,
    };
    // Recorded paths are canonical; try the spelling as given too, in case one is not.
    if let Some(meta) = store.find_by_worktree(&target)? {
        return Ok((target, Some(meta)));
    }
    let target = worktree::normalize(&target)?;
    if let Some(meta) = store.find_by_worktree(&target)? {
        return Ok((target, Some(meta)));
    }
    if !target.exists() {
        return Ok((target, None));
    }
    let wt = worktree::Worktree::resolve(Some(target.as_os_str()))?;
    let meta = store.find_by_worktree(&wt.root)?;
    Ok((target, meta))
}
