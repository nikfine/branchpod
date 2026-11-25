//! `bp status`: what the ledger records, and what has become of the supervisor and the services.
//!
//! Only looks: nothing is started, stopped or recorded. The supervisor knows its services best, so
//! it is asked first, but not waited for longer than [`ANSWER_WAIT`]; what it cannot tell comes from
//! the recorded processes ([`proc::is_running`]).
//!
//! A pod that a `bp up` or `bp down` is working on is neither healthy nor broken but busy. That is
//! also why what looks broken is looked at again: the `bp down` that took it apart may have ended in
//! between.

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::ledger::{Kind, ResourceState, Status};
use crate::logs::read::Snapshot;
use crate::pod::{PodMeta, Unfinished};
use crate::proc::{self, ProcId};
use crate::supervisor::{self, Exit, ServiceStatus};
use crate::{down, services, up};

/// How long the supervisor gets to answer. Whoever asks for the status wants to know that it hangs.
const ANSWER_WAIT: Duration = Duration::from_secs(3);

/// How often to look again at a pod that keeps changing.
const LOOKS: usize = 3;

/// How much of the log of a service that is not running is shown.
const TAIL_LINES: usize = 10;
const TAIL_BYTES: u64 = 4096;

/// The state of a supervisor or a service. Part of the public `--json` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Running,
    /// Supervisor: its process runs, but it does not answer.
    Unresponsive,
    /// Service: it exited, as its supervisor saw.
    Exited,
    /// Service: its process runs, but not under the pod's supervisor any more.
    Orphaned,
    /// Its process does not run, and nobody knows how it ended.
    Gone,
    /// No process is recorded for it.
    NotStarted,
    /// Its record cannot be read.
    Unknown,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Unresponsive => "unresponsive",
            State::Exited => "exited",
            State::Orphaned => "orphaned",
            State::Gone => "gone",
            State::NotStarted => "not started",
            State::Unknown => "unknown",
        }
    }
}

/// A resource as the ledger records it and, if it is a process, as it is.
#[derive(Debug, Serialize)]
pub(crate) struct View {
    #[serde(flatten)]
    resource: ResourceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<Runtime>,
}

#[derive(Debug, PartialEq, Serialize)]
pub(crate) struct Runtime {
    pub(crate) state: State,
    /// Not known for a service that a supervisor of an older version saw exit.
    #[serde(skip_serializing_if = "Option::is_none")]
    exit: Option<Exit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exited_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Of a service that is not running.
    #[serde(skip_serializing_if = "Option::is_none")]
    log: Option<Log>,
}

impl From<State> for Runtime {
    fn from(state: State) -> Runtime {
        Runtime {
            state,
            exit: None,
            exited_at_ms: None,
            error: None,
            log: None,
        }
    }
}

