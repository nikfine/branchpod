//! `bp up` on a pod that is there already: starts what does not run any more, after a reboot or a
//! crash, and destroys nothing.
//!
//! Only a pod that a `bp up` finished and that is still what the config says is resumed
//! ([`plan`]); for any other one `up` asks for `bp down` first. Files and ports stay as they are.
//! Whether a command's resource runs is unknown, so its `start` is run every time, and one without
//! it is left alone. Services are started again under their supervisor, or under a new one if it is
//! gone; those that run are not touched.
//!
//! Interrupted or failed at any point, what is recorded can be torn down, and resumed again.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::{Config, FileMode};
use crate::ledger::{Kind, ResourceState, Status};
use crate::pod::{PodMeta, Store, Unfinished};
use crate::supervisor::{self, Launcher};
use crate::up::{self, Item, Journal, Outcome, Report};
use crate::vars::Vars;
use crate::worktree::Worktree;
use crate::{commands, control, down, ports, proc, services, status};

/// Why a pod cannot be resumed and has to be taken down first.
#[derive(Debug)]
struct Mismatch(String);

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Mismatch {}

fn mismatch(reason: impl Into<String>) -> anyhow::Error {
    Mismatch(reason.into()).into()
}

/// What to do about a command of the pod.
#[derive(Debug)]
enum Start {
    Run(Vec<String>),
    /// It has something to destroy, but no `start`.
    Unknown,
    /// It created nothing that could run.
    Nothing,
}

#[derive(Debug)]
struct Plan {
    ports: Vec<(String, u16)>,
    commands: Vec<(String, Start)>,
    services: Vec<Service>,
}

#[derive(Debug)]
struct Service {
    plan: services::Plan,
    resource: ResourceState,
    record: services::Record,
}

/// Brings pod `id` up again. `resources` are those that are not destroyed. The caller holds the pod
/// lock.
pub(crate) fn resume(
    store: &Store,
    wt: &Worktree,
    config: &Config,
    launcher: &Launcher,
    id: &str,
    resources: &[ResourceState],
) -> Result<Report> {
    let Some(meta) = store.meta(id)? else {
        bail!("pod {id} has resources recorded, but its pod.json is missing");
    };
    let vars = up::builtin_vars(&meta.id, wt);
    let plan = match plan(config, wt, vars, &store.pod_dir(&meta.id), &meta, resources) {
        Ok(plan) => plan,
        Err(e) if e.is::<Mismatch>() => bail!(
            "pod {} already has resources recorded, and {e}:\n{}run `bp down {}` first",
            meta.id,
            up::listing(resources),
            up::quote(&wt.root)
        ),
        Err(e) => return Err(e),
    };

    let supervised = !plan.services.is_empty();
    let supervision = supervised
        .then(|| Supervision::find(&meta, resources))
        .transpose()?;
    // Before anything is started.
    if supervision.as_ref().is_some_and(|s| !s.alive) {
        launcher.preflight().context("cannot run services")?;
    }

    let mut report = Report {
        outcome: Outcome::AlreadyUp,
        pod: meta,
        items: Vec::new(),
        warnings: Vec::new(),
    };
    if let Err(e) = start(store, wt, launcher, plan, supervision, &mut report) {
        let path = up::quote(&wt.root);
        return Err(e.context(format!(
            "pod {} is not up again; fix the cause and run `bp up {path}` once more, or \
             `bp down {path}` to remove it",
            report.pod.id
        )));
    }
    Ok(report)
}

