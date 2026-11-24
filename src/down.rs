//! `bp down`: tears down everything recorded for a pod.
//!
//! Driven by the ledger alone, so it works with the worktree already gone, from another branch, or
//! after the config changed. Every resource that is not destroyed is visited, pending and failed
//! ones included (they may exist partially). Each step is idempotent and its outcome is recorded;
//! a failure does not stop the others, and what is left is reported at the end. Once nothing is
//! left, the pod's state is removed as well.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

use crate::files::{self, EntryType};
use crate::ledger::{Event, Kind, Ledger, ResourceState, Status};
use crate::pod::{PodMeta, Store};
use crate::supervisor::{self, Request};
use crate::{commands, control, ports, proc, services};

/// How long a supervisor whose channel broke gets to be gone.
const DYING_WAIT: Duration = Duration::from_secs(1);

/// Outcome of tearing down one resource.
#[derive(Debug)]
pub(crate) struct Step {
    pub(crate) kind: Kind,
    pub(crate) name: String,
    /// `None` when the resource is gone.
    pub(crate) error: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Report {
    pub(crate) steps: Vec<Step>,
    /// Every resource is gone, but the pod's own state could not be removed.
    pub(crate) state_error: Option<String>,
}

impl Report {
    /// Nothing recorded for the pod is left.
    pub(crate) fn is_clean(&self) -> bool {
        self.state_error.is_none() && self.steps.iter().all(|s| s.error.is_none())
    }

    pub(crate) fn leftovers(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|s| s.error.is_some())
    }
}

/// Tears down pod `meta`. The caller holds the pod lock.
///
/// Fails only when teardown cannot start (the ledger cannot be read or opened); everything
/// after that is collected in the report.
pub(crate) fn teardown(store: &Store, meta: &PodMeta) -> Result<Report> {
    let resources = store.resources(&meta.id)?;
    let todo = order(&resources);
    // Services are stopped through the supervisor that runs them.
    let supervisor = resources
        .iter()
        .rfind(|r| r.kind == Kind::Supervisor)
        .and_then(|r| data::<supervisor::Record>(r).ok());
    let mut steps = Vec::new();
    if !todo.is_empty() {
        let mut ledger = Ledger::open(&store.ledger_path(&meta.id))?;
        for i in todo {
            let r = &resources[i];
            let mut error = destroy(store, meta, r, supervisor.as_ref(), &mut ledger)
                .err()
                .map(|e| format!("{e:#}"));
            let event = match &error {
                None => Event::Destroyed,
                Some(e) => Event::Failed { error: e.clone() },
            };
            // Unrecorded, a destroyed resource is still live in the ledger: the next `bp down`
            // visits it again, which is harmless because every step is idempotent.
            if let Err(e) = ledger.record(r.kind, &r.name, event) {
                error = Some(match error {
                    None => format!("destroyed, but {e:#}"),
                    Some(prev) => format!("{prev}; {e:#}"),
                });
            }
            steps.push(Step {
                kind: r.kind,
                name: r.name.clone(),
                error,
            });
        }
    }
    let mut report = Report {
        steps,
        state_error: None,
    };
    if report.is_clean() {
        report.state_error = store.remove_pod(&meta.id).err().map(|e| format!("{e:#}"));
    }
    Ok(report)
}

/// Indices of the resources to visit, in teardown order: the reverse of creation, except that
/// services stop first (Windows keeps the files a process holds locked), followed by the
/// supervisor that ran them, and file links go before anything else is removed, so that no
/// recursive delete can reach into the main checkout through one.
fn order(resources: &[ResourceState]) -> Vec<usize> {
    let mut todo: Vec<usize> = (0..resources.len())
        .rev()
        .filter(|&i| resources[i].status != Status::Destroyed)
        .collect();
    todo.sort_by_key(|&i| phase(&resources[i])); // stable: reverse order within a phase
    todo
}

fn phase(r: &ResourceState) -> u8 {
    match r.kind {
        Kind::Service => 0,
        Kind::Supervisor => 1,
        Kind::File if data::<files::Record>(r).is_ok_and(|f| f.entry == EntryType::Link) => 2,
        _ => 3,
    }
}

