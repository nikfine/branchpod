//! `bp up`: provision everything a worktree needs.
//!
//! Under the pod lock, resources are created in a fixed order of kinds — ports, files, commands,
//! services — and within a kind in config order (ports by name: they never depend on each other).
//! Ports come first because templates and commands reference them; files before commands because a
//! command (a container) may mount a file provisioned for it; services last, because they use all
//! of it. Services run under the pod's supervisor, started (and recorded) just before the first of
//! them.
//!
//! Before anything is created, the whole config is planned once with stand-in values for what only
//! creation yields (ports, command output): a typo in the last command or a missing source fails
//! before the first port is allocated. What only creation can reveal still fails midway.
//!
//! Every resource goes through the write-ahead ledger: the intent (with what is needed to undo an
//! interrupted creation) before it is created, its data after, or the failure. On failure nothing
//! is rolled back here: what was created stays recorded and `bp down` removes it.
//!
//! `up` is not re-entrant on a pod: if the ledger holds any resource that is not destroyed, it
//! fails straight away and asks for `bp down` first. Reconciling a live pod with a changed config
//! is a different feature.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::config::{Config, FileMode};
use crate::ledger::{Event, Kind, Ledger, ResourceState, Status};
use crate::pod::{self, META_VERSION, PodMeta, Store};
use crate::podlock::Holder;
use crate::ports::{self, Registry};
use crate::proc::ProcId;
use crate::supervisor::{self, Launcher, Request};
use crate::vars::Vars;
use crate::worktree::Worktree;
use crate::{commands, control, files, services};

/// A provisioned pod.
#[derive(Debug)]
pub(crate) struct Report {
    pub(crate) pod: PodMeta,
    /// What this run did, in creation order.
    pub(crate) items: Vec<Item>,
    /// For the user: the pod is up, but may not stay so.
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) kind: Kind,
    pub(crate) name: String,
    /// Short human-readable result: the port, the file mode, the command's output.
    pub(crate) detail: String,
}

/// `up` failed after recording resources. They may exist and are left for `bp down`.
///
/// The cause is `error`, not a `source()`: the message already includes it, and a source would
/// print it twice in an error chain. A typed cause (such as [`commands::Failed`] with the full
/// stderr) is reached with `failed.error.downcast_ref()`.
#[derive(Debug)]
pub(crate) struct Failed {
    pub(crate) error: anyhow::Error,
    pub(crate) pod: String,
    pub(crate) worktree: PathBuf,
    /// Every resource of the pod that is not destroyed, as the ledger has it now. Empty if the
    /// ledger could not be read back.
    pub(crate) recorded: Vec<ResourceState>,
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}\n\n", self.error)?;
        if self.recorded.is_empty() {
            writeln!(
                f,
                "resources may have been created for pod {} (the ledger could not be read back)",
                self.pod
            )?;
        } else {
            write!(
                f,
                "resources recorded for pod {} (they may exist):\n{}",
                self.pod,
                listing(&self.recorded)
            )?;
        }
        write!(f, "run `bp down {}` to remove them", quote(&self.worktree))
    }
}

impl std::error::Error for Failed {}

/// The ledger, remembering whether this run wrote anything to it.
struct Journal {
    ledger: Ledger,
    wrote: bool,
}

