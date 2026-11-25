//! The subcommands of `bp`: each runs one and prints its result.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

use anyhow::{Result, bail};
use serde_json::json;

use crate::ledger::{Kind, ResourceState, Status};
use crate::logs::read;
use crate::proc::{self, ProcId};
use crate::{config, console, down, fsutil, pod, services, status, supervisor, up, worktree};

/// Version of the `--json` output. Part of the public API: bump on breaking changes.
const JSON_SCHEMA: u32 = 1;

/// Exit code of `bp down` when resources are left behind. 1 means teardown could not run at all;
/// 2 is taken by usage errors.
const EXIT_LEFTOVERS: u8 = 3;

/// Exit codes of `bp status` when the pod is not healthy. Two, because what to do differs: take a
/// degraded pod down, but wait for whoever works on a busy one.
const EXIT_DEGRADED: u8 = 4;
const EXIT_BUSY: u8 = 5;

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
        println!(
            "  {:<10} {:<24} {:>6.1}s  {}",
            item.kind,
            item.name,
            item.took.as_secs_f64(),
            item.detail
        );
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
    let report = match meta {
        Some(meta) => status::look(
            &store.pod_dir(&meta.id),
            || read_pod(&store, &meta.id),
            |sure| at_work(&store, &meta.id, sure),
        )?,
        None => None,
    };
    let Some(report) = report else {
        if json {
            let out = json!({
                "schema": JSON_SCHEMA, "pod": null, "resources": [], "health": null,
            });
            print_out(&serde_json::to_string_pretty(&out)?)?;
        } else {
            print_out(&format!("no pod for {}", target.display()))?;
        }
        return Ok(ExitCode::SUCCESS);
    };
    if json {
        let out = json!({
            "schema": JSON_SCHEMA,
            "pod": report.pod,
            "resources": report.resources,
            "health": report.health,
        });
        print_out(&serde_json::to_string_pretty(&out)?)?;
    } else {
        print_out(report.text(supervisor::now_ms()).trim_end())?;
    }
    Ok(match report.health.state {
        status::Health::Healthy => ExitCode::SUCCESS,
        status::Health::Degraded => ExitCode::from(EXIT_DEGRADED),
        status::Health::Busy => ExitCode::from(EXIT_BUSY),
    })
}