#[derive(Debug, PartialEq, Serialize)]
struct Log {
    path: PathBuf,
    /// The last lines.
    tail: Vec<String>,
    /// There is more than `tail`.
    truncated: bool,
    /// Some bytes are not UTF-8 and were replaced.
    lossy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Health {
    Healthy,
    /// Something is not as `bp up` left it.
    Degraded,
    /// A `bp up` or `bp down` is at work.
    Busy,
}

#[derive(Debug, Serialize)]
pub(crate) struct Summary {
    pub(crate) state: Health,
    /// Empty also when all that is wrong is that the pod is `unfinished`.
    problems: Vec<Problem>,
    /// What to do next.
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

#[derive(Debug, PartialEq, Serialize)]
struct Problem {
    kind: Kind,
    name: String,
    /// The [`State`] of a process that is not running, otherwise the status in the ledger.
    #[serde(flatten)]
    is: Is,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Is {
    State(State),
    Status(Status),
}

/// What is recorded about a pod.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Recorded {
    pub(crate) meta: PodMeta,
    pub(crate) resources: Vec<ResourceState>,
}

/// The status of a pod.
#[derive(Debug)]
pub(crate) struct Report {
    pub(crate) pod: PodMeta,
    pub(crate) resources: Vec<View>,
    pub(crate) health: Summary,
}

/// Looks at a pod whose state is in `pod_dir`. `None` if it is torn down meanwhile. `read` reads
/// what is recorded, with `None` for a pod that is gone. `at_work` names the `bp up` or `bp down` that runs, if
/// any; it is asked to make sure (`true`) if it matters.
pub(crate) fn look(
    pod_dir: &Path,
    read: impl Fn() -> Result<Option<Recorded>>,
    at_work: impl Fn(bool) -> Option<String>,
) -> Result<Option<Report>> {
    look_with(pod_dir, read, at_work, observe)
}

fn look_with(
    pod_dir: &Path,
    read: impl Fn() -> Result<Option<Recorded>>,
    at_work: impl Fn(bool) -> Option<String>,
    observe: impl Fn(Option<&supervisor::Record>) -> Supervisor,
) -> Result<Option<Report>> {
    let Some(mut recorded) = read()? else {
        return Ok(None);
    };
    for attempt in 1.. {
        let record = supervisor_record(&recorded.resources);
        let supervisor = observe(record.as_ref());
        let mut views = assess(&recorded.resources, &supervisor, proc::is_running);
        let problems = problems(&views);
        let good = problems.is_empty() && recorded.meta.unfinished.is_none();
        let work = at_work(!good);
        if work.is_none() && !good && attempt < LOOKS {
            let Some(now) = read()? else {
                return Ok(None);
            };
            if now != recorded {
                recorded = now;
                continue;
            }
        }
        add_logs(&mut views, pod_dir);
        let pod = recorded.meta;
        let hang = hang(&supervisor, record.and_then(|r| r.process));
        let (state, hint) = match work {
            Some(work) => (Health::Busy, Some(busy_hint(&work, &pod, hang))),
            None if good => (Health::Healthy, None),
            None if pod.unfinished.is_none() && startable(&problems) => {
                (Health::Degraded, Some(up_hint(&pod)))
            }
            None => (Health::Degraded, Some(down_hint(&pod, hang))),
        };
        return Ok(Some(Report {
            pod,
            resources: views,
            health: Summary {
                state,
                problems,
                hint,
            },
        }));
    }
    unreachable!("the last attempt returns")
}

/// What was found out about the supervisor.
#[derive(Debug)]
pub(crate) enum Supervisor {
    /// It answered; its services.
    Running(Vec<ServiceStatus>),
    Unresponsive {
        error: String,
        /// It took the request and did not answer: whatever asks it next may wait without end.
        hangs: bool,
    },
    Gone,
    NotStarted,
    Unknown,
}

pub(crate) fn supervisor_record(resources: &[ResourceState]) -> Option<supervisor::Record> {
    let resource = resources
        .iter()
        .rfind(|r| r.kind == Kind::Supervisor && r.status != Status::Destroyed)?;
    record(resource).ok()
}

fn record<T: serde::de::DeserializeOwned>(resource: &ResourceState) -> Result<T> {
    serde_json::from_value(resource.data.clone()).context("unreadable ledger data")
}

pub(crate) fn observe(record: Option<&supervisor::Record>) -> Supervisor {
    let Some(record) = record else {
        return Supervisor::Unknown;
    };
    let error = match supervisor::services_within(&record.endpoint, ANSWER_WAIT) {
        Ok(services) => return Supervisor::Running(services),
        Err(e) => e,
    };
    let hangs = error.downcast_ref::<supervisor::NoAnswer>().is_some();
    match record.process {
        None if supervisor::is_unreachable(&error) => return Supervisor::NotStarted,
        None => {}
        // One that is being killed stops answering before it is gone.
        Some(process) => {
            let wait = if hangs {
                Duration::ZERO
            } else {
                down::DYING_WAIT
            };
            if proc::wait_gone(process, wait) {
                return Supervisor::Gone;
            }
        }
    }
    Supervisor::Unresponsive {
        error: format!("{error:#}"),
        hangs,
    }
}

fn assess(
    resources: &[ResourceState],
    supervisor: &Supervisor,
    alive: impl Fn(ProcId) -> bool,
) -> Vec<View> {
    resources
        .iter()
        .map(|r| View {
            runtime: match r.kind {
                _ if r.status == Status::Destroyed => None,
                Kind::Supervisor => Some(supervisor_runtime(supervisor)),
                Kind::Service => Some(service_runtime(r, supervisor, &alive)),
                _ => None,
            },
            resource: r.clone(),
        })
        .collect()
}

fn supervisor_runtime(supervisor: &Supervisor) -> Runtime {
    match supervisor {
        Supervisor::Running(_) => State::Running.into(),
        Supervisor::Unresponsive { error, .. } => Runtime {
            error: Some(error.clone()),
            ..State::Unresponsive.into()
        },
        Supervisor::Gone => State::Gone.into(),
        Supervisor::NotStarted => State::NotStarted.into(),
        Supervisor::Unknown => State::Unknown.into(),
    }
}

pub(crate) fn service_runtime(
    resource: &ResourceState,
    supervisor: &Supervisor,
    alive: impl Fn(ProcId) -> bool,
) -> Runtime {
    let record: services::Record = match record(resource) {
        Ok(record) => record,
        Err(e) => {
            return Runtime {
                error: Some(format!("{e:#}")),
                ..State::Unknown.into()
            };
        }
    };
    // Without a recorded process (`bp up` was interrupted) any of that name is the one.
    if let Supervisor::Running(services) = supervisor
        && let Some(service) = services
            .iter()
            .find(|s| s.name == resource.name && record.process.is_none_or(|p| p == s.process))
    {
        return match (service.ended, &service.exit) {
            (Some(ended), _) => Runtime {
                exit: Some(ended.exit),
                exited_at_ms: Some(ended.at_ms),
                ..State::Exited.into()
            },
            (None, Some(_)) => State::Exited.into(),
            (None, None) => State::Running.into(),
        };
    }
    let Some(process) = record.process else {
        return State::NotStarted.into();
    };
    match supervisor {
        _ if !alive(process) => State::Gone.into(),
        Supervisor::Unresponsive { .. } => State::Running.into(),
        _ => State::Orphaned.into(),
    }
}

fn problems(views: &[View]) -> Vec<Problem> {
    views
        .iter()
        .filter_map(|view| {
            let r = &view.resource;
            let is = match (&view.runtime, r.status) {
                (Some(runtime), _) if runtime.state != State::Running => Is::State(runtime.state),
                // Destroyed ones may be of an earlier run. A `bp down` that did not finish is told
                // by the pod itself.
                (_, Status::Live | Status::Destroyed) => return None,
                (_, status) => Is::Status(status),
            };
            Some(Problem {
                kind: r.kind,
                name: r.name.clone(),
                is,
            })
        })
        .collect()
}

/// What to do about a supervisor that hangs: `bp down` waits for its answer, however long that takes.
pub(crate) fn hang(supervisor: &Supervisor, process: Option<ProcId>) -> Option<String> {
    let Supervisor::Unresponsive { hangs: true, .. } = supervisor else {
        return None;
    };
    let which = match process {
        Some(process) => format!("process {}", process.pid),
        None => "its process".to_owned(),
    };
    Some(format!(
        "the supervisor does not answer; if that does not change, end {which}"
    ))
}

/// Whether `bp up` can deal with the problems of a pod that is not unfinished.
fn startable(problems: &[Problem]) -> bool {
    problems.iter().all(|p| match (p.kind, &p.is) {
        (Kind::Service, is) => *is != Is::State(State::Unknown),
        (Kind::Supervisor, is) => matches!(is, Is::State(State::Gone | State::NotStarted)),
        _ => false,
    })
}

/// The next step for a pod of which only processes do not run. With the path: the caller may be in
/// another worktree.
fn up_hint(pod: &PodMeta) -> String {
    format!("run `bp up {}`", up::quote(&pod.worktree))
}

/// The same for a pod that has to be taken down. Two commands, not `a && b`, which not every shell
/// has.
pub(crate) fn down_hint(pod: &PodMeta, hang: Option<String>) -> String {
    let path = up::quote(&pod.worktree);
    let (reason, again) = match pod.unfinished {
        Some(Unfinished::Up) => ("`bp up` did not finish; ", ""),
        Some(Unfinished::Down) => ("`bp down` did not finish; ", " again"),
        Some(Unfinished::Other) => ("another `bp` command did not finish; ", ""),
        None => ("", ""),
    };
    let steps = format!("run `bp down {path}`{again}, then `bp up {path}`");
    match hang {
        Some(hang) => format!("{hang}, then {steps}"),
        None => format!("{reason}{steps}"),
    }
}

/// The same for a pod that `work` is at.
fn busy_hint(work: &str, pod: &PodMeta, hang: Option<String>) -> String {
    let path = up::quote(&pod.worktree);
    let wait = format!("{work} is at work; wait for it to end, then run `bp status {path}` again");
    match hang {
        // It may be the very thing that is waited for.
        Some(hang) => format!("{wait}; {hang}"),
        None => wait,
    }
}

fn add_logs(views: &mut [View], pod_dir: &Path) {
    for view in views {
        let Some(runtime) = &mut view.runtime else {
            continue;
        };
        if view.resource.kind != Kind::Service || runtime.state == State::Running {
            continue;
        }
        let record = record(&view.resource).ok();
        let path = services::recorded_log(record.as_ref(), pod_dir, &view.resource.name);
        runtime.log = read_log(path);
    }
}

/// `None` if there is none.
fn read_log(path: PathBuf) -> Option<Log> {
    let read = match Snapshot::open(&path) {
        Ok(None) => return None,
        Ok(Some(mut snapshot)) => snapshot.last_bytes(TAIL_BYTES),
        Err(e) => Err(e),
    };
    Some(match read {
        Ok((bytes, more)) => {
            let (tail, truncated, lossy) = last_lines(&bytes, more);
            Log {
                path,
                tail,
                truncated,
                lossy,
                error: None,
            }
        }
        Err(e) => Log {
            path,
            tail: Vec::new(),
            truncated: false,
            lossy: false,
            error: Some(e.to_string()),
        },
    })
}

/// The last lines of `bytes`, with `more` before them; whether these are not all, and whether some
/// of it is not UTF-8.
fn last_lines(mut bytes: &[u8], more: bool) -> (Vec<String>, bool, bool) {
    if more {
        // The rest of a character that was cut in two.
        let cut = bytes
            .iter()
            .take(3)
            .take_while(|&&b| b & 0b1100_0000 == 0b1000_0000)
            .count();
        bytes = &bytes[cut..];
    }
    let text = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if bytes.is_empty() {
        return (Vec::new(), more, false);
    }
    let lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
    let skip = lines.len().saturating_sub(TAIL_LINES);
    let mut lossy = false;
    let tail = lines[skip..]
        .iter()
        .map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            lossy |= std::str::from_utf8(line).is_err();
            String::from_utf8_lossy(line).into_owned()
        })
        .collect();
    (tail, more || skip > 0, lossy)
}

