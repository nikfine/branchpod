//! `bp up`: provision everything a worktree needs.
//!
//! Under the pod lock, resources are created in a fixed order of kinds — ports, files, commands —
//! and within a kind in config order (ports by name: they never depend on each other). Ports come
//! first because templates and commands reference them; files before commands because a command
//! (a container) may mount a file provisioned for it.
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
use crate::vars::Vars;
use crate::worktree::Worktree;
use crate::{commands, files};

/// A provisioned pod.
#[derive(Debug)]
pub struct Report {
    pub pod: PodMeta,
    /// What this run did, in creation order.
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: Kind,
    pub name: String,
    /// Short human-readable result: the port, the file mode, the command's output.
    pub detail: String,
}

/// `up` failed after recording resources. They may exist and are left for `bp down`.
#[derive(Debug)]
pub struct Failed {
    pub error: anyhow::Error,
    pub pod: String,
    pub worktree: PathBuf,
    /// Every resource of the pod that is not destroyed, as the ledger has it now.
    pub recorded: Vec<ResourceState>,
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:#}\n\nresources recorded for pod {} (they may exist):\n{}\
             run `bp down \"{}\"` to remove them",
            self.error,
            self.pod,
            listing(&self.recorded),
            self.worktree.display()
        )
    }
}

impl std::error::Error for Failed {}

/// Provisions the pod of `wt` from `config`. `on_wait` is called if another `bp` holds the pod
/// lock, before waiting for it.
pub fn up(
    store: &Store,
    wt: &Worktree,
    config: &Config,
    on_wait: impl FnOnce(Option<&Holder>),
) -> Result<Report> {
    if let Some(service) = config.service.first() {
        bail!(
            "service {:?}: services are not supported yet (the supervisor is not implemented)",
            service.name
        );
    }
    let main = wt.main_checkout.as_deref();
    let needs_main = config.file.iter().any(|f| f.mode != FileMode::Ignore);
    if needs_main && main.is_none() {
        bail!("file resources are copied from the main checkout, but the repository is bare");
    }

    let id = pod::pod_id(&wt.root);
    let _lock = store.lock_pod(&id, "up", on_wait)?;

    let existing = live(store.resources(&id)?);
    if !existing.is_empty() {
        bail!(
            "pod {id} already has resources recorded:\n{}\
             run `bp down \"{}\"` first",
            listing(&existing),
            wt.root.display()
        );
    }

    let meta = PodMeta {
        v: META_VERSION,
        id: id.clone(),
        worktree: wt.root.clone(),
        main_checkout: wt.main_checkout.clone(),
    };
    store.write_meta(&meta)?;

    let mut vars = builtin_vars(&id, wt)?;
    let mut ledger = Ledger::open(&store.ledger_path(&id))?;
    let mut items = Vec::new();
    let result = provision(
        store,
        &id,
        wt,
        main,
        config,
        &mut ledger,
        &mut vars,
        &mut items,
    );
    match result {
        Ok(()) => Ok(Report { pod: meta, items }),
        Err(error) => {
            let recorded = store.resources(&id).map(live).unwrap_or_default();
            if recorded.is_empty() {
                return Err(error);
            }
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

#[allow(clippy::too_many_arguments)]
fn provision(
    store: &Store,
    id: &str,
    wt: &Worktree,
    main: Option<&Path>,
    config: &Config,
    ledger: &mut Ledger,
    vars: &mut Vars,
    items: &mut Vec<Item>,
) -> Result<()> {
    // The pod lock is held: the registry lock is taken after it, as the lock order requires.
    let registry = Registry::new(store.home());
    for (name, spec) in &config.port {
        let record = step(ledger, Kind::Port, name, Value::Null, || {
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
        let mode = serde_json::to_value(spec.mode)?
            .as_str()
            .unwrap_or_default()
            .to_owned();
        // Checked against the disk before the intent: a rule that cannot be created records nothing.
        let Some(plan) = files::plan(spec, main, &wt.root, vars)? else {
            items.push(Item {
                kind: Kind::File,
                name,
                detail: "skipped (optional, no source)".into(),
            });
            continue;
        };
        let intent = serde_json::to_value(&plan.record)?;
        step(ledger, Kind::File, &name, intent, || files::create(&plan))?;
        items.push(Item {
            kind: Kind::File,
            name,
            detail: mode,
        });
    }

    for spec in &config.command {
        let plan = commands::plan(spec, &wt.root, vars)?;
        let intent = serde_json::to_value(&plan.record)?;
        // stderr of a successful command is for the run log, which does not exist yet.
        let record = step(ledger, Kind::Command, &spec.name, intent, || {
            commands::create(&plan).map(|out| out.record)
        })?;
        commands::export(&spec.name, &record, vars);
        items.push(Item {
            kind: Kind::Command,
            name: spec.name.clone(),
            detail: command_detail(record.stdout.as_deref().unwrap_or_default()),
        });
    }
    Ok(())
}

/// Records the intent, runs `create` and records its data or its failure.
fn step<T: Serialize>(
    ledger: &mut Ledger,
    kind: Kind,
    name: &str,
    intent: Value,
    create: impl FnOnce() -> Result<T>,
) -> Result<T> {
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

/// `pod.id`, `worktree`, `main` and `branch`. A name that has no value here (a detached HEAD, a
/// bare repository, a path that is not UTF-8) is declared unavailable with the reason, so that a
/// reference to it fails with an explanation instead of rendering something wrong.
fn builtin_vars(id: &str, wt: &Worktree) -> Result<Vars> {
    let mut vars = Vars::new();
    vars.set("pod.id", id);
    path_var(&mut vars, "worktree", &wt.root);
    match &wt.main_checkout {
        Some(main) => path_var(&mut vars, "main", main),
        None => vars.set_unavailable("main", "the repository is bare"),
    }
    match wt.branch()? {
        Some(branch) => vars.set("branch", branch),
        None => vars.set_unavailable("branch", "HEAD is detached"),
    }
    Ok(vars)
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
        out.push_str(&format!(
            "  {:<8} {:<24} {}\n",
            kind_name(r.kind),
            r.name,
            status_name(r.status)
        ));
    }
    out
}

pub fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Git => "git",
        Kind::File => "file",
        Kind::Port => "port",
        Kind::Command => "command",
        Kind::Service => "service",
    }
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Pending => "pending",
        Status::Live => "live",
        Status::Failed => "failed",
        Status::Destroyed => "destroyed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    struct Repo {
        _tmp: tempfile::TempDir,
        store: Store,
        main: PathBuf,
        linked: PathBuf,
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
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
        up(&r.store, &worktree(r), &config, |_| panic!("lock is busy"))
    }

    /// TOML array printing its arguments joined by spaces, using what every CI runner has.
    fn echo(text: &str) -> String {
        if cfg!(windows) {
            format!("['cmd', '/d', '/c', 'echo {text}']")
        } else {
            format!("['echo', '{text}']")
        }
    }

    fn fail() -> String {
        if cfg!(windows) {
            "['cmd', '/d', '/c', 'echo boom 1>&2 & exit 3']".into()
        } else {
            "['sh', '-c', 'echo boom >&2; exit 3']".into()
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
        let extra = format!("[[command]]\nname = 'broken'\ncreate = {}\n", fail());
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
    fn services_are_refused_before_anything_happens() {
        let r = repo();
        let text = "version = 1\n[[service]]\nname = 'vite'\nrun = ['npx.cmd', 'vite']\n";
        let err = format!("{:#}", run(&r, text).unwrap_err());
        assert!(err.contains("not supported yet"), "{err}");
        assert!(r.store.list().unwrap().is_empty());
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