/// Prints `text` and a newline. Whoever does not want to read all of it (`| head`) need not.
fn print_out(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    match writeln!(out, "{text}").and_then(|()| out.flush()) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

/// The `bp` that works on the pod, if any. Its record is enough to look closer; without one a free
/// lock is taken for a moment only to be `sure`.
fn at_work(store: &pod::Store, id: &str, sure: bool) -> Option<String> {
    let holder = store.holder(id);
    if !(sure || holder.is_some()) || !store.is_pod_locked(id) {
        return None;
    }
    Some(holder.map_or_else(|| "another bp process".to_owned(), |h| h.to_string()))
}

/// What is recorded about pod `id`; `None` if it is torn down.
fn read_pod(store: &pod::Store, id: &str) -> Result<Option<status::Recorded>> {
    let Some(meta) = store.meta(id)? else {
        return Ok(None);
    };
    let resources = read_resources(store, &meta)?;
    // Not the pod that `meta` says any more: its state has just been removed.
    if resources.is_empty() && !is_there(store, &meta) {
        return Ok(None);
    }
    Ok(Some(status::Recorded { meta, resources }))
}

/// Whether the pod still exists.
fn is_there(store: &pod::Store, meta: &pod::PodMeta) -> bool {
    store.pod_dir(&meta.id).join("pod.json").exists()
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
    // At once, as `down` may be killed before it ends. `notice`: a panic because nobody reads stderr any
    // more would cut the teardown short.
    let report = down::teardown(&store, &meta, |name, path| {
        notice(format_args!(
            "file {name} is not as `bp up` made it; kept as {}",
            path.display()
        ));
    })?;
    for s in &report.steps {
        let outcome = match (&s.error, s.kept) {
            (Some(_), _) => "left",
            (None, false) => "destroyed",
            (None, true) => "saved",
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
    let read = || read_resources(&store, &meta);
    let up_runs = || up_runs(&store, &meta.id);
    let resources = if follow {
        await_record(service, read, up_runs)?
    } else {
        read()?
    };
    let Some(resource) = find_service(&resources, service) else {
        let known: Vec<&str> = resources
            .iter()
            .filter(|r| r.kind == Kind::Service)
            .map(|r| r.name.as_str())
            .collect();
        if known.is_empty() {
            if !is_there(&store, &meta) {
                bail!("no pod for {}", target.display());
            }
            bail!("pod {} has no services", meta.id);
        }
        bail!(
            "pod {} has no service {service:?}; it has: {}",
            meta.id,
            known.join(", ")
        );
    };

    let record = record_of(resource);
    let log = services::recorded_log(record.as_ref(), &store.pod_dir(&meta.id), service);
    let mut out = console::Output::stdout();
    let cursor = match read::Snapshot::open(&log)? {
        Some(mut snapshot) => {
            let wanted = lines.map_or(read::Lines::Last(LOG_LINES), |l| l.0);
            let more = snapshot.tail(wanted, |bytes| out.write(bytes))?;
            if more && lines.is_none() && !follow {
                notice(format_args!(
                    "the last {LOG_LINES} lines of service {service}; \
                     `--lines all` prints everything"
                ));
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
    let started = record.and_then(|r| r.process);
    let mut failure = None;
    if resource.status == Status::Pending && started.is_none() {
        notice(format_args!(
            "service {service} has not started yet; waiting"
        ));
    }

    let end = read::follow(
        cursor,
        || service_state(service, started, read, up_runs, &mut failure),
        |item| match item {
            read::Item::Data(bytes) => out.write(bytes),
            read::Item::Skipped(n) => {
                notice(format_args!(
                    "{n} part(s) of the log were removed before they could be read"
                ));
                Ok(())
            }
        },
    )?;
    out.finish()?;
    if let Some(e) = failure {
        bail!("service {service:?} did not start: {e}");
    }
    match end {
        read::End::Exited => notice(format_args!("service {service} is not running any more")),
        read::End::StillWritten => notice(format_args!(
            "service {service} is not running any more, but something it started still \
             writes to its log"
        )),
        read::End::Gone => notice(format_args!("pod {} was torn down", meta.id)),
    }
    Ok(())
}

fn find_service<'a>(resources: &'a [ResourceState], name: &str) -> Option<&'a ResourceState> {
    resources
        .iter()
        .find(|r| r.kind == Kind::Service && r.name == name)
}

fn record_of(resource: &ResourceState) -> Option<services::Record> {
    serde_json::from_value(resource.data.clone()).ok()
}

/// A line for the user on stderr. Unlike `eprintln!` it does not panic when nobody reads it any more
/// (`2>&1 | head`).
fn notice(text: std::fmt::Arguments) {
    let _ = writeln!(io::stderr(), "bp: {text}");
}

fn read_resources(store: &pod::Store, meta: &pod::PodMeta) -> Result<Vec<ResourceState>> {
    match fsutil::retry_any(|| store.resources(&meta.id)) {
        Ok(resources) => Ok(resources),
        // Read in the middle of the removal of the pod's state.
        Err(_) if !is_there(store, meta) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Whether a `bp up` of the pod is running: it holds the pod lock until its last record is written.
/// Ask only when it matters, a free lock is taken for a moment.
fn up_runs(store: &pod::Store, id: &str) -> bool {
    store.is_pod_locked(id) && store.holder(id).is_none_or(|h| h.command != "down")
}

/// The ledger, and whether a running `bp up` may still change what is `open` in it.
fn read_settled(
    open: impl Fn(&[ResourceState]) -> bool,
    read: impl Fn() -> Result<Vec<ResourceState>>,
    up_runs: impl Fn() -> bool,
) -> Result<(Vec<ResourceState>, bool)> {
    let resources = read()?;
    if !open(&resources) {
        return Ok((resources, false));
    }
    if up_runs() {
        return Ok((resources, true));
    }
    // `up` may have written its last record and ended since the ledger was read.
    Ok((read()?, false))
}

/// The ledger once it has a record of `service`, or once no `bp up` runs that could write one: `up`
/// records a service only when its turn comes, after the commands.
fn await_record(
    service: &str,
    read: impl Fn() -> Result<Vec<ResourceState>>,
    up_runs: impl Fn() -> bool,
) -> Result<Vec<ResourceState>> {
    let mut told = false;
    loop {
        let unknown = |r: &[ResourceState]| find_service(r, service).is_none();
        let (resources, wait) = read_settled(unknown, &read, &up_runs)?;
        if !wait {
            return Ok(resources);
        }
        if !told {
            notice(format_args!(
                "no service {service} yet, and `bp up` is running; waiting"
            ));
            told = true;
        }
        std::thread::sleep(read::POLL);
    }
}

/// What the followed service is doing now. `started` is its process when following began: with
/// another one, this is already the next run of the pod. Why it never started, if it did not,
/// goes to `failure`.
fn service_state(
    service: &str,
    started: Option<ProcId>,
    read: impl Fn() -> Result<Vec<ResourceState>>,
    up_runs: impl Fn() -> bool,
    failure: &mut Option<String>,
) -> Result<read::State> {
    let process_of = |r: &ResourceState| record_of(r).and_then(|r| r.process);
    let starting = |r: &[ResourceState]| {
        find_service(r, service)
            .is_some_and(|r| r.status == Status::Pending && process_of(r).is_none())
    };
    let (resources, wait) = read_settled(starting, read, up_runs)?;
    if wait {
        return Ok(read::State::Active);
    }
    let Some(resource) = find_service(&resources, service) else {
        return Ok(read::State::Gone);
    };
    let process = process_of(resource);
    if started.is_some() && process != started {
        return Ok(read::State::Gone);
    }
    Ok(match (process, resource.status) {
        (Some(process), _) if proc::is_running(process) => read::State::Active,
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

/// Finds the pod of the worktree whose root is `path`; without a path, of the worktree the current
/// directory is in. A recorded pod is preferred, so a worktree that is already gone is still found,
/// however the caller spells its path (recorded paths are canonical); a path that does not exist has
/// no other pod.
///
/// Git is not asked about a path: what `git worktree remove` leaves behind may be in no repository,
/// in a broken one, or in the main checkout, whose pod is not the one that is meant.
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
    if path.is_none() {
        let wt = worktree::Worktree::resolve(Some(target.as_os_str()))?;
        let meta = store.find_by_worktree(&wt.root)?;
        return Ok((target, meta));
    }
    // `up` takes a directory inside a worktree for that worktree. "No pod" would make whoever passes
    // the same to `down` believe that nothing is left.
    let pods = store.list()?;
    for dir in target.ancestors() {
        if pods.iter().any(|p| pod::same_path(&p.worktree, dir)) {
            bail!(
                "{} is not a worktree root; it lies inside {}, which has a pod",
                target.display(),
                dir.display()
            );
        }
        // The root of another worktree, or what is left of one: `up` goes no further either.
        if dir.join(".git").symlink_metadata().is_ok() {
            break;
        }
    }
    Ok((target, None))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn service(status: Status, process: Option<ProcId>) -> Vec<ResourceState> {
        vec![ResourceState {
            kind: Kind::Service,
            name: "web".into(),
            status,
            data: json!({ "run": ["x"], "cwd": "/wt", "process": process }),
            error: None,
        }]
    }

    /// Reads that give one answer after the other.
    fn reads(mut answers: Vec<Vec<ResourceState>>) -> impl Fn() -> Result<Vec<ResourceState>> {
        answers.reverse();
        let answers = RefCell::new(answers);
        move || Ok(answers.borrow_mut().pop().expect("one read too many"))
    }

    fn me() -> ProcId {
        proc::identify(std::process::id()).unwrap()
    }

    /// The lock would be taken for a moment before each look at the log.
    #[test]
    fn the_pod_lock_of_a_started_service_is_left_alone() {
        let gone = ProcId { start: 1, ..me() };
        for (resources, expected) in [
            (service(Status::Live, Some(me())), read::State::Active),
            (service(Status::Live, Some(gone)), read::State::Exited),
            (service(Status::Destroyed, Some(gone)), read::State::Exited),
            (service(Status::Failed, None), read::State::Exited),
            (Vec::new(), read::State::Gone),
        ] {
            let up_runs = || panic!("asked about the lock");
            let read = reads(vec![resources]);
            let state = service_state("web", None, read, up_runs, &mut None).unwrap();
            assert_eq!(state, expected);
        }
        let found = await_record("web", reads(vec![service(Status::Pending, None)]), || {
            panic!("asked about the lock")
        });
        assert_eq!(found.unwrap().len(), 1);
    }

    #[test]
    fn a_service_started_just_before_up_ended_is_running() {
        let answers = vec![
            service(Status::Pending, None),
            service(Status::Live, Some(me())),
        ];
        let mut failure = None;
        let state = service_state("web", None, reads(answers), || false, &mut failure).unwrap();
        assert_eq!((state, failure), (read::State::Active, None));

        let answers = vec![Vec::new(), service(Status::Live, Some(me()))];
        assert_eq!(
            await_record("web", reads(answers), || false).unwrap().len(),
            1
        );
    }

    #[test]
    fn a_pod_without_resources_is_still_a_pod() {
        let dir = tempfile::tempdir().unwrap();
        let store = pod::Store::at(dir.path().to_path_buf());
        assert!(read_pod(&store, "p").unwrap().is_none());
        let meta = json!({ "v": 1, "id": "p", "worktree": "/wt", "unfinished": "up" });
        let meta = serde_json::from_value(meta).unwrap();
        store.write_meta(&meta).unwrap();
        let pod = read_pod(&store, "p").unwrap().unwrap();
        assert_eq!((pod.meta, pod.resources), (meta, vec![]));
        // Its removal was interrupted before the last file.
        std::fs::write(store.ledger_path("p"), "").unwrap();
        std::fs::remove_file(store.pod_dir("p").join("pod.json")).unwrap();
        assert!(read_pod(&store, "p").unwrap().is_none());
    }

    #[test]
    fn another_process_is_another_run() {
        let before = ProcId { start: 1, ..me() };
        let read = reads(vec![service(Status::Live, Some(me()))]);
        let state = service_state("web", Some(before), read, || false, &mut None).unwrap();
        assert_eq!(state, read::State::Gone);
    }
}