impl Report {
    /// For people: a line per resource, the summary, and the end of the logs of services that do not
    /// run. `now` is in milliseconds since the Unix epoch.
    pub(crate) fn text(&self, now: u64) -> String {
        let mut out = format!("pod {} ({})\n", self.pod.id, self.pod.worktree.display());
        for view in &self.resources {
            let r = &view.resource;
            let is = describe(view, now);
            let _ = writeln!(out, "  {:<10} {:<24} {is}", r.kind, r.name);
        }
        let state = match self.health.state {
            Health::Healthy => "healthy",
            Health::Degraded => "degraded",
            Health::Busy => "busy",
        };
        let _ = match &self.health.hint {
            Some(hint) => writeln!(out, "{state}: {hint}"),
            None => writeln!(out, "{state}"),
        };
        for view in &self.resources {
            let Some(log) = view.runtime.as_ref().and_then(|r| r.log.as_ref()) else {
                continue;
            };
            let (name, path) = (&view.resource.name, log.path.display());
            let _ = match &log.error {
                Some(e) => writeln!(out, "\nservice {name}: cannot read {path}: {}", plain(e)),
                None if log.tail.is_empty() => writeln!(out, "\nservice {name}: {path} is empty"),
                None if log.truncated => writeln!(out, "\nservice {name}: the end of {path}"),
                None => writeln!(out, "\nservice {name}: {path}"),
            };
            for line in &log.tail {
                let _ = writeln!(out, "  | {}", plain(line));
            }
        }
        out
    }
}