fn destroy(
    store: &Store,
    meta: &PodMeta,
    r: &ResourceState,
    supervisor: Option<&supervisor::Record>,
    ledger: &mut Ledger,
) -> Result<()> {
    match r.kind {
        Kind::Service => {
            let record: services::Record = data(r)?;
            stop_service(supervisor, &r.name, &record)
                .with_context(|| format!("service {:?}", r.name))
        }
        Kind::Supervisor => stop_supervisor(&data(r)?),
        Kind::File => {
            let record: files::Record = data(r)?;
            files::remove(&record, |moved| {
                let data = serde_json::to_value(moved)?;
                ledger.record(r.kind, &r.name, Event::Moved { data })
            })
        }
        Kind::Port => ports::Registry::new(store.home())
            .release(&meta.id, &r.name)
            .map(drop),
        Kind::Command => {
            let record: commands::Record = data(r)?;
            commands::destroy(&record, &command_cwd(store, meta)?)
                .map(drop)
                .with_context(|| format!("command {:?}", r.name))
        }
        Kind::Git => {
            bail!("teardown of {} resources is not supported yet", r.kind)
        }
    }
}

/// Asks the supervisor to stop the service. When it is gone, the service's process tree is
/// stopped by its recorded leader instead; a service never reported as started had its pid known
/// only to that supervisor.
fn stop_service(
    supervisor: Option<&supervisor::Record>,
    name: &str,
    record: &services::Record,
) -> Result<()> {
    if let Some(sup) = supervisor {
        let stop = Request::Stop {
            name: name.to_owned(),
        };
        match supervisor::request(&sup.endpoint, &stop) {
            Ok(_) => {}
            Err(e) if is_gone(sup, &e) => {}
            Err(e) => return Err(e),
        }
    }
    // Also after the supervisor answered: it may not know a process recorded for this pod.
    if let Some(process) = record.process {
        proc::stop_recorded_tree(process, supervisor::GRACE)?;
    }
    Ok(())
}

/// Whether a request failed with `error` because there is no supervisor any more. The channel of
/// one that is being killed can outlive it for a moment, and then breaks: that counts only if the
/// recorded process is gone, too.
fn is_gone(supervisor: &supervisor::Record, error: &anyhow::Error) -> bool {
    if supervisor::is_unreachable(error) {
        return true;
    }
    if supervisor::is_answer(error) {
        return false;
    }
    supervisor
        .process
        .is_some_and(|process| proc::wait_gone(process, DYING_WAIT))
}

/// Shuts the supervisor down (it stops whatever still runs) and waits for it to exit; kills it if
/// it does not answer or does not exit.
fn stop_supervisor(record: &supervisor::Record) -> Result<()> {
    let answered = match supervisor::request(&record.endpoint, &Request::Shutdown) {
        Ok(_) => true,
        Err(e) if is_gone(record, &e) => false,
        Err(e) => return Err(e),
    };
    if let Some(process) = record.process {
        let exited = answered && proc::wait_gone(process, supervisor::EXIT_WAIT);
        if !exited {
            proc::kill(process)?;
            if !proc::wait_gone(process, supervisor::EXIT_WAIT) {
                bail!("supervisor (pid {}) does not exit", process.pid);
            }
        }
    }
    control::remove_stale(&record.endpoint)
}

fn data<T: DeserializeOwned>(r: &ResourceState) -> Result<T> {
    serde_json::from_value(r.data.clone())
        .with_context(|| format!("unreadable ledger data of {} {:?}", r.kind, r.name))
}