/// Compares what is recorded with the config and the disk, and renders what is to be started. Fails
/// with [`Mismatch`] if the pod is not what the config describes. Nothing is changed.
fn plan(
    config: &Config,
    wt: &Worktree,
    mut vars: Vars,
    pod_dir: &Path,
    meta: &PodMeta,
    resources: &[ResourceState],
) -> Result<Plan> {
    match meta.unfinished {
        None => {}
        Some(Unfinished::Up) => return Err(mismatch("`bp up` did not finish")),
        Some(Unfinished::Down) => return Err(mismatch("`bp down` did not finish")),
        Some(Unfinished::Other) => return Err(mismatch("another `bp` command did not finish")),
    }
    for r in resources {
        // Services and their supervisor can be started again, whatever came of them.
        let startable = matches!(r.kind, Kind::Service | Kind::Supervisor);
        if r.status != Status::Live && !startable {
            return Err(mismatch(format!("{} {:?} is {}", r.kind, r.name, r.status)));
        }
    }
    same_worktree(meta, wt)?;
    same_names(config, resources)?;

    let find = |kind, name: &str| {
        let found = resources.iter().find(|r| r.kind == kind && r.name == name);
        found.expect("every name of the config is recorded")
    };
    let mut ports = Vec::new();
    for (name, spec) in &config.port {
        let ports::Record { port } = down::data(find(Kind::Port, name))?;
        let [first, last] = spec.range;
        if !(first..=last).contains(&port) {
            let reason = format!("port {name:?} is {port}, which is not in its range any more");
            return Err(mismatch(reason));
        }
        vars.set(format!("port.{name}"), port.to_string());
        ports.push((name.clone(), port));
    }

    let mut commands = Vec::new();
    for spec in &config.command {
        let record: commands::Record = down::data(find(Kind::Command, &spec.name))?;
        let now = commands::plan(spec, &wt.root, &vars, pod_dir)?.record;
        if (&now.create, &now.file) != (&record.create, &record.file) {
            let reason = format!("command {:?} is not the one that was run", spec.name);
            return Err(mismatch(reason));
        }
        commands::export(&spec.name, &record, &mut vars);
        let start = match (&spec.start, &record.destroy) {
            (Some(_), _) => Start::Run(
                commands::render_start(spec, &vars)
                    .with_context(|| format!("command {:?}", spec.name))?,
            ),
            (None, Some(_)) => Start::Unknown,
            (None, None) => Start::Nothing,
        };
        commands.push((spec.name.clone(), start));
    }

    let mut services = Vec::new();
    for spec in &config.service {
        let resource = find(Kind::Service, &spec.name).clone();
        let record: services::Record = down::data(&resource)?;
        let plan = services::plan(spec, &wt.root, &vars, pod_dir)?;
        if (&plan.record.run, &plan.record.cwd) != (&record.run, &record.cwd) {
            let reason = format!("service {:?} is not the one that was started", spec.name);
            return Err(mismatch(reason));
        }
        services.push(Service {
            plan,
            resource,
            record,
        });
    }
    Ok(Plan {
        ports,
        commands,
        services,
    })
}

/// Whether this is the worktree the pod was created for, not a new one at the same path.
fn same_worktree(meta: &PodMeta, wt: &Worktree) -> Result<()> {
    match &meta.mark {
        None => Err(mismatch(
            "it was created by a version of bp that did not mark its worktree",
        )),
        Some(mark) if mark.is_on(wt)? => Ok(()),
        Some(_) => Err(mismatch(
            "the worktree does not have the mark of the pod: it is probably a new one at the same path",
        )),
    }
}

/// Whether the config has the resources that are recorded and no others.
fn same_names(config: &Config, resources: &[ResourceState]) -> Result<()> {
    let files = || config.file.iter().filter(|f| f.mode != FileMode::Ignore);
    let mut configured = BTreeSet::new();
    configured.extend(config.port.keys().map(|name| (Kind::Port, name.clone())));
    configured.extend(files().map(|f| (Kind::File, f.name())));
    configured.extend(
        config
            .command
            .iter()
            .map(|c| (Kind::Command, c.name.clone())),
    );
    configured.extend(
        config
            .service
            .iter()
            .map(|s| (Kind::Service, s.name.clone())),
    );
    // Not recorded if it had no source.
    let optional: BTreeSet<_> = files().filter(|f| f.optional).map(|f| f.name()).collect();
    let recorded: BTreeSet<_> = resources
        .iter()
        .filter(|r| r.kind != Kind::Supervisor)
        .map(|r| (r.kind, r.name.clone()))
        .collect();

    if let Some((kind, name)) = recorded.difference(&configured).next() {
        return Err(mismatch(format!(
            "{kind} {name:?} is not in the config any more"
        )));
    }
    let new = configured
        .difference(&recorded)
        .find(|(kind, name)| *kind != Kind::File || !optional.contains(name));
    match new {
        Some((kind, name)) => Err(mismatch(format!(
            "{kind} {name:?} is in the config but was not created"
        ))),
        None => Ok(()),
    }
}

