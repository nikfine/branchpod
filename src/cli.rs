//! The subcommands of `bp`: each runs one and prints its result.

use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

use anyhow::{Result, bail};
use serde_json::json;

use crate::ledger::{Kind, ResourceState, Status};
use crate::logs::read;
use crate::proc::{self, ProcId};
use crate::{config, console, down, fsutil, pod, services, supervisor, up, worktree};

/// Version of the `--json` output. Part of the public API: bump on breaking changes.
const JSON_SCHEMA: u32 = 1;

/// Exit code of `bp down` when resources are left behind. 1 means teardown could not run at all;
/// 2 is taken by usage errors.
const EXIT_LEFTOVERS: u8 = 3;

/// `bp config`.
pub fn config(path: Option<&OsStr>, json: bool) -> Result<ExitCode> {
    let wt = worktree::Worktree::resolve(path)?;
    let config = config::Config::load(&wt.root, wt.main_checkout.as_deref())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "schema": JSON_SCHEMA, "config": config }))?
        );
    } else {
        print!("{}", toml::to_string(&config)?);
    }
    Ok(ExitCode::SUCCESS)
}

/// `bp up`.
pub fn up(path: Option<&OsStr>) -> Result<ExitCode> {
    let wt = worktree::Worktree::resolve(path)?;
    let config = config::Config::load(&wt.root, wt.main_checkout.as_deref())?;
    let store = pod::Store::open()?;
    let launcher = supervisor::Launcher::current_exe()?;
    let report = up::up(&store, &wt, &config, &launcher, |holder| match holder {
        Some(holder) => eprintln!("bp: waiting for {holder} to finish"),
        None => eprintln!("bp: waiting for another bp process on this worktree"),
    })?;
    println!(
        "pod {} is up ({})",
        report.pod.id,
        report.pod.worktree.display()
    );
    for item in &report.items {
        println!("  {:<10} {:<24} {}", item.kind, item.name, item.detail);
    }
    for warning in &report.warnings {
        eprintln!("bp: warning: {warning}");
    }
    Ok(ExitCode::SUCCESS)
}