/// Provisions the pod of `wt` from `config`. `on_wait` is called if another `bp` holds the pod
/// lock, before waiting for it.
pub(crate) fn up(
    store: &Store,
    wt: &Worktree,
    config: &Config,
    launcher: &Launcher,
    on_wait: impl FnOnce(Option<&Holder>),
) -> Result<Report> {
    let main = wt.main_checkout.as_deref();
    let needs_main = config.file.iter().any(|f| f.mode != FileMode::Ignore);
    if needs_main {
        match main {
            None => {
                bail!("file resources are copied from the main checkout, but {NO_MAIN_CHECKOUT}")
            }
            Some(main) if pod::same_path(main, &wt.root) => bail!(
                "{} is the main checkout: file resources are carried over from it into linked \
                 worktrees, so there is nothing to carry over here; run `bp up` in a linked worktree",
                wt.root.display()
            ),
            Some(_) => {}
        }
    }

    let id = pod::pod_id(&wt.root);
    let _lock = store.lock_pod(&id, "up", on_wait)?;

    let existing = live(store.resources(&id)?);
    if !existing.is_empty() {
        bail!(
            "pod {id} already has resources recorded:\n{}\
             run `bp down {}` first",
            listing(&existing),
            quote(&wt.root)
        );
    }
    // Left when an earlier `down` could not remove the pod's state: no `destroy` needs it any more,
    // it may hold secrets, and it must not pass for the output of a command of this run.
    commands::remove_outputs(&store.pod_dir(&id))?;

    let mut vars = builtin_vars(&id, wt);
    check(config, wt, main, &vars, &store.pod_dir(&id))?;
    if !config.service.is_empty() {
        launcher.preflight().context("cannot run services")?;
    }

    let meta = PodMeta {
        v: META_VERSION,
        id: id.clone(),
        worktree: wt.root.clone(),
        main_checkout: wt.main_checkout.clone(),
    };
    store.write_meta(&meta)?;

    let mut journal = Journal {
        ledger: Ledger::open(&store.ledger_path(&id))?,
        wrote: false,
    };
    let mut items = Vec::new();
    let mut warnings = Vec::new();
    let result = provision(
        store,
        &id,
        wt,
        main,
        config,
        launcher,
        &mut journal,
        &mut vars,
        &mut items,
        &mut warnings,
    );
    match result {
        Ok(()) => Ok(Report {
            pod: meta,
            items,
            warnings,
        }),
        Err(error) if !journal.wrote => Err(error),
        Err(error) => {
            let recorded = store.resources(&id).map(live).unwrap_or_default();
            Err(Failed {
                error,
                pod: id,
                worktree: wt.root.clone(),
                recorded,
            }
            .into())
        }
    }
}