/// What has become of the supervisor of a pod with services.
struct Supervision {
    /// As recorded.
    record: supervisor::Record,
    observed: status::Supervisor,
    /// Otherwise it is gone.
    alive: bool,
}

impl Supervision {
    /// Fails if it neither works nor is gone.
    fn find(meta: &PodMeta, resources: &[ResourceState]) -> Result<Supervision> {
        use status::Supervisor::{Gone, NotStarted, Running};
        let record = status::supervisor_record(resources);
        let observed = status::observe(record.as_ref());
        let process = record.as_ref().and_then(|r| r.process);
        let alive = match observed {
            // One whose process was not recorded could not be waited for by `bp down`.
            Running(_) if process.is_some() => Some(true),
            Gone | NotStarted => Some(false),
            _ => None,
        };
        match (record, alive) {
            (Some(record), Some(alive)) => Ok(Supervision {
                record,
                observed,
                alive,
            }),
            _ => bail!(
                "the supervisor of pod {} cannot be used as it is: {}",
                meta.id,
                status::down_hint(meta, status::hang(&observed, process))
            ),
        }
    }

    /// Whether the service runs, and all that is needed to stop it is recorded.
    fn runs(&self, service: &Service) -> bool {
        let runtime = status::service_runtime(&service.resource, &self.observed, proc::is_running);
        self.alive
            && service.resource.status == Status::Live
            && runtime.state == status::State::Running
    }
}