/// `bp status`.
pub fn status(path: Option<&OsStr>, json: bool) -> Result<ExitCode> {
    let store = pod::Store::open()?;
    let (target, meta) = find_pod(&store, path)?;
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
                    println!("  {:<10} {:<24} {}", r.kind, r.name, r.status);
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `bp down`.
pub fn down(path: Option<&OsStr>) -> Result<ExitCode> {
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
        println!("  {:<9} {:<10} {}", outcome, s.kind, s.name);
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

/// Lines `bp logs` prints unless told otherwise: enough to see a failure, without flooding the
/// context of an agent.
const LOG_LINES: u64 = 200;

/// The value of `bp logs --lines`.
#[derive(Debug, Clone, Copy)]
pub struct Lines(read::Lines);

impl FromStr for Lines {
    type Err = String;

    fn from_str(text: &str) -> Result<Lines, String> {
        match text {
            "all" => Ok(Lines(read::Lines::All)),
            _ => text
                .parse()
                .map(|n| Lines(read::Lines::Last(n)))
                .map_err(|_| "must be a number or `all`".to_owned()),
        }
    }
}

/// `bp logs`.
pub fn logs(
    service: &str,
    path: Option<&OsStr>,
    follow: bool,
    lines: Option<Lines>,
) -> Result<ExitCode> {
    match print_logs(service, path, follow, lines) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        // Whoever read our output (`| head`) has seen enough.
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe) =>
        {
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => Err(e),
    }
}

fn print_logs(
    service: &str,
    path: Option<&OsStr>,
    follow: bool,
    lines: Option<Lines>,
) -> Result<()> {
    let store = pod::Store::open()?;
    let (target, meta) = find_pod(&store, path)?;
    let Some(meta) = meta else {
        bail!("no pod for {}", target.display());
    };
    let resources = store.resources(&meta.id)?;
    let Some(resource) = find_service(&resources, service) else {
        let known: Vec<&str> = resources
            .iter()
            .filter(|r| r.kind == Kind::Service)
            .map(|r| r.name.as_str())
            .collect();
        if known.is_empty() {
            bail!("pod {} has no services", meta.id);
        }
        bail!(
            "pod {} has no service {service:?}; it has: {}",
            meta.id,
            known.join(", ")
        );
    };

    let log = services::log_path(&store.pod_dir(&meta.id), service);
    let mut out = console::Output::stdout();
    let cursor = match read::Snapshot::open(&log)? {
        Some(mut snapshot) => {
            let wanted = lines.map_or(read::Lines::Last(LOG_LINES), |l| l.0);
            let more = snapshot.tail(wanted, |bytes| out.write(bytes))?;
            if more && lines.is_none() && !follow {
                eprintln!(
                    "bp: the last {LOG_LINES} lines of service {service}; \
                     `--lines all` prints everything"
                );
            }
            snapshot.end()
        }
        None if follow && resource.status == Status::Pending => read::Cursor::start(&log),
        None => match &resource.error {
            Some(e) => bail!("service {service:?} has no log; it failed: {e}"),
            None => bail!("service {service:?} has no log"),
        },
    };
    if !follow {
        return Ok(out.finish()?);
    }
    let started = process_of(resource);
    let mut failure = None;
    if resource.status == Status::Pending && started.is_none() {
        eprintln!("bp: service {service} has not started yet; waiting");
    }

    let end = read::follow(
        cursor,
        || service_state(&store, &meta, service, started, &mut failure),
        |item| match item {
            read::Item::Data(bytes) => out.write(bytes),
            read::Item::Skipped(n) => {
                eprintln!("bp: {n} part(s) of the log were removed before they could be read");
                Ok(())
            }
        },
    )?;
    out.finish()?;
    if let Some(e) = failure {
        bail!("service {service:?} did not start: {e}");
    }
    match end {
        read::End::Exited => eprintln!("bp: service {service} is not running any more"),
        read::End::StillWritten => eprintln!(
            "bp: service {service} is not running any more, but something it started still \
             writes to its log"
        ),
        read::End::Gone => eprintln!("bp: pod {} was torn down", meta.id),
    }
    Ok(())
}

fn find_service<'a>(resources: &'a [ResourceState], name: &str) -> Option<&'a ResourceState> {
    resources
        .iter()
        .find(|r| r.kind == Kind::Service && r.name == name)
}

fn process_of(resource: &ResourceState) -> Option<ProcId> {
    serde_json::from_value::<services::Record>(resource.data.clone())
        .ok()?
        .process
}

/// What the followed service is doing now. `started` is its process when following began: with
/// another one, this is already the next run of the pod. Why it never started, if it did not,
/// goes to `failure`.
fn service_state(
    store: &pod::Store,
    meta: &pod::PodMeta,
    service: &str,
    started: Option<ProcId>,
    failure: &mut Option<String>,
) -> Result<read::State> {
    // Asked before the ledger is read: `up` holds the lock until its last record is written.
    let locked = store.is_pod_locked(&meta.id);
    let resources = match fsutil::retry_any(|| store.resources(&meta.id)) {
        Ok(resources) => resources,
        // Read in the middle of the removal of the pod's state.
        Err(_) if !store.pod_dir(&meta.id).join("pod.json").exists() => Vec::new(),
        Err(e) => return Err(e),
    };
    let Some(resource) = find_service(&resources, service) else {
        return Ok(read::State::Gone);
    };
    let process = process_of(resource);
    if started.is_some() && process != started {
        return Ok(read::State::Gone);
    }
    Ok(match (process, resource.status) {
        (Some(process), _) if proc::is_running(process) => read::State::Active,
        (None, Status::Pending) if locked => read::State::Active,
        (None, Status::Pending) => {
            *failure = Some("`bp up` ended before it recorded the start; run `bp down`".into());
            read::State::Exited
        }
        (None, Status::Failed) => {
            *failure = resource.error.clone();
            read::State::Exited
        }
        _ => read::State::Exited,
    })
}

/// `bp supervise`: the supervisor process of a pod.
pub fn supervise(endpoint: &str) -> Result<ExitCode> {
    supervisor::serve(endpoint, supervisor::TetherCheck::On)?;
    Ok(ExitCode::SUCCESS)
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