/// Commands are undone in the worktree root while it exists, otherwise in the `bp` home. Not the
/// pod's state directory: a daemon started by `destroy` (a sync agent, say) would keep it as its
/// working directory, and on Windows the directory could then never be removed.
fn command_cwd(store: &Store, meta: &PodMeta) -> Result<PathBuf> {
    if meta.worktree.is_absolute() && meta.worktree.is_dir() {
        return Ok(meta.worktree.clone());
    }
    Ok(std::path::absolute(store.home())?)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;
    use crate::pod::{self, META_VERSION};
    use crate::test_support::{Reaper, fail, launcher, long_running};

    struct Setup {
        _tmp: tempfile::TempDir,
        store: Store,
        meta: PodMeta,
        tmp: PathBuf,
    }

    fn setup(worktree_exists: bool) -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(tmp.path()).unwrap();
        let worktree = base.join("wt-\u{e9}t\u{e9}");
        if worktree_exists {
            fs::create_dir(&worktree).unwrap();
        }
        let store = Store::at(base.join("home"));
        let meta = PodMeta {
            v: META_VERSION,
            id: pod::pod_id(&worktree),
            worktree,
            main_checkout: None,
        };
        store.write_meta(&meta).unwrap();
        Setup {
            _tmp: tmp,
            store,
            meta,
            tmp: base,
        }
    }

    impl Setup {
        fn record(&self, kind: Kind, name: &str, events: Vec<Event>) {
            let mut ledger = Ledger::open(&self.store.ledger_path(&self.meta.id)).unwrap();
            for e in events {
                ledger.record(kind, name, e).unwrap();
            }
        }

        fn live(&self, kind: Kind, name: &str, data: Value) {
            self.record(kind, name, vec![Event::intent(), Event::Created { data }]);
        }
    }

    fn file_record(root: &Path, rel: &str, entry: &str) -> Value {
        json!({ "root": root, "path": root.join(rel), "type": entry })
    }

    /// Deletes `file`, using only what every CI runner has.
    fn delete(file: &Path) -> Vec<String> {
        let file = file.to_string_lossy().into_owned();
        if cfg!(windows) {
            vec!["cmd".into(), "/d".into(), "/c".into(), "del".into(), file]
        } else {
            vec!["rm".into(), file]
        }
    }

    #[test]
    fn tears_down_every_kind_and_removes_the_pod() {
        let s = setup(true);
        let wt = &s.meta.worktree;
        let registry = ports::Registry::new(s.store.home());
        let port = registry
            .allocate(&s.meta.id, "web", [41000, 41999])
            .unwrap();
        s.live(Kind::Port, "web", json!({ "port": port }));

        fs::write(wt.join(".env"), "A=1").unwrap();
        s.live(Kind::File, ".env", file_record(wt, ".env", "file"));
        fs::create_dir_all(wt.join("vendor/pkg")).unwrap();
        fs::write(wt.join("vendor/pkg/a.txt"), "a").unwrap();
        s.live(Kind::File, "vendor", file_record(wt, "vendor", "dir"));

        let marker = s.tmp.join("container");
        fs::write(&marker, "").unwrap();
        s.live(
            Kind::Command,
            "app",
            json!({ "create": ["x"], "stdout": "abc", "destroy": delete(&marker) }),
        );

        let report = teardown(&s.store, &s.meta).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(
            report
                .steps
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["app", "vendor", ".env", "web"]
        );
        assert!(!marker.exists());
        assert_eq!(fs::read_dir(wt).unwrap().count(), 0);
        assert_eq!(registry.registered(&s.meta.id, "web").unwrap(), None);
        assert!(!s.store.pod_dir(&s.meta.id).exists());
        assert_eq!(s.store.find_by_worktree(wt).unwrap(), None);
    }

    #[test]
    fn failures_are_collected_recorded_and_retried() {
        let s = setup(true);
        let wt = &s.meta.worktree;
        // Pending: interrupted before it was made. Its intent data is enough.
        s.record(
            Kind::File,
            "conf",
            vec![Event::Intent {
                data: file_record(wt, "conf", "dir"),
            }],
        );
        s.live(
            Kind::Command,
            "app",
            json!({ "create": ["x"], "destroy": fail(3, "boom") }),
        );
        s.live(Kind::Service, "vite", json!({ "pid": 1 }));
        s.live(Kind::Port, "web", Value::Null);

        let report = teardown(&s.store, &s.meta).unwrap();
        assert!(!report.is_clean());
        let failed: Vec<&str> = report.leftovers().map(|s| s.name.as_str()).collect();
        assert_eq!(failed, ["vite", "app"]);
        let app = report.steps.iter().find(|s| s.name == "app").unwrap();
        assert!(app.error.as_deref().unwrap().contains("exited"), "{app:?}");

        // Recorded: failed ones keep their data for the next attempt, the rest are destroyed.
        let state = s.store.resources(&s.meta.id).unwrap();
        let status = |name: &str| state.iter().find(|r| r.name == name).unwrap();
        assert_eq!(status("conf").status, Status::Destroyed);
        assert_eq!(status("web").status, Status::Destroyed);
        assert_eq!(status("app").status, Status::Failed);
        assert_eq!(status("app").data["destroy"], json!(fail(3, "boom")));
        assert!(s.store.pod_dir(&s.meta.id).join("pod.json").exists());

        // The next run visits only what is left.
        let again = teardown(&s.store, &s.meta).unwrap();
        let names: Vec<&str> = again.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["vite", "app"]);
    }

    #[test]
    fn works_when_the_worktree_is_gone() {
        let s = setup(false);
        let marker = s.tmp.join("volume");
        fs::write(&marker, "").unwrap();
        s.live(
            Kind::Command,
            "volume",
            json!({ "create": ["x"], "destroy": delete(&marker) }),
        );
        s.live(
            Kind::File,
            ".env",
            file_record(&s.meta.worktree, ".env", "file"),
        );
        let report = teardown(&s.store, &s.meta).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert!(!marker.exists());
        assert!(!s.store.pod_dir(&s.meta.id).exists());
    }

    #[test]
    fn an_interrupted_directory_delete_is_finished_from_the_trash() {
        let s = setup(true);
        let wt = &s.meta.worktree;
        let trash = wt.join("vendor.bp-trash-1-2");
        fs::create_dir_all(trash.join("half")).unwrap();
        s.live(Kind::File, "vendor", file_record(wt, "vendor", "dir"));
        let mut moved = file_record(wt, "vendor", "dir");
        moved["trash"] = json!(trash);
        s.record(Kind::File, "vendor", vec![Event::Moved { data: moved }]);
        let report = teardown(&s.store, &s.meta).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert!(!trash.exists());
    }

    #[test]
    fn an_output_file_whose_creation_was_not_recorded_goes_with_the_pod() {
        let s = setup(true);
        let marker = s.tmp.join("container");
        fs::write(&marker, "").unwrap();
        let outputs = s.store.pod_dir(&s.meta.id).join("outputs");
        // Interrupted at some point after the intent: with the file or before it.
        for (name, destroy) in [("env", delete(&marker)), ("none", vec![])] {
            let file = outputs.join(name);
            let mut data = json!({ "create": ["x"], "file": file });
            if !destroy.is_empty() {
                fs::create_dir_all(&outputs).unwrap();
                fs::write(&file, "TOKEN=s3cr3t").unwrap();
                fs::write(outputs.join("env.tmp"), "TOK").unwrap();
                data["destroy"] = json!(destroy);
            }
            s.record(Kind::Command, name, vec![Event::Intent { data }]);
        }
        let report = teardown(&s.store, &s.meta).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.steps.len(), 2);
        assert!(!marker.exists());
        assert!(!s.store.pod_dir(&s.meta.id).exists());
    }

    #[test]
    fn an_empty_or_fully_destroyed_ledger_just_removes_the_pod() {
        let s = setup(true);
        s.live(Kind::Port, "web", Value::Null);
        s.record(Kind::Port, "web", vec![Event::Destroyed]);
        let report = teardown(&s.store, &s.meta).unwrap();
        assert!(report.is_clean() && report.steps.is_empty());
        assert!(!s.store.pod_dir(&s.meta.id).exists());
    }

    /// Starts a supervisor running one long service; records both as `bp up` would.
    fn supervised(s: &Setup) -> (supervisor::Record, services::Record, Reaper) {
        let endpoint = control::new_endpoint(&s.meta.id).unwrap();
        let process = launcher()
            .launch(&endpoint, &s.tmp, &s.tmp.join("supervisor.log"))
            .unwrap()
            .process;
        let mut reaper = Reaper(vec![process]);
        let sup = supervisor::Record {
            endpoint,
            process: Some(process),
        };
        let start = supervisor::Start {
            name: "web".into(),
            run: long_running("web"),
            cwd: s.meta.worktree.clone(),
            env: Default::default(),
            log: s.tmp.join("web.log"),
        };
        let data = supervisor::request(&sup.endpoint, &Request::Start(start.clone())).unwrap();
        let web = services::Record {
            run: start.run,
            cwd: start.cwd,
            process: Some(serde_json::from_value(data).unwrap()),
        };
        reaper.0.extend(web.process);
        s.live(
            Kind::Supervisor,
            "supervisor",
            serde_json::to_value(&sup).unwrap(),
        );
        s.live(Kind::Service, "web", serde_json::to_value(&web).unwrap());
        (sup, web, reaper)
    }

    #[test]
    fn services_are_stopped_through_the_supervisor() {
        let s = setup(true);
        let (sup, web, _reaper) = supervised(&s);
        let report = teardown(&s.store, &s.meta).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert!(!proc::is_running(web.process.unwrap()));
        assert!(!proc::is_running(sup.process.unwrap()));
        assert!(!s.store.pod_dir(&s.meta.id).exists());
    }

    #[test]
    fn services_are_stopped_when_the_supervisor_is_gone() {
        let s = setup(true);
        let (sup, web, _reaper) = supervised(&s);
        let sup_process = sup.process.unwrap();
        proc::kill(sup_process).unwrap();
        assert!(proc::wait_gone(sup_process, supervisor::EXIT_WAIT));
        let web_process = web.process.unwrap();
        if cfg!(unix) {
            assert!(
                proc::is_running(web_process),
                "a service outlives a killed supervisor"
            );
        }

        let report = teardown(&s.store, &s.meta).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert!(proc::wait_gone(web_process, Duration::from_secs(5)));
    }

    #[test]
    fn a_supervisor_that_never_reported_is_not_waited_for() {
        let s = setup(true);
        // Interrupted between the intent and the supervisor's answer: nothing listens.
        let intent = supervisor::Record {
            endpoint: control::new_endpoint(&s.meta.id).unwrap(),
            process: None,
        };
        s.record(
            Kind::Supervisor,
            "supervisor",
            vec![Event::Intent {
                data: serde_json::to_value(&intent).unwrap(),
            }],
        );
        let report = teardown(&s.store, &s.meta).unwrap();
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn unreadable_data_is_a_leftover_not_a_crash() {
        let s = setup(true);
        s.live(Kind::File, ".env", json!({ "nonsense": true }));
        let report = teardown(&s.store, &s.meta).unwrap();
        let err = report.steps[0].error.as_deref().unwrap();
        assert!(err.contains("unreadable ledger data"), "{err}");
    }

    #[test]
    fn order_stops_services_then_removes_links_then_the_rest() {
        let root = Path::new("/wt");
        let r = |kind, name: &str, data: Value, status| ResourceState {
            kind,
            name: name.into(),
            status,
            data,
            error: None,
        };
        let resources = [
            r(Kind::Port, "web", Value::Null, Status::Live),
            r(
                Kind::File,
                "nm",
                file_record(root, "nm", "link"),
                Status::Live,
            ),
            r(
                Kind::File,
                "app",
                file_record(root, "app", "dir"),
                Status::Failed,
            ),
            r(Kind::Command, "db", Value::Null, Status::Pending),
            r(
                Kind::File,
                "vendor",
                file_record(root, "app/v", "link"),
                Status::Live,
            ),
            r(Kind::Service, "vite", Value::Null, Status::Live),
            r(Kind::Port, "old", Value::Null, Status::Destroyed),
        ];
        let names: Vec<&str> = order(&resources)
            .into_iter()
            .map(|i| resources[i].name.as_str())
            .collect();
        assert_eq!(names, ["vite", "vendor", "nm", "db", "app", "web"]);
    }
}