/// Starts what does not run.
fn start(
    store: &Store,
    wt: &Worktree,
    launcher: &Launcher,
    plan: Plan,
    supervision: Option<Supervision>,
    report: &mut Report,
) -> Result<()> {
    let mut item = |kind, name: &str, detail: String| {
        report.items.push(Item {
            kind,
            name: name.to_owned(),
            detail,
        });
    };
    for (name, port) in &plan.ports {
        item(Kind::Port, name, port.to_string());
    }
    for (name, start) in &plan.commands {
        match start {
            Start::Run(argv) => {
                commands::start(name, argv, &wt.root)?;
                item(Kind::Command, name, "started".into());
            }
            Start::Unknown => {
                item(Kind::Command, name, "left as it is (no `start`)".into());
            }
            Start::Nothing => {}
        }
    }

    let Some(supervision) = supervision else {
        return Ok(());
    };
    let (running, stopped): (Vec<_>, Vec<_>) =
        plan.services.iter().partition(|s| supervision.runs(s));
    for service in running {
        item(
            Kind::Service,
            &service.resource.name,
            "already running".into(),
        );
    }
    if supervision.alive && stopped.is_empty() {
        return Ok(());
    }

    report.outcome = Outcome::Restarted;
    let id = &report.pod.id;
    let mut journal = Journal::open(store, id)?;
    let endpoint = if supervision.alive {
        supervision.record.endpoint.clone()
    } else {
        control::remove_stale(&supervision.record.endpoint)?;
        up::start_supervisor(
            store,
            id,
            launcher,
            &mut journal,
            &mut report.items,
            &mut report.warnings,
        )?
    };
    let supervisor = supervision.alive.then_some(&supervision.record);
    for service in stopped {
        // What is left of it: children, or all of it if it outlived its supervisor (Unix).
        let name = &service.resource.name;
        down::stop_service(supervisor, name, &service.record)
            .with_context(|| format!("service {name:?}"))?;
        up::start_service(&endpoint, &service.plan, &mut journal, &mut report.items)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::ledger::{Event, Ledger};
    use crate::pod;
    use crate::proc::ProcId;
    use crate::test_support::{Reaper, fail, git_ok, long_running, reap_pod, wait_for_log};
    use crate::up::tests::{Repo, echo, repo, resources, run};

    /// A command line that adds to the file `started` of the worktree.
    fn note() -> Vec<String> {
        let script = "echo x>>started".to_owned();
        if cfg!(windows) {
            vec!["cmd".into(), "/d".into(), "/c".into(), script]
        } else {
            vec!["sh".into(), "-c".into(), script]
        }
    }

    fn toml(argv: &[String]) -> String {
        serde_json::to_string(argv).unwrap()
    }

    /// Ports, files and a command with `start`, and the services `names`.
    fn config(names: &[&str]) -> String {
        let mut text = format!(
            "version = 1\n\
             [port.web]\nrange = [41000, 41999]\n\
             [[file]]\npath = '.env'\nmode = 'template'\n\
             [[file]]\npath = 'absent'\nmode = 'copy'\noptional = true\n\
             [[command]]\nname = 'app'\ncreate = {}\nstart = {}\ndestroy = {}\n",
            echo("id-{{port.web}}"),
            toml(&note()),
            echo("gone"),
        );
        for name in names {
            let run = toml(&long_running(&format!("{name}-up")));
            text += &format!("[[service]]\nname = '{name}'\nrun = {run}\n");
        }
        text
    }

    fn setup() -> Repo {
        let r = repo();
        fs::write(r.main.join(".env"), "URL={{port.web}}\n").unwrap();
        r
    }

    fn id(r: &Repo) -> String {
        pod::pod_id(&r.linked)
    }

    /// Brings the pod up; what is recorded by then is killed at the end of the test.
    fn up(r: &Repo, config: &str) -> (Result<Report>, Reaper) {
        let report = run(r, config);
        (report, reap_pod(&r.store, &id(r)))
    }

    fn process(r: &Repo, name: &str) -> ProcId {
        let state = resources(r);
        let found = state
            .iter()
            .rfind(|s| s.name == name && matches!(s.kind, Kind::Service | Kind::Supervisor));
        serde_json::from_value(found.unwrap().data["process"].clone()).unwrap()
    }

    fn wait_for_service(r: &Repo, name: &str) {
        let log = services::log_path(&r.store.pod_dir(&id(r)), name);
        wait_for_log(&log, &format!("{name}-up"));
    }

    fn ledger(r: &Repo) -> String {
        fs::read_to_string(r.store.ledger_path(&id(r))).unwrap()
    }

    /// How many times `start` of the command was run.
    fn starts(r: &Repo) -> usize {
        fs::read_to_string(r.linked.join("started")).map_or(0, |t| t.lines().count())
    }

    /// Kills a supervisor and its service, which outlives it on Unix.
    fn end(processes: [ProcId; 2]) {
        for process in processes {
            proc::kill(process).unwrap();
            assert!(proc::wait_gone(process, Duration::from_secs(10)));
        }
    }

    fn down(r: &Repo) {
        let meta = r.store.find_by_worktree(&r.linked).unwrap().unwrap();
        let report = down::teardown(&r.store, &meta, |_, _| ()).unwrap();
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn a_pod_whose_processes_are_gone_is_brought_up_again() {
        let r = setup();
        let config = config(&["web"]);
        let (first, _reaper) = up(&r, &config);
        let first = first.unwrap();
        assert_eq!((first.outcome, starts(&r)), (Outcome::Created, 0));
        let (sup, web) = (process(&r, "supervisor"), process(&r, "web"));
        // From then on it writes nothing, so on Unix it outlives its supervisor.
        wait_for_service(&r, "web");
        proc::kill(sup).unwrap();
        assert!(proc::wait_gone(sup, Duration::from_secs(10)));
        // Nothing is created anew: what was edited stays.
        fs::write(r.linked.join(".env"), "edited").unwrap();

        let (again, _reaper) = up(&r, &config);
        let again = again.unwrap();
        assert_eq!((again.outcome, starts(&r)), (Outcome::Restarted, 1));
        assert!(!proc::is_running(web), "it was stopped first");
        let (new_sup, new_web) = (process(&r, "supervisor"), process(&r, "web"));
        assert!(new_sup != sup && proc::is_running(new_sup));
        assert!(new_web != web && proc::is_running(new_web));
        assert_eq!(fs::read_to_string(r.linked.join(".env")).unwrap(), "edited");
        let item = |kind: Kind, report: &Report| {
            let found = report.items.iter().find(|i| i.kind == kind);
            found.unwrap().detail.clone()
        };
        assert_eq!(item(Kind::Port, &again), item(Kind::Port, &first));
        assert!(resources(&r).iter().all(|s| s.status == Status::Live));
        assert_eq!(again.pod.unfinished, None);

        down(&r);
        assert!(!proc::is_running(new_web) && !proc::is_running(new_sup));
    }

    #[test]
    fn a_healthy_pod_is_not_touched() {
        let r = setup();
        let config = config(&["web"]);
        let (first, _reaper) = up(&r, &config);
        first.unwrap();
        wait_for_service(&r, "web");
        let before = (ledger(&r), process(&r, "supervisor"), process(&r, "web"));
        let report = run(&r, &config).unwrap();
        assert_eq!(report.outcome, Outcome::AlreadyUp);
        let after = (ledger(&r), process(&r, "supervisor"), process(&r, "web"));
        assert_eq!(after, before);
        assert!(proc::is_running(before.1) && proc::is_running(before.2));
        // Whether its container runs is not known.
        assert_eq!(starts(&r), 1);
        down(&r);
    }

    #[test]
    fn only_the_service_that_does_not_run_is_started() {
        let r = setup();
        let config = config(&["one", "two"]);
        let (first, _reaper) = up(&r, &config);
        first.unwrap();
        wait_for_service(&r, "one");
        wait_for_service(&r, "two");
        let (sup, one, two) = (
            process(&r, "supervisor"),
            process(&r, "one"),
            process(&r, "two"),
        );
        proc::kill(one).unwrap();
        // Until the supervisor has noticed.
        let endpoint = status::supervisor_record(&resources(&r)).unwrap().endpoint;
        let deadline = Instant::now() + Duration::from_secs(10);
        while supervisor::services_within(&endpoint, Duration::from_secs(5))
            .unwrap()
            .iter()
            .all(|s| s.ended.is_none())
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(25));
        }

        let (again, _reaper) = up(&r, &config);
        assert_eq!(again.unwrap().outcome, Outcome::Restarted);
        assert_eq!((process(&r, "supervisor"), process(&r, "two")), (sup, two));
        assert!(proc::is_running(two));
        assert!(process(&r, "one") != one && proc::is_running(process(&r, "one")));
        down(&r);
    }

    /// At each record of a restart: what was started by then, or after it, is there.
    #[test]
    fn an_interrupted_restart_is_resumed_or_torn_down() {
        for lost in 1..=3 {
            let r = setup();
            let config = config(&["web"]);
            let (first, _reaper) = up(&r, &config);
            first.unwrap();
            let old = [process(&r, "supervisor"), process(&r, "web")];
            wait_for_service(&r, "web");
            end(old);
            let (again, _reaper) = up(&r, &config);
            again.unwrap();
            let started = [process(&r, "web"), process(&r, "supervisor")];

            let text = ledger(&r);
            let lines: Vec<&str> = text.lines().collect();
            let kept = lines[..lines.len() - lost].join("\n") + "\n";
            fs::write(r.store.ledger_path(&id(&r)), kept).unwrap();

            let (report, _reaper) = up(&r, &config);
            if lost == 3 {
                // The supervisor answers, but its process was not recorded.
                let err = format!("{:#}", report.unwrap_err());
                assert!(
                    err.contains("cannot be used") && err.contains("bp down"),
                    "{err}"
                );
            } else {
                assert_eq!(report.unwrap().outcome, Outcome::Restarted);
                assert!(proc::is_running(process(&r, "web")));
                assert!(!proc::is_running(started[0]), "two of them");
                assert_eq!(process(&r, "supervisor"), started[1]);
            }
            down(&r);
            // One whose process was not recorded is not waited for.
            for process in started {
                assert!(proc::wait_gone(process, Duration::from_secs(10)));
            }
        }
    }

    #[test]
    fn a_failed_start_starts_no_services_and_can_be_repeated() {
        let r = setup();
        let good = config(&["web"]);
        let (first, _reaper) = up(&r, &good);
        first.unwrap();
        wait_for_service(&r, "web");
        end([process(&r, "supervisor"), process(&r, "web")]);
        let before = ledger(&r);
        let bad = good.replace(&toml(&note()), &toml(&fail(3, "boom")));
        assert_ne!(bad, good);
        let err = format!("{:#}", run(&r, &bad).unwrap_err());
        assert!(
            err.contains("\"app\": start") && err.contains("boom"),
            "{err}"
        );
        assert!(err.contains("bp up") && err.contains("bp down"), "{err}");
        assert_eq!(ledger(&r), before);

        // `start` is taken from the config, and is no reason to take the pod down.
        let (again, _reaper) = up(&r, &good);
        assert_eq!(again.unwrap().outcome, Outcome::Restarted);

        // So can a service that was not started: no reason either, whatever its status.
        let mut journal = Ledger::open(&r.store.ledger_path(&id(&r))).unwrap();
        let error = "no".to_owned();
        let failed = Event::Failed { error };
        journal.record(Kind::Service, "web", failed).unwrap();
        drop(journal);
        let (again, _reaper) = up(&r, &good);
        assert_eq!(again.unwrap().outcome, Outcome::Restarted);
        down(&r);
    }

    /// `git worktree remove` takes the mark along.
    #[test]
    fn a_new_worktree_at_the_same_path_is_not_the_pod() {
        let r = setup();
        let config = config(&["web"]);
        let (first, _reaper) = up(&r, &config);
        first.unwrap();
        wait_for_service(&r, "web");
        let sup = process(&r, "supervisor");
        end([sup, process(&r, "web")]);
        let before = ledger(&r);
        let linked = r.linked.to_str().unwrap();
        git_ok(&r.main, &["worktree", "remove", "--force", linked]);
        git_ok(&r.main, &["worktree", "add", "-q", linked, "feature"]);
        // Whatever is there.
        fs::write(r.linked.join(".env"), "mine").unwrap();

        let err = format!("{:#}", run(&r, &config).unwrap_err());
        assert!(
            err.contains("already has resources recorded") && err.contains("new one"),
            "{err}"
        );
        assert_eq!((ledger(&r), starts(&r)), (before, 0));
        assert_eq!(process(&r, "supervisor"), sup);
        down(&r);
    }

    #[test]
    fn the_mark_goes_with_the_pod() {
        let r = setup();
        let config = config(&[]);
        let git_dir = r
            .main
            .join(".git/worktrees")
            .join(r.linked.file_name().unwrap());
        let marked = || git_dir.join(".branchpod").exists();
        run(&r, &config).unwrap();
        assert!(marked());
        run(&r, &config).unwrap();
        let meta = r.store.meta(&id(&r)).unwrap().unwrap();
        down(&r);
        assert!(!marked());

        // Interrupted before anything was recorded, and once more: one mark, known to the pod.
        for _ in 0..2 {
            fs::remove_file(r.linked.join(".env")).ok();
            r.store
                .write_meta(&meta.with(Some(Unfinished::Up)))
                .unwrap();
            meta.mark.as_ref().unwrap().write().unwrap();
            run(&r, &config).unwrap();
            run(&r, &config).unwrap();
            down(&r);
            assert!(!marked());
        }

        // From a version that did not mark worktrees.
        run(&r, &config).unwrap();
        let mut meta = r.store.meta(&id(&r)).unwrap().unwrap();
        let mark = meta.mark.take();
        r.store.write_meta(&meta).unwrap();
        let err = format!("{:#}", run(&r, &config).unwrap_err());
        assert!(err.contains("did not mark"), "{err}");
        meta.mark = mark;
        r.store.write_meta(&meta).unwrap();
        // The worktree is gone.
        let linked = r.linked.to_str().unwrap();
        git_ok(&r.main, &["worktree", "remove", "--force", linked]);
        down(&r);
        assert!(!git_dir.exists());
    }

    /// Nothing is started for a pod that is not what the config says, or not complete.
    #[test]
    fn what_does_not_match_has_to_be_taken_down_first() {
        let r = setup();
        let good = config(&[]);
        run(&r, &good).unwrap();
        let before = ledger(&r);
        let refused = |config: &str, reason: &str| {
            let started = starts(&r);
            let err = format!("{:#}", run(&r, config).unwrap_err());
            assert!(
                err.contains("already has resources recorded")
                    && err.contains(reason)
                    && err.contains("bp down"),
                "{reason}: {err}"
            );
            assert_eq!(starts(&r), started, "{reason}");
        };
        let service = format!(
            "[[service]]\nname = 'web'\nrun = {}\n",
            toml(&long_running("x"))
        );
        for (config, reason) in [
            (
                good.replace("[41000, 41999]", "[42000, 42001]"),
                "port \"web\" is 41",
            ),
            (
                format!("{good}[port.more]\nrange = [41000, 41999]\n"),
                "port \"more\" is in the config but was not created",
            ),
            (
                good.replace("path = '.env'", "path = '.env2'"),
                "file \".env\" is not in the config any more",
            ),
            (
                format!("{good}[[file]]\npath = 'more'\nmode = 'copy'\n"),
                "file \"more\" is in the config",
            ),
            (
                good.replace("id-{{port.web}}", "other-{{port.web}}"),
                "command \"app\" is not the one",
            ),
            (
                format!("{good}{service}"),
                "service \"web\" is in the config",
            ),
        ] {
            assert_ne!(config, good);
            refused(&config, reason);
        }
        // Only `start` and `destroy` differ, or an optional file that was skipped.
        let other = good.replace(&echo("gone"), &echo("other"));
        assert_eq!(run(&r, &other).unwrap().outcome, Outcome::AlreadyUp);
        // A mistake in the config is no reason to take the pod down.
        let typo = good.replace("id-{{port.web}}", "id-{{port.wbe}}");
        let err = format!("{:#}", run(&r, &typo).unwrap_err());
        assert!(
            err.contains("port.wbe") && !err.contains("bp down"),
            "{err}"
        );

        let meta = r.store.meta(&id(&r)).unwrap().unwrap();
        for (unfinished, reason) in [
            (Unfinished::Up, "`bp up` did not finish"),
            (Unfinished::Down, "`bp down` did not finish"),
        ] {
            r.store.write_meta(&meta.with(Some(unfinished))).unwrap();
            refused(&good, reason);
        }
        r.store.write_meta(&meta).unwrap();
        assert_eq!(ledger(&r), before);

        let mut journal = Ledger::open(&r.store.ledger_path(&id(&r))).unwrap();
        let error = "no".to_owned();
        let failed = Event::Failed { error };
        journal.record(Kind::Command, "app", failed).unwrap();
        refused(&good, "command \"app\" is failed");
    }

    #[test]
    fn a_service_that_changed_is_not_started() {
        let r = setup();
        let good = config(&["web"]);
        let (first, _reaper) = up(&r, &good);
        first.unwrap();
        let web = process(&r, "web");
        let other = good.replace("web-up", "web-other");
        let err = format!("{:#}", run(&r, &other).unwrap_err());
        assert!(err.contains("service \"web\" is not the one"), "{err}");
        assert!(proc::is_running(web));
        down(&r);
    }

    #[test]
    fn a_command_without_start_is_left_alone_and_said_so() {
        let r = setup();
        let start = format!("start = {}\n", toml(&note()));
        let text = config(&[]).replace(&start, "") + "[[command]]\nname = 'value'\ncreate = ";
        let text = text + &echo("v") + "\n";
        run(&r, &text).unwrap();
        let report = run(&r, &text).unwrap();
        let items: Vec<_> = report
            .items
            .iter()
            .filter(|i| i.kind == Kind::Command)
            .map(|i| (i.name.as_str(), i.detail.as_str()))
            .collect();
        assert_eq!(items, [("app", "left as it is (no `start`)")]);
        assert!(report.warnings.is_empty());
    }
}