fn describe(view: &View, now: u64) -> String {
    let resource = &view.resource;
    let mut error = resource.error.as_ref();
    let mut text = match (&view.runtime, resource.status) {
        (None, status) => status.to_string(),
        (Some(runtime), Status::Live) => runtime.state.as_str().to_owned(),
        (Some(runtime), status) => format!("{status}, {}", runtime.state.as_str()),
    };
    if let Some(runtime) = &view.runtime {
        if let Some(exit) = runtime.exit {
            let _ = write!(text, " ({exit})");
        }
        if let Some(at) = runtime.exited_at_ms {
            let _ = write!(text, " {} ago", age(now.saturating_sub(at) / 1000));
        }
        error = runtime.error.as_ref().or(error);
    }
    if let Some(e) = error {
        let _ = write!(text, ": {}", plain(e.lines().next().unwrap_or_default()));
    }
    text
}

fn age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

/// `text` without what a terminal would obey.
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' => c,
            c if c.is_control() => char::REPLACEMENT_CHARACTER,
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::time::Instant;

    use serde_json::{Value, json};

    use super::*;
    use crate::control::{self, Server};
    use crate::supervisor::{Ended, Request, Start};
    use crate::test_support::{Reaper, launcher, long_running, short_running, wait_for_log};

    const P: ProcId = ProcId { pid: 7, start: 1 };

    fn resource(kind: Kind, name: &str, status: Status, data: Value) -> ResourceState {
        ResourceState {
            kind,
            name: name.into(),
            status,
            data,
            error: None,
        }
    }

    fn web(status: Status, process: Option<ProcId>) -> ResourceState {
        let data = json!({ "run": ["x"], "cwd": "/wt", "process": process });
        resource(Kind::Service, "web", status, data)
    }

    fn port(status: Status) -> ResourceState {
        resource(Kind::Port, "web", status, Value::Null)
    }

    fn meta(unfinished: Option<Unfinished>) -> PodMeta {
        PodMeta {
            v: 1,
            id: "p-1".into(),
            worktree: "/wt".into(),
            main_checkout: None,
            unfinished,
            mark: None,
        }
    }

    fn recorded(resources: Vec<ResourceState>) -> Recorded {
        Recorded {
            meta: meta(None),
            resources,
        }
    }

    fn unresponsive() -> Supervisor {
        Supervisor::Unresponsive {
            error: "no answer".into(),
            hangs: true,
        }
    }

    fn answer(process: ProcId, ended: Option<Ended>, exit: Option<&str>) -> Supervisor {
        Supervisor::Running(vec![ServiceStatus {
            name: "web".into(),
            process,
            ended,
            exit: exit.map(str::to_owned),
        }])
    }

    #[test]
    fn a_service_is_what_its_supervisor_says_or_else_what_its_process_is() {
        let ended = Ended {
            exit: Exit::Code(1),
            at_ms: 5,
        };
        let other = ProcId { start: 2, ..P };
        for (supervisor, alive, expected) in [
            (answer(P, None, None), true, State::Running),
            // The supervisor knows better (#84).
            (answer(P, None, None), false, State::Running),
            (answer(P, Some(ended), None), false, State::Exited),
            (answer(P, None, Some("exit code: 1")), false, State::Exited),
            // It runs another process of that name.
            (answer(other, None, None), true, State::Orphaned),
            (answer(other, None, None), false, State::Gone),
            (unresponsive(), true, State::Running),
            (unresponsive(), false, State::Gone),
            (Supervisor::Gone, true, State::Orphaned),
            (Supervisor::Gone, false, State::Gone),
        ] {
            let case = format!("{supervisor:?} {alive}");
            let views = assess(&[web(Status::Live, Some(P))], &supervisor, |p| {
                assert_eq!(p, P);
                alive
            });
            let runtime = views[0].runtime.as_ref().unwrap();
            assert_eq!(runtime.state, expected, "{case}");
        }
        let views = assess(
            &[web(Status::Live, Some(P))],
            &answer(P, Some(ended), None),
            |_| false,
        );
        assert_eq!(
            serde_json::to_value(&views[0]).unwrap()["runtime"],
            json!({ "state": "exited", "exit": { "code": 1 }, "exited_at_ms": 5 })
        );
    }

    #[test]
    fn a_service_without_a_process_was_not_started() {
        let mut failed = web(Status::Failed, None);
        failed.error = Some("boom".into());
        let unreadable = resource(Kind::Service, "web", Status::Live, json!({ "pid": 1 }));
        let views = assess(
            &[web(Status::Pending, None), failed, unreadable],
            &Supervisor::Running(vec![]),
            |_| panic!("no process to look for"),
        );
        let runtime = |i: usize| views[i].runtime.as_ref().unwrap();
        assert_eq!(runtime(0).state, State::NotStarted);
        assert_eq!(runtime(1).state, State::NotStarted);
        assert!(describe(&views[1], 0).ends_with("failed, not started: boom"));
        assert_eq!(runtime(2).state, State::Unknown);
        assert_eq!(
            serde_json::to_value(problems(&views)).unwrap()[0],
            json!({ "kind": "service", "name": "web", "state": "not_started" })
        );
    }

    /// `bp up` was interrupted after the supervisor had started it.
    #[test]
    fn a_service_whose_start_was_not_recorded_is_what_its_supervisor_says() {
        let views = assess(
            &[web(Status::Pending, None)],
            &answer(P, None, None),
            |_| panic!("no process to look for"),
        );
        assert_eq!(views[0].runtime, Some(State::Running.into()));
        assert_eq!(problems(&views)[0].is, Is::Status(Status::Pending));
    }

    #[test]
    fn what_is_neither_live_nor_destroyed_is_a_problem() {
        let found = |statuses: &[Status]| -> Vec<Is> {
            let resources: Vec<_> = statuses.iter().map(|&s| port(s)).collect();
            let views = assess(&resources, &Supervisor::Unknown, |_| true);
            problems(&views).into_iter().map(|p| p.is).collect()
        };
        use Status::{Destroyed, Failed, Live, Pending};
        assert_eq!(found(&[]), []);
        assert_eq!(found(&[Live, Live]), []);
        assert_eq!(found(&[Destroyed, Live]), []);
        assert_eq!(found(&[Live, Destroyed]), []);
        assert_eq!(
            found(&[Pending, Failed]),
            [Is::Status(Pending), Is::Status(Failed)]
        );
    }

    /// Looks at what a sequence of reads returns, without a supervisor.
    fn look_at(
        reads: Vec<Option<Vec<ResourceState>>>,
        at_work: impl Fn(bool) -> Option<String>,
    ) -> Option<Report> {
        let reads = RefCell::new(reads.into_iter());
        let dir = tempfile::tempdir().unwrap();
        look_with(
            dir.path(),
            || {
                let read = reads.borrow_mut().next().expect("one read too many");
                Ok(read.map(recorded))
            },
            at_work,
            |_| Supervisor::Gone,
        )
        .unwrap()
    }

    #[test]
    fn a_pod_that_is_worked_on_is_busy_whatever_it_looks_like() {
        for status in [Status::Live, Status::Pending, Status::Failed] {
            let report = look_at(vec![Some(vec![port(status)])], |_| {
                Some("bp down (pid 5)".into())
            })
            .unwrap();
            assert_eq!(report.health.state, Health::Busy);
            let hint = report.health.hint.unwrap();
            assert!(hint.contains("bp down (pid 5)"), "{hint}");
        }
    }

    #[test]
    fn the_lock_is_looked_at_for_sure_only_if_it_matters() {
        let asked = RefCell::new(Vec::new());
        let at_work = |sure| {
            asked.borrow_mut().push(sure);
            None
        };
        let live = vec![port(Status::Live)];
        let report = look_at(vec![Some(live)], at_work).unwrap();
        assert_eq!(report.health.state, Health::Healthy);
        let failed = vec![port(Status::Failed)];
        let report = look_at(vec![Some(failed.clone()), Some(failed)], at_work).unwrap();
        assert_eq!(report.health.state, Health::Degraded);
        assert_eq!(*asked.borrow(), [false, true]);
    }

    #[test]
    fn a_change_that_ended_meanwhile_is_no_damage() {
        let broken = || Some(vec![port(Status::Pending)]);
        let report = look_at(vec![broken(), Some(vec![port(Status::Live)])], |_| None);
        assert_eq!(report.unwrap().health.state, Health::Healthy);
        // Torn down.
        assert!(look_at(vec![broken(), None], |_| None).is_none());
        assert!(look_at(vec![None], |_| None).is_none());
        // Never the same: it is not looked at for ever.
        let report = look_at(
            vec![broken(), Some(vec![port(Status::Failed)]), broken()],
            |_| None,
        );
        assert_eq!(report.unwrap().health.state, Health::Degraded);
    }

    /// Whatever is left looks fine: `up` rejected the next resource before recording it, or `down`
    /// had only come to the first one.
    #[test]
    fn a_pod_that_is_unfinished_is_degraded() {
        for (unfinished, resources) in [
            (Unfinished::Up, vec![]),
            (Unfinished::Up, vec![port(Status::Live)]),
            (
                Unfinished::Down,
                vec![port(Status::Destroyed), port(Status::Live)],
            ),
        ] {
            let pod = Recorded {
                meta: meta(Some(unfinished)),
                resources,
            };
            let dir = tempfile::tempdir().unwrap();
            let look = |read: &dyn Fn() -> Recorded, work: Option<&str>| {
                let at_work = |sure: bool| {
                    assert!(sure);
                    work.map(str::to_owned)
                };
                look_with(
                    dir.path(),
                    || Ok(Some(read())),
                    at_work,
                    |_| Supervisor::Gone,
                )
                .unwrap()
                .unwrap()
                .health
            };
            let health = look(&|| pod.clone(), None);
            assert_eq!(health.state, Health::Degraded);
            let hint = health.hint.unwrap();
            let name = format!("`bp {unfinished:?}` did not finish").to_lowercase();
            assert!(hint.starts_with(&name), "{hint}");
            assert_eq!(look(&|| pod.clone(), Some("bp")).state, Health::Busy);
            // It finishes while it is looked at.
            let first = RefCell::new(Some(pod.clone()));
            let read = || first.take().unwrap_or(recorded(vec![port(Status::Live)]));
            let at_work = |_| None;
            let health = look_with(
                dir.path(),
                || Ok(Some(read())),
                at_work,
                |_| Supervisor::Gone,
            );
            let health = health.unwrap().unwrap().health;
            assert_eq!(health.state, Health::Healthy);
        }
    }

    #[test]
    fn the_hint_names_the_worktree_and_never_leads_into_a_command_that_hangs() {
        let path = up::quote(Path::new("/wt"));
        let steps = format!("run `bp down {path}`, then `bp up {path}`");
        // Nothing listens: `bp down` notices that. Nor does it wait for one that refuses.
        let cut_off = Supervisor::Unresponsive {
            error: "nothing listens".into(),
            hangs: false,
        };
        for supervisor in [cut_off, Supervisor::Gone] {
            assert_eq!(hang(&supervisor, Some(P)), None);
        }
        assert_eq!(down_hint(&meta(None), None), steps);
        let hang = hang(&unresponsive(), Some(P));
        let text = down_hint(&meta(None), hang.clone());
        assert!(text.contains("end process 7, then run"), "{text}");
        assert!(text.ends_with(&steps), "{text}");

        let text = busy_hint("bp down (pid 5)", &meta(None), None);
        assert!(
            text.ends_with(&format!("run `bp status {path}` again")),
            "{text}"
        );
        let text = busy_hint("bp down (pid 5)", &meta(None), hang);
        assert!(text.starts_with("bp down (pid 5)"), "{text}");
        assert!(text.ends_with("end process 7"), "{text}");
    }

    #[test]
    fn up_is_the_next_step_if_only_processes_do_not_run() {
        let p = |kind, is| Problem {
            kind,
            name: "x".into(),
            is,
        };
        use Kind::{Command, Service, Supervisor};
        for (problems, expected) in [
            (vec![p(Service, Is::State(State::Exited))], true),
            (vec![p(Service, Is::Status(Status::Pending))], true),
            (
                vec![
                    p(Supervisor, Is::State(State::Gone)),
                    p(Service, Is::State(State::Orphaned)),
                ],
                true,
            ),
            (vec![p(Supervisor, Is::State(State::NotStarted))], true),
            (vec![p(Supervisor, Is::State(State::Unresponsive))], false),
            // It answers, but its process was not recorded.
            (vec![p(Supervisor, Is::Status(Status::Pending))], false),
            (vec![p(Service, Is::State(State::Unknown))], false),
            (
                vec![
                    p(Service, Is::State(State::Gone)),
                    p(Command, Is::Status(Status::Failed)),
                ],
                false,
            ),
        ] {
            assert_eq!(startable(&problems), expected, "{problems:?}");
        }
    }

    #[test]
    fn the_last_lines_are_text_whatever_the_bytes_are() {
        let lines = |bytes: &[u8], more| last_lines(bytes, more);
        assert_eq!(lines(b"", false), (vec![], false, false));
        assert_eq!(lines(b"\n", false), (vec!["".into()], false, false));
        assert_eq!(
            lines(b"a\r\nb", false),
            (vec!["a".into(), "b".into()], false, false)
        );
        assert_eq!(
            lines(b"\xffa\n\xc3\xa9\n", false),
            (vec!["\u{fffd}a".into(), "\u{e9}".into()], false, true)
        );
        // Cut in the middle of a character.
        assert_eq!(
            lines("\u{20ac}x".as_bytes().split_at(1).1, true),
            (vec!["x".into()], true, false)
        );
        let many: String = (0..15).map(|i| format!("{i}\n")).collect();
        let (tail, truncated, _) = lines(many.as_bytes(), false);
        assert!(truncated);
        assert_eq!((tail.len(), tail[0].as_str()), (10, "5"));
    }

    #[test]
    fn a_long_line_is_cut() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web.log");
        fs::write(&path, "x".repeat(100_000)).unwrap();
        let log = read_log(path).unwrap();
        assert!(log.truncated);
        assert_eq!(log.tail, ["x".repeat(4096)]);
        assert!(read_log(dir.path().join("none.log")).is_none());
    }

    #[test]
    fn text_has_a_line_per_resource_and_no_escape_sequences() {
        let runtime = Runtime {
            exit: Some(Exit::Code(1)),
            exited_at_ms: Some(1_000),
            log: Some(Log {
                path: "/p/web.log".into(),
                tail: vec!["\u{1b}[31mred\tend".into()],
                truncated: true,
                lossy: false,
                error: None,
            }),
            ..State::Exited.into()
        };
        let view = |resource, runtime| View { resource, runtime };
        let mut failed = port(Status::Failed);
        failed.error = Some("no port\nin the range".into());
        let report = Report {
            pod: meta(None),
            resources: vec![
                view(port(Status::Live), None),
                view(failed, None),
                view(web(Status::Failed, Some(P)), Some(State::Orphaned.into())),
                view(web(Status::Live, Some(P)), Some(runtime)),
            ],
            health: Summary {
                state: Health::Degraded,
                problems: vec![],
                hint: Some("do it".into()),
            },
        };
        assert_eq!(
            report.text(126_000),
            "pod p-1 (/wt)\n\
             \x20 port       web                      live\n\
             \x20 port       web                      failed: no port\n\
             \x20 service    web                      failed, orphaned\n\
             \x20 service    web                      exited (code 1) 2m ago\n\
             degraded: do it\n\
             \n\
             service web: the end of /p/web.log\n\
             \x20 | \u{fffd}[31mred\tend\n"
        );
    }

    /// A real supervisor with the service `web`, recorded as `bp up` would.
    struct Pod {
        tmp: tempfile::TempDir,
        resources: Vec<ResourceState>,
        supervisor: supervisor::Record,
        web: ProcId,
        _reaper: Reaper,
    }

    fn pod(run: Vec<String>) -> Pod {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = control::new_endpoint("test").unwrap();
        let process = launcher()
            .launch(&endpoint, tmp.path(), &tmp.path().join("supervisor.log"))
            .unwrap()
            .process;
        let mut reaper = Reaper(vec![process]);
        let supervisor = supervisor::Record {
            endpoint,
            process: Some(process),
        };
        let start = Start {
            name: "web".into(),
            run,
            cwd: tmp.path().to_path_buf(),
            env: Default::default(),
            log: tmp.path().join("web.log"),
        };
        let data = supervisor::request(&supervisor.endpoint, &Request::Start(start.clone()));
        let web: ProcId = serde_json::from_value(data.unwrap()).unwrap();
        reaper.0.push(web);
        let record = services::Record {
            run: start.run,
            cwd: start.cwd,
            log: Some(start.log),
            process: Some(web),
            boot: proc::boot_id(),
        };
        let live = |kind, name, data| resource(kind, name, Status::Live, data);
        let resources = vec![
            live(
                Kind::Supervisor,
                "supervisor",
                serde_json::to_value(&supervisor).unwrap(),
            ),
            live(Kind::Service, "web", serde_json::to_value(record).unwrap()),
        ];
        Pod {
            tmp,
            resources,
            supervisor,
            web,
            _reaper: reaper,
        }
    }

    impl Pod {
        fn look(&self) -> Report {
            look(
                self.tmp.path(),
                || Ok(Some(recorded(self.resources.clone()))),
                |_| None,
            )
            .unwrap()
            .unwrap()
        }

        /// The states of the supervisor and the service, and the health.
        fn states(&self) -> (State, State, Health) {
            let report = self.look();
            let state = |i: usize| report.resources[i].runtime.as_ref().unwrap().state;
            (state(0), state(1), report.health.state)
        }

        /// The same, once a signal sent to the supervisor has taken effect: it does so a moment after
        /// `kill` returns, and until then an answer is right.
        fn states_after_signal(&self) -> (State, State, Health) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let states = self.states();
                if states.0 != State::Running || Instant::now() >= deadline {
                    return states;
                }
            }
        }
    }

    #[test]
    fn a_service_that_exits_is_reported_with_its_code_and_last_output() {
        let pod = pod(short_running(3));
        assert_eq!(
            pod.states(),
            (State::Running, State::Running, Health::Healthy)
        );
        assert!(proc::wait_gone(pod.web, Duration::from_secs(10)));
        wait_for_log(&pod.tmp.path().join("web.log"), "last words");
        // On macOS a process can count as gone before its parent is told (#84).
        let deadline = Instant::now() + Duration::from_secs(10);
        while pod.states().1 == State::Running && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        let report = pod.look();
        let runtime = report.resources[1].runtime.as_ref().unwrap();
        assert_eq!(
            (runtime.state, runtime.exit),
            (State::Exited, Some(Exit::Code(3)))
        );
        assert!(runtime.exited_at_ms.is_some());
        assert_eq!(runtime.log.as_ref().unwrap().tail, ["last words"]);
        assert_eq!(report.health.state, Health::Degraded);
    }

    #[test]
    fn a_killed_supervisor_is_gone_and_its_service_orphaned_or_gone_with_it() {
        let pod = pod(long_running("web-up"));
        // From then on it writes nothing, so on Unix it outlives the supervisor.
        wait_for_log(&pod.tmp.path().join("web.log"), "web-up");
        proc::kill(pod.supervisor.process.unwrap()).unwrap();
        // Looked at at once: it may still be dying.
        let (supervisor, _, health) = pod.states_after_signal();
        assert_eq!((supervisor, health), (State::Gone, Health::Degraded));
        if cfg!(unix) {
            assert_eq!(
                pod.states(),
                (State::Gone, State::Orphaned, Health::Degraded)
            );
            proc::stop_recorded_tree(pod.web, Duration::from_secs(5)).unwrap();
        }
        assert!(proc::wait_gone(pod.web, Duration::from_secs(10)));
        assert_eq!(pod.states(), (State::Gone, State::Gone, Health::Degraded));
        let report = pod.look();
        let log = report.resources[1].runtime.as_ref().unwrap().log.as_ref();
        assert_eq!(log.unwrap().tail, ["web-up"]);
    }

    /// It listens and lives but never answers, as a supervisor that hangs.
    #[test]
    fn a_supervisor_that_does_not_answer_is_unresponsive_and_not_waited_for() {
        let endpoint = control::new_endpoint("test").unwrap();
        let server = Server::bind(&endpoint).unwrap();
        let me = proc::identify(std::process::id()).unwrap();
        let supervisor = supervisor::Record {
            endpoint,
            process: Some(me),
        };
        let resources = vec![
            resource(
                Kind::Supervisor,
                "supervisor",
                Status::Live,
                serde_json::to_value(&supervisor).unwrap(),
            ),
            web(Status::Live, Some(me)),
        ];
        let tmp = tempfile::tempdir().unwrap();
        // The second one finds the place of the next client taken.
        for _ in 0..2 {
            let started = Instant::now();
            let report = look(
                tmp.path(),
                || Ok(Some(recorded(resources.clone()))),
                |_| None,
            )
            .unwrap()
            .unwrap();
            assert!(started.elapsed() < ANSWER_WAIT + Duration::from_secs(5));
            let runtime = |i: usize| report.resources[i].runtime.as_ref().unwrap();
            assert_eq!(runtime(0).state, State::Unresponsive);
            assert_eq!(runtime(0).error.as_deref(), Some("no answer within 3s"));
            assert_eq!(runtime(1).state, State::Running);
            assert_eq!(report.health.state, Health::Degraded);
            let hint = report.health.hint.unwrap();
            assert!(hint.contains(&format!("end process {}", me.pid)), "{hint}");
        }
        drop(server);
    }

    /// One of another version, say. `bp down` gets the same answer and does not wait.
    #[test]
    fn a_supervisor_that_refuses_does_not_hang() {
        let endpoint = control::new_endpoint("test").unwrap();
        let mut server = Server::bind(&endpoint).unwrap();
        let me = proc::identify(std::process::id()).unwrap();
        let record = supervisor::Record {
            endpoint,
            process: Some(me),
        };
        let observer = std::thread::spawn(move || observe(Some(&record)));
        let conn = server.accept().unwrap();
        conn.read_line().unwrap();
        conn.write_line(r#"{"v":1,"result":"error","message":"no"}"#)
            .unwrap();
        drop(conn);
        match observer.join().unwrap() {
            Supervisor::Unresponsive { error, hangs } => {
                assert_eq!((&*error, hangs), ("no", false))
            }
            other => panic!("{other:?}"),
        }
    }

    /// Its endpoint was removed, by whoever cleans up the directory for temporary files, say.
    #[test]
    fn a_supervisor_that_cannot_be_reached_is_not_gone_while_it_runs() {
        let me = proc::identify(std::process::id()).unwrap();
        let record = |process| supervisor::Record {
            endpoint: control::new_endpoint("test").unwrap(),
            process,
        };
        let observed = observe(Some(&record(Some(me))));
        assert!(
            matches!(observed, Supervisor::Unresponsive { hangs: false, .. }),
            "{observed:?}"
        );
        let other = ProcId { start: 1, ..me };
        let observed = observe(Some(&record(Some(other))));
        assert!(matches!(observed, Supervisor::Gone), "{observed:?}");
        let observed = observe(Some(&record(None)));
        assert!(matches!(observed, Supervisor::NotStarted), "{observed:?}");
    }

    /// A process that is stopped until this is dropped, also when the test fails.
    #[cfg(unix)]
    struct Stopped(ProcId);

    #[cfg(unix)]
    impl Stopped {
        fn new(process: ProcId) -> Stopped {
            let stopped = Stopped(process);
            assert_eq!(stopped.signal(libc::SIGSTOP), 0);
            stopped
        }

        fn signal(&self, signal: libc::c_int) -> libc::c_int {
            // SAFETY: plain system call on a process of ours.
            unsafe { libc::kill(self.0.pid as libc::pid_t, signal) }
        }
    }

    #[cfg(unix)]
    impl Drop for Stopped {
        fn drop(&mut self) {
            self.signal(libc::SIGCONT);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_stopped_supervisor_is_unresponsive_until_it_goes_on() {
        let pod = pod(long_running("web-up"));
        let stopped = Stopped::new(pod.supervisor.process.unwrap());
        assert_eq!(
            pod.states_after_signal(),
            (State::Unresponsive, State::Running, Health::Degraded)
        );
        drop(stopped);
        assert_eq!(
            pod.states(),
            (State::Running, State::Running, Health::Healthy)
        );
    }
}