/// Plans every resource with stand-in values for what only creation yields: the first port of
/// each range, a single-line placeholder for each command's output. Nothing is written.
fn check(
    config: &Config,
    wt: &Worktree,
    main: Option<&Path>,
    vars: &Vars,
    pod_dir: &Path,
) -> Result<()> {
    let mut probe = vars.clone();
    for (name, spec) in &config.port {
        probe.set(format!("port.{name}"), spec.range[0].to_string());
    }
    for spec in &config.file {
        if spec.mode == FileMode::Ignore {
            continue;
        }
        let main = main.expect("checked before taking the lock");
        files::plan(spec, main, &wt.root, &probe)?;
    }
    for spec in &config.command {
        let plan = commands::plan(spec, &wt.root, &probe, pod_dir)?;
        commands::export_planned(&plan, &mut probe);
    }
    for spec in &config.service {
        services::plan(spec, &wt.root, &probe, pod_dir)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn provision(
    store: &Store,
    id: &str,
    wt: &Worktree,
    main: Option<&Path>,
    config: &Config,
    launcher: &Launcher,
    journal: &mut Journal,
    vars: &mut Vars,
    items: &mut Vec<Item>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    // The pod lock is held: the registry lock is taken after it, as the lock order requires.
    let registry = Registry::new(store.home());
    for (name, spec) in &config.port {
        let record = step(journal, Kind::Port, name, Value::Null, || {
            // The pod has nothing live, so a registration it still has is stale (a lost ledger,
            // a pod directory removed by hand): it would be returned without a bind check.
            registry.release(id, name)?;
            let port = registry.allocate(id, name, spec.range)?;
            Ok(ports::Record { port })
        })
        .with_context(|| format!("port {name:?}"))?;
        vars.set(format!("port.{name}"), record.port.to_string());
        items.push(Item {
            kind: Kind::Port,
            name: name.clone(),
            detail: record.port.to_string(),
        });
    }

    for spec in &config.file {
        if spec.mode == FileMode::Ignore {
            continue;
        }
        let main = main.expect("checked before taking the lock");
        let name = spec.name();
        // Planned again with the real values; still before the intent, so a rule that cannot be
        // created records nothing.
        let Some(plan) = files::plan(spec, main, &wt.root, vars)? else {
            items.push(Item {
                kind: Kind::File,
                name,
                detail: "skipped (optional, no source)".into(),
            });
            continue;
        };
        let intent = serde_json::to_value(&plan.record)?;
        step(journal, Kind::File, &name, intent, || files::create(&plan))?;
        items.push(Item {
            kind: Kind::File,
            name,
            detail: spec.mode.as_str().to_owned(),
        });
    }

    let pod_dir = store.pod_dir(id);
    for spec in &config.command {
        let plan = commands::plan(spec, &wt.root, vars, &pod_dir)?;
        let intent = serde_json::to_value(&plan.record)?;
        let record = step(journal, Kind::Command, &spec.name, intent, || {
            commands::create(&plan).map(|out| out.record)
        })?;
        commands::export(&spec.name, &record, vars);
        items.push(Item {
            kind: Kind::Command,
            name: spec.name.clone(),
            detail: match &record.file {
                Some(file) => format!("output in {file}"),
                None => command_detail(record.stdout.as_deref().unwrap_or_default()),
            },
        });
    }

    if config.service.is_empty() {
        return Ok(());
    }
    let intent = supervisor::Record {
        endpoint: control::new_endpoint(id)?,
        process: None,
    };
    // Not the worktree or the pod directory: on Windows its working directory could not be
    // removed while it runs.
    let cwd = std::path::absolute(store.home())?;
    let sup = step(
        journal,
        Kind::Supervisor,
        SUPERVISOR,
        serde_json::to_value(&intent)?,
        || {
            let log = pod_dir.join("logs").join("supervisor.log");
            let launched = launcher.launch(&intent.endpoint, &cwd, &log)?;
            warnings.extend(launched.warning);
            Ok(supervisor::Record {
                process: Some(launched.process),
                ..intent.clone()
            })
        },
    )?;
    items.push(Item {
        kind: Kind::Supervisor,
        name: SUPERVISOR.to_owned(),
        detail: process_detail(sup.process),
    });

    for spec in &config.service {
        let plan = services::plan(spec, &wt.root, vars, &pod_dir)?;
        let intent = serde_json::to_value(&plan.record)?;
        let record = step(journal, Kind::Service, &spec.name, intent, || {
            let data = supervisor::request(&sup.endpoint, &Request::Start(plan.start.clone()))?;
            Ok(services::Record {
                process: Some(serde_json::from_value(data)?),
                ..plan.record.clone()
            })
        })
        .with_context(|| format!("service {:?}", spec.name))?;
        items.push(Item {
            kind: Kind::Service,
            name: spec.name.clone(),
            detail: process_detail(record.process),
        });
    }
    Ok(())
}

/// Ledger name of the supervisor resource.
const SUPERVISOR: &str = "supervisor";

fn process_detail(process: Option<ProcId>) -> String {
    process
        .map(|p| format!("pid {}", p.pid))
        .unwrap_or_default()
}

/// Records the intent, runs `create` and records its data or its failure.
fn step<T: Serialize>(
    journal: &mut Journal,
    kind: Kind,
    name: &str,
    intent: Value,
    create: impl FnOnce() -> Result<T>,
) -> Result<T> {
    // Set before the write: a failed write may still have reached the file.
    journal.wrote = true;
    let ledger = &mut journal.ledger;
    ledger.record(kind, name, Event::Intent { data: intent })?;
    match create() {
        Ok(created) => {
            // Not recording it leaves the resource pending, which teardown still visits.
            let data = serde_json::to_value(&created)?;
            ledger.record(kind, name, Event::Created { data })?;
            Ok(created)
        }
        Err(error) => {
            let failed = Event::Failed {
                error: format!("{error:#}"),
            };
            if let Err(e) = ledger.record(kind, name, failed) {
                return Err(error.context(format!("the failure was not recorded either: {e:#}")));
            }
            Err(error)
        }
    }
}

/// Why `main` has no value. Submodules and `--separate-git-dir` have a main checkout too, but it
/// cannot be told from the common git dir yet.
const NO_MAIN_CHECKOUT: &str =
    "there is no main checkout (a bare repository, or a separate git dir such as a submodule's)";

/// `pod.id`, `worktree`, `main` and `branch`. A name that has no value here (a detached HEAD, no
/// main checkout, a path that is not UTF-8) is declared unavailable with the reason, so that a
/// reference to it fails with an explanation instead of rendering something wrong.
fn builtin_vars(id: &str, wt: &Worktree) -> Vars {
    let mut vars = Vars::new();
    vars.set("pod.id", id);
    path_var(&mut vars, "worktree", &wt.root);
    match &wt.main_checkout {
        Some(main) => path_var(&mut vars, "main", main),
        None => vars.set_unavailable("main", NO_MAIN_CHECKOUT),
    }
    // A config that does not use `branch` must not fail because it cannot be determined.
    match wt.branch() {
        Ok(Some(branch)) => vars.set("branch", branch),
        Ok(None) => vars.set_unavailable("branch", "HEAD is detached"),
        Err(e) => vars.set_unavailable("branch", format!("{e:#}")),
    }
    vars
}

/// `path` quoted for the user's shell, for a command to copy from a message: single quotes on
/// Unix (nothing expands inside them), double quotes on Windows (`cmd.exe` knows no others;
/// PowerShell takes both).
fn quote(path: &Path) -> String {
    let text = path.display().to_string();
    if cfg!(windows) {
        format!("\"{text}\"")
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

fn path_var(vars: &mut Vars, name: &str, path: &Path) {
    match path.to_str() {
        Some(text) => vars.set(name, text),
        None => vars.set_unavailable(name, format!("{} is not valid UTF-8", path.display())),
    }
}

/// The output of a command, as long as it fits on a short line.
fn command_detail(stdout: &str) -> String {
    const MAX: usize = 64;
    match stdout.lines().count() {
        0 => "(no output)".into(),
        1 if stdout.chars().count() <= MAX => stdout.to_owned(),
        1 => format!("{}...", stdout.chars().take(MAX).collect::<String>()),
        n => format!("({n} lines of output)"),
    }
}

fn live(resources: Vec<ResourceState>) -> Vec<ResourceState> {
    resources
        .into_iter()
        .filter(|r| r.status != Status::Destroyed)
        .collect()
}

/// One indented line per resource: kind, name, status.
fn listing(resources: &[ResourceState]) -> String {
    let mut out = String::new();
    for r in resources {
        out.push_str(&format!("  {:<10} {:<24} {}\n", r.kind, r.name, r.status));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fail, git_ok, launcher, long_running, reap_pod};
    use std::fs;

    struct Repo {
        _tmp: tempfile::TempDir,
        store: Store,
        main: PathBuf,
        linked: PathBuf,
    }

    /// A main checkout and a linked worktree on branch `feature`, with a `bp` home of their own.
    fn repo() -> Repo {
        let tmp = tempfile::tempdir().unwrap();
        // Non-ASCII on purpose: it is the normal case on many developer machines.
        let main = tmp.path().join("main-caf\u{e9}");
        fs::create_dir(&main).unwrap();
        git_ok(&main, &["init", "-q", "-b", "main"]);
        git_ok(
            &main,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let linked = tmp.path().join("feature-na\u{ef}ve");
        git_ok(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().unwrap(),
            ],
        );
        Repo {
            store: Store::at(tmp.path().join("home")),
            main: dunce::canonicalize(&main).unwrap(),
            linked: dunce::canonicalize(&linked).unwrap(),
            _tmp: tmp,
        }
    }

    fn worktree(r: &Repo) -> Worktree {
        Worktree::resolve(Some(r.linked.as_os_str())).unwrap()
    }

    fn run(r: &Repo, config: &str) -> Result<Report> {
        let config = Config::parse(config).unwrap();
        up(&r.store, &worktree(r), &config, &launcher(), |_| {
            panic!("lock is busy")
        })
    }

    /// TOML array printing its arguments joined by spaces, using what every CI runner has.
    fn echo(text: &str) -> String {
        if cfg!(windows) {
            format!("['cmd', '/d', '/c', 'echo {text}']")
        } else {
            format!("['echo', '{text}']")
        }
    }

    fn resources(r: &Repo) -> Vec<ResourceState> {
        r.store.resources(&pod::pod_id(&r.linked)).unwrap()
    }

    fn config(extra: &str) -> String {
        format!(
            "version = 1\n\
             [port.web]\nrange = [41000, 41999]\n\
             [[file]]\npath = '.env'\nmode = 'template'\n\
             [[file]]\npath = 'optional.txt'\nmode = 'copy'\noptional = true\n\
             [[command]]\nname = 'app'\ncreate = {}\ndestroy = ['x', '{{{{command.app.stdout}}}}']\n\
             {extra}",
            echo("app-{{pod.id}}")
        )
    }

    #[test]
    fn provisions_in_order_with_builtin_variables() {
        let r = repo();
        fs::write(
            r.main.join(".env"),
            "URL=http://localhost:{{port.web}}\nBRANCH={{branch}}\nWT={{worktree}}\nMAIN={{main}}\n",
        )
        .unwrap();
        let extra = format!(
            "[[command]]\nname = 'uses'\ncreate = {}\n",
            echo("{{command.app.stdout}}:{{port.web}}")
        );
        let report = run(&r, &config(&extra)).unwrap();

        let id = pod::pod_id(&r.linked);
        assert_eq!(report.pod.id, id);
        let port = report.items[0].detail.clone();
        let env = fs::read_to_string(r.linked.join(".env")).unwrap();
        assert_eq!(
            env,
            format!(
                "URL=http://localhost:{port}\nBRANCH=feature\nWT={}\nMAIN={}\n",
                r.linked.display(),
                r.main.display()
            )
        );
        let summary: Vec<(Kind, &str, &str)> = report
            .items
            .iter()
            .map(|i| (i.kind, i.name.as_str(), i.detail.as_str()))
            .collect();
        let app = format!("app-{id}");
        let uses = format!("{app}:{port}");
        assert_eq!(
            summary,
            [
                (Kind::Port, "web", port.as_str()),
                (Kind::File, ".env", "template"),
                (Kind::File, "optional.txt", "skipped (optional, no source)"),
                (Kind::Command, "app", app.as_str()),
                (Kind::Command, "uses", uses.as_str()),
            ]
        );

        // The ledger has every resource live, with what teardown needs.
        let state = resources(&r);
        let names: Vec<_> = state.iter().map(|s| (s.kind, s.name.as_str())).collect();
        assert_eq!(
            names,
            [
                (Kind::Port, "web"),
                (Kind::File, ".env"),
                (Kind::Command, "app"),
                (Kind::Command, "uses"),
            ]
        );
        assert!(state.iter().all(|s| s.status == Status::Live));
        assert_eq!(state[0].data["port"].to_string(), port);
        let app_record: commands::Record = serde_json::from_value(state[2].data.clone()).unwrap();
        assert_eq!(app_record.destroy.unwrap(), ["x".to_owned(), app.clone()]);
        let file_record: files::Record = serde_json::from_value(state[1].data.clone()).unwrap();
        assert_eq!(file_record.path, r.linked.join(".env"));

        // The pod is found by its worktree, and the port is registered to it.
        let meta = r.store.find_by_worktree(&r.linked).unwrap().unwrap();
        assert_eq!(meta.main_checkout.as_deref(), Some(r.main.as_path()));
        let registered = Registry::new(r.store.home())
            .registered(&id, "web")
            .unwrap();
        assert_eq!(registered.map(|p| p.to_string()), Some(port));
    }

    #[test]
    fn a_second_up_fails_without_touching_anything() {
        let r = repo();
        fs::write(r.main.join(".env"), "A=1\n").unwrap();
        run(&r, &config("")).unwrap();
        let ledger = fs::read(r.store.ledger_path(&pod::pod_id(&r.linked))).unwrap();

        let err = format!("{:#}", run(&r, &config("")).unwrap_err());
        assert!(
            err.contains("already has resources") && err.contains("bp down"),
            "{err}"
        );
        assert!(err.contains("port") && err.contains("live"), "{err}");
        let after = fs::read(r.store.ledger_path(&pod::pod_id(&r.linked))).unwrap();
        assert_eq!(ledger, after, "nothing recorded by the refused run");
    }

    #[test]
    fn a_torn_down_pod_comes_up_again() {
        let r = repo();
        let text = format!(
            "version = 1\n[port.web]\nrange = [41000, 41999]\n\
             [[command]]\nname = 'app'\ncreate = {}\n",
            echo("x")
        );
        let first = run(&r, &text).unwrap();
        // What `bp down` leaves behind: every resource destroyed.
        let id = pod::pod_id(&r.linked);
        let mut ledger = Ledger::open(&r.store.ledger_path(&id)).unwrap();
        for s in resources(&r) {
            ledger.record(s.kind, &s.name, Event::Destroyed).unwrap();
        }
        drop(ledger);

        let second = run(&r, &text).unwrap();
        assert_eq!(second.items, first.items);
        assert!(resources(&r).iter().all(|s| s.status == Status::Live));
    }

    #[test]
    fn a_failure_keeps_what_was_created_and_points_to_down() {
        let r = repo();
        fs::write(r.main.join(".env"), "A=1\n").unwrap();
        let extra = format!(
            "[[command]]\nname = 'broken'\ncreate = {}\n",
            serde_json::to_string(&fail(3, "boom")).unwrap()
        );
        let err = run(&r, &config(&extra)).unwrap_err();

        let failed = err.downcast_ref::<Failed>().expect("typed failure");
        let statuses: Vec<_> = failed
            .recorded
            .iter()
            .map(|s| (s.name.as_str(), s.status))
            .collect();
        assert_eq!(
            statuses,
            [
                ("web", Status::Live),
                (".env", Status::Live),
                ("app", Status::Live),
                ("broken", Status::Failed),
            ]
        );
        let message = format!("{err:#}");
        assert!(message.contains("boom"), "{message}");
        assert!(message.contains("bp down"), "{message}");
        assert!(
            message.contains(&r.linked.display().to_string()),
            "{message}"
        );
        // Nothing was rolled back.
        assert!(r.linked.join(".env").exists());

        // And the pod is not brought up over it.
        let again = format!("{:#}", run(&r, &config(&extra)).unwrap_err());
        assert!(again.contains("already has resources"), "{again}");
    }

    #[test]
    fn a_failure_before_any_intent_records_nothing() {
        let r = repo();
        // The source is missing: the file is rejected while planning, before its intent.
        let text = "version = 1\n[[file]]\npath = '.env'\nmode = 'copy'\n";
        let err = run(&r, text).unwrap_err();
        assert!(err.downcast_ref::<Failed>().is_none(), "{err:#}");
        assert!(format!("{err:#}").contains("does not exist"), "{err:#}");
        assert!(resources(&r).is_empty());
    }

    #[test]
    fn unavailable_builtins_explain_why() {
        let r = repo();
        git_ok(&r.linked, &["checkout", "-q", "--detach"]);
        let text = format!(
            "version = 1\n[[command]]\nname = 'app'\ncreate = {}\n",
            echo("{{branch}}")
        );
        let err = format!("{:#}", run(&r, &text).unwrap_err());
        assert!(err.contains("HEAD is detached"), "{err}");
        assert!(resources(&r).is_empty());
    }

    #[test]
    fn mistakes_anywhere_in_the_config_fail_before_anything_is_created() {
        let r = repo();
        fs::write(r.main.join(".env"), "URL=localhost:{{port.web}}\n").unwrap();
        let typo = format!(
            "[[command]]\nname = 'late'\ncreate = {}\n",
            echo("{{command.app.stdout}}-{{port.wbe}}")
        );
        let missing = "[[file]]\npath = 'absent.txt'\nmode = 'copy'\n";
        for (extra, expected) in [(typo.as_str(), "port.wbe"), (missing, "does not exist")] {
            let err = format!("{:#}", run(&r, &config(extra)).unwrap_err());
            assert!(err.contains(expected), "{err}");
            assert!(resources(&r).is_empty(), "{err}");
            assert!(!r.linked.join(".env").exists());
            let registry = Registry::new(r.store.home());
            assert!(registry.list().unwrap().is_empty(), "no port allocated");
        }
    }

    #[test]
    fn the_main_checkout_is_refused_when_files_would_be_carried_over() {
        let r = repo();
        fs::write(r.main.join(".env"), "A=1\n").unwrap();
        let wt = Worktree::resolve(Some(r.main.as_os_str())).unwrap();
        let config = Config::parse(&config("")).unwrap();
        let launcher = launcher();
        let err = format!(
            "{:#}",
            up(&r.store, &wt, &config, &launcher, |_| panic!()).unwrap_err()
        );
        assert!(err.contains("is the main checkout"), "{err}");
        assert!(r.store.list().unwrap().is_empty());
    }

    #[test]
    fn a_stale_port_registration_is_not_trusted() {
        let r = repo();
        let id = pod::pod_id(&r.linked);
        // Left behind by a ledger that was lost: a port that someone else now listens on.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = listener.local_addr().unwrap().port();
        if busy == u16::MAX {
            return;
        }
        let range = [busy, busy + 1];
        // Register the busy port for the pod the way an earlier `up` would have.
        let text = format!(
            "version = 1\n[port.web]\nrange = [{}, {}]\n",
            range[0], range[1]
        );
        fs::create_dir_all(r.store.home()).unwrap();
        fs::write(
            r.store.home().join("ports.json"),
            format!(r#"{{"v":1,"ports":{{"{busy}":{{"pod":"{id}","name":"web"}}}}}}"#),
        )
        .unwrap();
        match run(&r, &text) {
            Ok(report) => assert_ne!(report.items[0].detail, busy.to_string()),
            // The neighbour may be busy on this machine too; the stale port is never handed out.
            Err(e) => assert!(format!("{e:#}").contains("no free port"), "{e:#}"),
        }
    }

    #[test]
    fn hints_quote_the_path_for_the_shell() {
        if cfg!(windows) {
            assert_eq!(quote(Path::new(r"C:\a b")), r#""C:\a b""#);
        } else {
            assert_eq!(quote(Path::new("/a $b/it's")), r"'/a $b/it'\''s'");
        }
    }

    fn service_config(name: &str, run: &[String]) -> String {
        format!(
            "[[service]]\nname = '{name}'\nrun = {}\nenv = {{ BP_PORT = '{{{{port.web}}}}' }}\n",
            serde_json::to_string(run).unwrap()
        )
    }

    #[test]
    fn services_run_under_a_supervisor_until_down() {
        let r = repo();
        let text = format!(
            "version = 1\n[port.web]\nrange = [41000, 41999]\n{}",
            service_config("web", &long_running("web-up"))
        );
        let result = run(&r, &text);
        let _reaper = reap_pod(&r.store, &pod::pod_id(&r.linked));
        let report = result.unwrap();
        let kinds: Vec<(Kind, &str)> = report
            .items
            .iter()
            .map(|i| (i.kind, i.name.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                (Kind::Port, "web"),
                (Kind::Supervisor, "supervisor"),
                (Kind::Service, "web"),
            ]
        );

        let state = resources(&r);
        assert!(state.iter().all(|s| s.status == Status::Live), "{state:?}");
        let sup: supervisor::Record = serde_json::from_value(state[1].data.clone()).unwrap();
        let web: services::Record = serde_json::from_value(state[2].data.clone()).unwrap();
        let (sup_process, web_process) = (sup.process.unwrap(), web.process.unwrap());
        assert!(crate::proc::is_running(sup_process));
        assert!(crate::proc::is_running(web_process));
        assert_eq!(web.cwd, r.linked);
        // The environment is passed to the service but not recorded.
        assert!(!state[2].data.to_string().contains("BP_PORT"));

        let meta = r.store.find_by_worktree(&r.linked).unwrap().unwrap();
        let log = services::log_path(&r.store.pod_dir(&meta.id), "web");
        for _ in 0..400 {
            if fs::read_to_string(&log).is_ok_and(|s| s.contains("web-up")) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(fs::read_to_string(&log).unwrap().contains("web-up"));

        let teardown = crate::down::teardown(&r.store, &meta).unwrap();
        assert!(teardown.is_clean(), "{teardown:?}");
        let order: Vec<&str> = teardown.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(order, ["web", "supervisor", "web"]);
        assert!(!crate::proc::is_running(web_process));
        assert!(!crate::proc::is_running(sup_process));
        assert!(!r.store.pod_dir(&meta.id).exists());
    }

    #[test]
    fn a_pod_whose_supervisor_was_killed_goes_down_and_up_again() {
        let r = repo();
        let text = format!(
            "version = 1\n[port.web]\nrange = [41000, 41999]\n{}",
            service_config("web", &long_running("web-up"))
        );
        let process = |kind| -> ProcId {
            let state = resources(&r);
            let found = state.iter().rfind(|s| s.kind == kind).unwrap();
            serde_json::from_value(found.data["process"].clone()).unwrap()
        };
        let down = || {
            let meta = r.store.find_by_worktree(&r.linked).unwrap().unwrap();
            let teardown = crate::down::teardown(&r.store, &meta).unwrap();
            assert!(teardown.is_clean(), "{teardown:?}");
        };

        let result = run(&r, &text);
        let _reaper = reap_pod(&r.store, &pod::pod_id(&r.linked));
        result.unwrap();
        let (sup, web) = (process(Kind::Supervisor), process(Kind::Service));
        // `down` right after it, without waiting for anything to be gone.
        crate::proc::kill(sup).unwrap();
        down();
        assert!(!crate::proc::is_running(sup));
        assert!(!crate::proc::is_running(web));

        let result = run(&r, &text);
        let _reaper = reap_pod(&r.store, &pod::pod_id(&r.linked));
        result.unwrap();
        assert!(crate::proc::is_running(process(Kind::Service)));
        down();
    }

    #[test]
    fn a_service_that_cannot_start_is_left_for_down() {
        let r = repo();
        let text = format!(
            "version = 1\n[port.web]\nrange = [41000, 41999]\n{}",
            service_config("broken", &["bp-no-such-program".to_owned()])
        );
        let result = run(&r, &text);
        let _reaper = reap_pod(&r.store, &pod::pod_id(&r.linked));
        let err = result.unwrap_err();
        let failed = err.downcast_ref::<Failed>().expect("typed failure");
        let statuses: Vec<_> = failed.recorded.iter().map(|s| (s.kind, s.status)).collect();
        assert_eq!(
            statuses,
            [
                (Kind::Port, Status::Live),
                (Kind::Supervisor, Status::Live),
                (Kind::Service, Status::Failed),
            ]
        );
        assert!(format!("{err:#}").contains("program not found"), "{err:#}");

        let meta = r.store.find_by_worktree(&r.linked).unwrap().unwrap();
        let teardown = crate::down::teardown(&r.store, &meta).unwrap();
        assert!(teardown.is_clean(), "{teardown:?}");
    }

    #[test]
    fn service_mistakes_fail_before_anything_is_created() {
        let r = repo();
        let text = format!(
            "version = 1\n{}",
            service_config("web", &["x".to_owned(), "{{port.nope}}".to_owned()])
        );
        let err = format!("{:#}", run(&r, &text).unwrap_err());
        assert!(
            err.contains("port.nope") || err.contains("port.web"),
            "{err}"
        );
        assert!(resources(&r).is_empty());
    }

    #[test]
    fn output_files_of_an_earlier_pod_are_removed_only_when_nothing_needs_them() {
        let r = repo();
        let text = format!(
            "version = 1\n[[command]]\nname = 'NAME'\noutput = 'file'\ncreate = {}\n",
            echo("x")
        );
        run(&r, &text.replace("NAME", "old")).unwrap();
        let id = pod::pod_id(&r.linked);
        let outputs = r.store.pod_dir(&id).join("outputs");
        let names = || -> Vec<String> {
            fs::read_dir(&outputs)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        };

        // Still needed by the live pod.
        run(&r, &text.replace("NAME", "new")).unwrap_err();
        assert_eq!(names(), ["old"]);

        // What `bp down` leaves when it cannot remove the state: everything destroyed. The command
        // has another name by now.
        let mut ledger = Ledger::open(&r.store.ledger_path(&id)).unwrap();
        ledger
            .record(Kind::Command, "old", Event::Destroyed)
            .unwrap();
        drop(ledger);
        run(&r, &text.replace("NAME", "new")).unwrap();
        assert_eq!(names(), ["new"]);
    }

    #[test]
    fn output_files_that_cannot_be_removed_stop_up() {
        let r = repo();
        let id = pod::pod_id(&r.linked);
        let pod_dir = r.store.pod_dir(&id);
        fs::create_dir_all(&pod_dir).unwrap();
        // Not a directory, so it is not removed as one.
        fs::write(pod_dir.join("outputs"), "x").unwrap();
        let text = format!(
            "version = 1\n[[command]]\nname = 'app'\ncreate = {}\n",
            echo("x")
        );
        let err = format!("{:#}", run(&r, &text).unwrap_err());
        assert!(
            err.contains("cannot remove the command output") && err.contains("outputs"),
            "{err}"
        );
        assert!(resources(&r).is_empty());
    }

    #[test]
    fn long_or_multiline_output_is_shortened() {
        assert_eq!(command_detail(""), "(no output)");
        assert_eq!(command_detail("abc"), "abc");
        assert_eq!(command_detail("a\nb"), "(2 lines of output)");
        let long = "x".repeat(100);
        assert_eq!(command_detail(&long), format!("{}...", "x".repeat(64)));
    }
}
