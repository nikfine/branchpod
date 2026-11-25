//! Command resources: external resources (containers, volumes, sync sessions) managed by a
//! configured `create` command and undone by its `destroy` command.
//!
//! Like file resources, creation is split in two for the write-ahead ledger: [`plan`] renders
//! the argument arrays without running anything and yields the intent [`Record`]; [`create`] runs
//! the command and returns the final record. The record carries `destroy` fully rendered, with
//! the captured output substituted, so teardown needs neither the config nor the worktree: it
//! runs after a restart, from another branch, or with the worktree already gone.
//!
//! Processes are spawned from an argument array, never through a shell. Output is decoded as
//! UTF-8, never with the console code page, and invalid UTF-8 is an error.
//!
//! With `output = "file"` the output is kept byte for byte in a file of the pod's state instead.
//! It may hold secrets, so it goes nowhere else: not into the ledger, the printed result, an error
//! or a log. The path is known beforehand and recorded in the intent; the file goes with the pod's
//! state, after every `destroy` that may read it.
//!
//! Recording in the ledger is the caller's job (`bp up` / `bp down`).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{CommandSpec, OutputMode};
use crate::fsutil;
use crate::vars::{self, Vars};

/// Lines of stderr quoted in an error message; the full text goes to the run log.
const STDERR_TAIL_LINES: usize = 20;

/// Ledger data of a command resource: everything teardown needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// The `create` command as it was run.
    pub(crate) create: Vec<String>,
    /// Trimmed stdout of `create`, the value of `{{command.<name>.stdout}}`. Set once created;
    /// never set with `output = "file"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stdout: Option<String>,
    /// With `output = "file"`, the file holding stdout: the value of `{{command.<name>.file}}`
    /// (hence a string). Known at intent time; the contents are never recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) file: Option<String>,
    /// The `destroy` command, fully rendered. Known at intent time unless it references this
    /// command's own output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) destroy: Option<Vec<String>>,
    /// The `destroy` command as configured, kept while it cannot be rendered because the output it
    /// references was never captured (an interrupted or failed creation) or could not be used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) destroy_unresolved: Option<Vec<String>>,
}

/// A rendered command resource, ready to be created.
#[derive(Debug)]
pub(crate) struct Plan {
    pub(crate) name: String,
    /// Ledger data for the intent.
    pub(crate) record: Record,
    cwd: PathBuf,
    vars: Vars,
}

/// Successful run of `create` or `destroy`. Its stderr is not kept until there is a run log (#9).
#[derive(Debug)]
pub(crate) struct Output {
    /// Ledger data (for `create`; the input record for `destroy`).
    pub(crate) record: Record,
}

/// A command ran and exited unsuccessfully. Kept as a typed error so that the run log can
/// write the full stderr; the message quotes only its tail.
#[derive(Debug)]
pub(crate) struct Failed {
    pub(crate) program: String,
    pub(crate) status: ExitStatus,
    pub(crate) stderr: String,
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} exited with {}{}",
            self.program,
            self.status,
            stderr_tail(&self.stderr)
        )
    }
}

impl std::error::Error for Failed {}

/// `"; stderr:"` followed by the last lines of `stderr`, indented; empty if there is none.
fn stderr_tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.trim_end().lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut out = String::from("; stderr:");
    for line in &lines[lines.len().saturating_sub(STDERR_TAIL_LINES)..] {
        out.push_str("\n  ");
        out.push_str(line);
    }
    out
}

/// The variable holding the captured output of command `name`.
fn stdout_var(name: &str) -> String {
    format!("command.{name}.stdout")
}

/// The variable holding the path of the file with the output of command `name`.
fn file_var(name: &str) -> String {
    format!("command.{name}.file")
}

fn outputs_dir(pod_dir: &Path) -> PathBuf {
    pod_dir.join("outputs")
}

/// Removes the output files of an earlier `up`, whatever its commands were called. The caller has
/// made sure that nothing is left whose `destroy` may read them.
pub(crate) fn remove_outputs(pod_dir: &Path) -> Result<()> {
    let dir = outputs_dir(pod_dir);
    match fsutil::retry(|| fs::remove_dir_all(&dir)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e).with_context(|| {
            format!(
                "cannot remove the command output of an earlier `bp up` in {}",
                dir.display()
            )
        }),
        _ => Ok(()),
    }
}

/// Where the output of command `name` goes with `output = "file"`, in the pod directory `pod_dir`.
fn output_path(pod_dir: &Path, name: &str) -> Result<String> {
    // The `bp` home may be given as a relative path, and the commands run elsewhere.
    let path = std::path::absolute(outputs_dir(pod_dir))
        .with_context(|| format!("cannot resolve {}", pod_dir.display()))?
        .join(name);
    match path.to_str() {
        Some(text) => Ok(text.to_owned()),
        None => bail!(
            "output = \"file\": the path {} is not valid UTF-8",
            path.display()
        ),
    }
}

/// Makes the output of a created command available to later resources: as
/// `{{command.<name>.stdout}}`, or its file as `{{command.<name>.file}}`.
///
/// Empty output defines no variable, so a reference to it is an unknown name and fails when
/// planning, instead of rendering an empty argument (`docker rm ""`, `bp-`).
pub(crate) fn export(name: &str, record: &Record, vars: &mut Vars) {
    if let Some(file) = &record.file {
        vars.set(file_var(name), file.as_str());
        vars.set_unavailable(
            stdout_var(name),
            format!(
                "command {name:?} has output = \"file\", which keeps its output in a file only; \
                 use `{}`",
                file_var(name)
            ),
        );
        return;
    }
    vars.set_unavailable(
        file_var(name),
        format!("command {name:?} does not have output = \"file\""),
    );
    if let Some(stdout) = record.stdout.as_deref().filter(|s| !s.is_empty()) {
        vars.set(stdout_var(name), stdout);
    }
}

/// Makes a planned command available to later resources, with a stand-in for output that only
/// `create` yields.
pub(crate) fn export_planned(plan: &Plan, vars: &mut Vars) {
    let mut record = plan.record.clone();
    record.stdout = Some("placeholder".to_owned());
    export(&plan.name, &record, vars);
}

/// Renders a command resource. `create` runs in `cwd` (the worktree root); `pod_dir` is the pod's
/// state directory, where the file of `output = "file"` goes.
///
/// `destroy` is rendered now when it does not depend on this command's own output, so that an
/// interrupted creation can still be undone from the ledger alone.
pub(crate) fn plan(spec: &CommandSpec, cwd: &Path, vars: &Vars, pod_dir: &Path) -> Result<Plan> {
    plan_inner(spec, cwd, vars, pod_dir).with_context(|| format!("command {:?}", spec.name))
}

/// A `destroy` argument with a line break carries more than an id: most likely a warning printed
/// next to it by an earlier command. One with a NUL is garbled (UTF-16 output) and no process can
/// get it. Teardown must not run with either. The values are checked before filters: `json` would
/// turn these characters into escapes and hide them.
fn render_destroy(vars: &Vars, argv: &[String]) -> Result<Vec<String>> {
    let unusable = |text: &str| text.contains(['\n', '\r', '\0']);
    let rendered = vars.render_all(argv).context("destroy")?;
    for (configured, arg) in argv.iter().zip(&rendered) {
        let in_value = vars::names(configured)
            .filter_map(|name| vars.get(name))
            .any(unusable);
        if in_value || unusable(arg) {
            bail!(
                "destroy: argument {arg:?} spans several lines or contains a NUL character; \
                 teardown needs a single-line value without one"
            );
        }
    }
    Ok(rendered)
}

fn plan_inner(spec: &CommandSpec, cwd: &Path, vars: &Vars, pod_dir: &Path) -> Result<Plan> {
    let own = stdout_var(&spec.name);
    for own in [&own, &file_var(&spec.name)] {
        if spec.create.iter().any(|arg| vars::references(arg, own)) {
            bail!("`create` cannot reference its own output `{own}`");
        }
    }
    let create = vars.render_all(&spec.create).context("create")?;
    let mut record = Record {
        create,
        stdout: None,
        file: match spec.output {
            OutputMode::Stdout => None,
            OutputMode::File => Some(output_path(pod_dir, &spec.name)?),
        },
        destroy: None,
        destroy_unresolved: None,
    };

    // Its own `destroy` may reference the file, whose path is known already.
    let mut vars = vars.clone();
    export(&spec.name, &record, &mut vars);

    let captured = record.file.is_none();
    if spec.start.is_some() {
        // Not rendered until it is run, but a mistake in it is found now.
        let mut probe = vars.clone();
        if captured {
            probe.set(own.as_str(), "x");
        }
        render_start(spec, &probe)?;
    }
    (record.destroy, record.destroy_unresolved) = match &spec.destroy {
        None => (None, None),
        Some(argv) if captured && argv.iter().any(|arg| vars::references(arg, &own)) => {
            // Check every other reference now rather than after the resource exists.
            let mut probe = vars.clone();
            probe.set(own.as_str(), "x");
            render_destroy(&probe, argv)?;
            (None, Some(argv.clone()))
        }
        Some(argv) => (Some(render_destroy(&vars, argv)?), None),
    };

    Ok(Plan {
        name: spec.name.clone(),
        record,
        cwd: cwd.to_path_buf(),
        vars,
    })
}

/// Runs `create` and returns the final record for the ledger. A non-zero exit is an error
/// ([`Failed`]); so is output that is not UTF-8, or output that `destroy` depends on and that is
/// empty or spans several lines (a warning printed next to the id would become part of it).
///
/// With `output = "file"` the output is written to the planned file, whatever it is, unless it is
/// blank, which is an error: a reader would take the empty file for the real thing.
pub(crate) fn create(plan: &Plan) -> Result<Output> {
    create_inner(plan).with_context(|| format!("command {:?}: create", plan.name))
}

/// The command succeeded, so the resource may exist although creation is reported as failed.
const MAY_EXIST: &str = "the resource may have been created; `bp gc` finds it by label";

fn create_inner(plan: &Plan) -> Result<Output> {
    if let Some(file) = &plan.record.file {
        return create_file(plan, Path::new(file));
    }
    let run = run(&plan.record.create, &plan.cwd)?;
    // stderr of a command that succeeded best explains output that cannot be used.
    let stdout = decode_stdout(run.stdout)
        .with_context(|| format!("{MAY_EXIST}{}", stderr_tail(&run.stderr)))?;

    let mut record = plan.record.clone();
    if let Some(argv) = record.destroy_unresolved.take() {
        let problem = if stdout.is_empty() {
            Some("printed nothing")
        } else if stdout.contains('\n') {
            Some("printed several lines")
        } else {
            None
        };
        if let Some(problem) = problem {
            bail!(
                "{problem}, but `destroy` needs a single-line output; {MAY_EXIST}{}",
                stderr_tail(&run.stderr)
            );
        }
        let mut vars = plan.vars.clone();
        vars.set(stdout_var(&plan.name), stdout.as_str());
        match render_destroy(&vars, &argv) {
            Ok(destroy) => record.destroy = Some(destroy),
            Err(e) => bail!("{e:#}; {MAY_EXIST}{}", stderr_tail(&run.stderr)),
        }
    }
    record.stdout = Some(stdout);
    Ok(Output { record })
}

fn create_file(plan: &Plan, file: &Path) -> Result<Output> {
    // Before the command runs: a failure here leaves nothing behind.
    if let Some(dir) = file.parent() {
        fsutil::create_private_dir(dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }

    // From here on errors must not show stdout.
    let run = run(&plan.record.create, &plan.cwd)?;
    if is_blank(&run.stdout) {
        bail!(
            "printed nothing to keep in a file (output = \"file\"){}",
            stderr_tail(&run.stderr)
        );
    }
    fsutil::write_atomic_private(file, &run.stdout)
        .with_context(|| format!("cannot write its output to {}", file.display()))?;
    Ok(Output {
        record: plan.record.clone(),
    })
}

/// Whether output is what [`decode_stdout`] makes an empty string. Tools end even an empty result
/// with a line break. Output that is not UTF-8 is not blank.
fn is_blank(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|text| trim(text).is_empty())
}

/// Runs the recorded `destroy` in `cwd`: the worktree root while it exists, otherwise any
/// directory of `bp`'s own. A resource without a `destroy` command has nothing to undo.
///
/// Fails when `destroy` depends on output that was never captured: the resource may exist and
/// is left for `bp gc`.
pub(crate) fn destroy(record: &Record, cwd: &Path) -> Result<Output> {
    let Some(argv) = &record.destroy else {
        if let Some(argv) = &record.destroy_unresolved {
            bail!(
                "cannot run destroy {argv:?}: the output it needs was never captured or could \
                 not be used; the resource may still exist, `bp gc` finds it by label"
            );
        }
        return Ok(Output {
            record: record.clone(),
        });
    };
    run(argv, cwd).context("destroy")?;
    Ok(Output {
        record: record.clone(),
    })
}

/// The `start` of a created command whose output was exported to `vars`; empty without one.
pub(crate) fn render_start(spec: &CommandSpec, vars: &Vars) -> Result<Vec<String>> {
    let argv = spec.start.as_deref().unwrap_or_default();
    vars.render_all(argv).context("start")
}

/// Runs the rendered `start` of command `name` in `cwd`, the worktree root. Its output is not used.
pub(crate) fn start(name: &str, argv: &[String], cwd: &Path) -> Result<()> {
    run(argv, cwd)
        .map(drop)
        .with_context(|| format!("command {name:?}: start"))
}

struct Run {
    stdout: Vec<u8>,
    stderr: String,
}

/// Runs `argv` without a shell, stdin closed, stdout and stderr captured. `cwd` must be
/// absolute: a relative program is resolved against it, and Unix would resolve the result again
/// after changing into `cwd`.
fn run(argv: &[String], cwd: &Path) -> Result<Run> {
    debug_assert!(cwd.is_absolute(), "relative cwd {}", cwd.display());
    let (program, args) = argv.split_first().context("empty argument array")?;
    if !cwd.is_dir() {
        bail!(
            "cannot run {program}: directory {} does not exist",
            cwd.display()
        );
    }
    let output = Command::new(resolve_program(program, cwd))
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| spawn_error(program, e))?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(Failed {
            program: program.clone(),
            status: output.status,
            stderr,
        }
        .into());
    }
    Ok(Run {
        stdout: output.stdout,
        stderr,
    })
}

/// A program given as a relative path with a directory part (`./scripts/up.sh`,
/// `tools\up.cmd`) is resolved against `cwd` here: Unix would do so, Windows would resolve it
/// against `bp`'s own working directory, and the same config must mean the same thing on every
/// platform. A bare name (`docker`) is left to the `PATH` search.
pub(crate) fn resolve_program(program: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(program);
    let relative = path.components().all(|c| {
        matches!(
            c,
            Component::Normal(_) | Component::CurDir | Component::ParentDir
        )
    });
    if !relative || path.components().count() < 2 {
        return path.to_path_buf();
    }
    let mut resolved = cwd.to_path_buf();
    for component in path.components() {
        if component != Component::CurDir {
            resolved.push(component);
        }
    }
    resolved
}

pub(crate) fn spawn_error(program: &str, e: io::Error) -> anyhow::Error {
    if e.kind() == io::ErrorKind::NotFound {
        // Windows does not resolve `npx` to `npx.cmd` by PATHEXT when spawning without a shell.
        let hint = if cfg!(windows) && Path::new(program).extension().is_none() {
            " (a .cmd or .bat script must be named with its extension, e.g. `npx.cmd`)"
        } else {
            ""
        };
        return anyhow::anyhow!("cannot run {program}: program not found{hint}");
    }
    anyhow::Error::new(e).context(format!("cannot run {program}"))
}

/// UTF-8, see [`trim`].
fn decode_stdout(bytes: Vec<u8>) -> Result<String> {
    let text = String::from_utf8(bytes).map_err(|e| {
        anyhow::anyhow!(
            "output is not valid UTF-8 (invalid byte at offset {})",
            e.utf8_error().valid_up_to()
        )
    })?;
    Ok(trim(&text).to_owned())
}

/// Without a leading byte order mark (Windows PowerShell writes one) and surrounding whitespace,
/// including the trailing newline.
fn trim(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text).trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fail;

    /// With the working directory as the pod's state directory, which only `output = "file"` uses.
    fn plan(spec: &CommandSpec, cwd: &Path, vars: &Vars) -> Result<Plan> {
        super::plan(spec, cwd, vars, cwd)
    }

    /// Prints the contents of `file` byte for byte, using only what every CI runner has.
    fn cat(file: &Path) -> Vec<String> {
        let file = file.to_string_lossy().into_owned();
        if cfg!(windows) {
            vec!["cmd".into(), "/d".into(), "/c".into(), "type".into(), file]
        } else {
            vec!["cat".into(), file]
        }
    }

    fn spec(name: &str, create: Vec<String>, destroy: Option<Vec<String>>) -> CommandSpec {
        CommandSpec {
            name: name.into(),
            create,
            output: OutputMode::Stdout,
            start: None,
            destroy,
            gc: None,
        }
    }

    fn file_spec(name: &str, create: Vec<String>, destroy: Option<Vec<String>>) -> CommandSpec {
        CommandSpec {
            output: OutputMode::File,
            ..spec(name, create, destroy)
        }
    }

    #[test]
    fn start_is_checked_when_planning_and_may_use_the_own_output() {
        let cwd = tempfile::tempdir().unwrap();
        let with_start = |mut spec: CommandSpec, arg: &str| {
            spec.start = Some(strings(&["x", arg]));
            spec
        };
        let create = || strings(&["x"]);
        let own = with_start(spec("app", create(), None), "{{command.app.stdout}}");
        plan(&own, cwd.path(), &vars()).unwrap();
        let mut known = vars();
        known.set("command.app.stdout", "abc");
        assert_eq!(render_start(&own, &known).unwrap(), ["x", "abc"]);

        let file = with_start(file_spec("app", create(), None), "{{command.app.file}}");
        plan(&file, cwd.path(), &vars()).unwrap();
        for wrong in [
            with_start(spec("app", create(), None), "{{command.app.file}}"),
            with_start(file_spec("app", create(), None), "{{command.app.stdout}}"),
            with_start(spec("app", create(), None), "{{port.nope}}"),
        ] {
            let err = format!("{:#}", plan(&wrong, cwd.path(), &vars()).unwrap_err());
            assert!(err.contains("command \"app\": start"), "{err}");
        }
    }

    fn vars() -> Vars {
        let mut v = Vars::new();
        v.set("pod.id", "feature-x-1a2b3c4d");
        v
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn captures_trimmed_output_and_renders_destroy() {
        let dir = tempfile::tempdir().unwrap();
        // A non-ASCII path: the temp directory itself often is one on Windows.
        let out = dir.path().join("вывод ü.txt");
        fs::write(&out, "\u{feff}  abc123 \r\n\r\n").unwrap();

        let destroy = strings(&[
            "docker",
            "rm",
            "-f",
            "{{ command.app.stdout }}",
            "{{pod.id}}",
        ]);
        let plan = plan(&spec("app", cat(&out), Some(destroy)), dir.path(), &vars()).unwrap();
        assert_eq!(plan.record.destroy, None);
        assert!(plan.record.destroy_unresolved.is_some());

        let created = create(&plan).unwrap();
        let record = created.record;
        assert_eq!(record.stdout.as_deref(), Some("abc123"));
        assert_eq!(
            record.destroy.unwrap(),
            strings(&["docker", "rm", "-f", "abc123", "feature-x-1a2b3c4d"])
        );
        assert_eq!(record.destroy_unresolved, None);
    }

    #[test]
    fn go_template_arguments_pass_through() {
        let dir = tempfile::tempdir().unwrap();
        let create = strings(&[
            "docker",
            "inspect",
            "-f",
            "{{{{.Config.Image}}",
            "{{{{command.app.stdout}}",
            "bp-{{pod.id}}",
        ]);
        let destroy = strings(&["docker", "rm", "-f", "--filter", "{{{{command.app.stdout}}"]);
        let plan = plan(&spec("app", create, Some(destroy)), dir.path(), &vars()).unwrap();
        assert_eq!(
            plan.record.create,
            strings(&[
                "docker",
                "inspect",
                "-f",
                "{{.Config.Image}}",
                "{{command.app.stdout}}",
                "bp-feature-x-1a2b3c4d",
            ])
        );
        // An escaped reference to its own output is not one: `destroy` is known at intent time.
        assert_eq!(
            plan.record.destroy.unwrap(),
            strings(&["docker", "rm", "-f", "--filter", "{{command.app.stdout}}"])
        );

        let err = super::plan(
            &spec(
                "app",
                strings(&["docker", "inspect", "-f", "{{.Id}}"]),
                None,
            ),
            dir.path(),
            &vars(),
            dir.path(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("literal"), "{err:#}");
    }

    #[test]
    fn literal_braces_survive_deferred_destroy() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        fs::write(&out, "abc123\n").unwrap();
        let destroy = strings(&["docker", "rm", "{{{{.Id}}={{command.app.stdout}}"]);
        let plan = plan(&spec("app", cat(&out), Some(destroy)), dir.path(), &vars()).unwrap();
        assert!(plan.record.destroy_unresolved.is_some());
        let record = create(&plan).unwrap().record;
        assert_eq!(
            record.destroy.unwrap(),
            strings(&["docker", "rm", "{{.Id}}=abc123"])
        );
    }

    #[test]
    fn several_lines_are_kept_unless_destroy_needs_them() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        fs::write(&out, "abc123\r\nWARNING: something\r\n").unwrap();

        let record = create(&plan(&spec("app", cat(&out), None), dir.path(), &vars()).unwrap())
            .unwrap()
            .record;
        assert_eq!(
            record.stdout.as_deref(),
            Some("abc123\r\nWARNING: something")
        );

        // Another command's teardown must not get it either.
        let mut later = vars();
        export("app", &record, &mut later);
        let destroy = strings(&["docker", "network", "disconnect", "{{command.app.stdout}}"]);
        let err = plan(
            &spec("net", strings(&["x"]), Some(destroy.clone())),
            dir.path(),
            &later,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("several lines"), "{err:#}");
        let own = strings(&[
            "docker",
            "rm",
            "{{command.net.stdout}}",
            "{{command.app.stdout}}",
        ]);
        assert!(plan(&spec("net", strings(&["x"]), Some(own)), dir.path(), &later).is_err());

        let destroy = strings(&["docker", "rm", "{{command.app.stdout}}"]);
        let plan = plan(&spec("app", cat(&out), Some(destroy)), dir.path(), &vars()).unwrap();
        let err = format!("{:#}", create(&plan).unwrap_err());
        assert!(err.contains("several lines"), "{err}");
    }

    #[test]
    fn filters_do_not_hide_unusable_values_from_destroy() {
        let dir = tempfile::tempdir().unwrap();
        let destroy = strings(&["docker", "rm", "{{command.app.stdout | json}}"]);
        for output in [
            "abc123\r\nWARNING: something\r\n",
            "abc123\rWARNING\n",
            "a\0b\n",
        ] {
            // Its own output: rendered after `create`.
            let out = dir.path().join("out.txt");
            fs::write(&out, output).unwrap();
            let own = spec("app", cat(&out), Some(destroy.clone()));
            let err = create(&plan(&own, dir.path(), &vars()).unwrap()).unwrap_err();
            let err = format!("{err:#}");
            assert!(
                err.contains("several lines") && err.contains("may have been created"),
                "{err}"
            );

            // Another command's output: rendered when planning.
            let mut later = vars();
            later.set("command.app.stdout", output.trim());
            let other = spec("net", strings(&["x"]), Some(destroy.clone()));
            let err = format!("{:#}", plan(&other, dir.path(), &later).unwrap_err());
            assert!(err.contains("several lines"), "{err}");
        }
    }

    #[test]
    fn filtered_own_output_is_recognized() {
        let dir = tempfile::tempdir().unwrap();
        let own = strings(&["echo", "{{command.app.stdout | json}}"]);
        let err = plan(&spec("app", own, None), dir.path(), &vars()).unwrap_err();
        assert!(format!("{err:#}").contains("its own output"), "{err:#}");

        let out = dir.path().join("out.txt");
        fs::write(&out, "a\"b\\c\n").unwrap();
        let destroy = strings(&["rm", "{{ command.app.stdout | json }}"]);
        let plan = plan(&spec("app", cat(&out), Some(destroy)), dir.path(), &vars()).unwrap();
        assert!(plan.record.destroy_unresolved.is_some());
        assert_eq!(
            create(&plan).unwrap().record.destroy.unwrap(),
            strings(&["rm", r#"a\"b\\c"#])
        );
    }

    /// Only the real output shows that a filter rejects it: the resource exists by then.
    #[cfg(windows)]
    #[test]
    fn output_rejected_by_a_filter_says_the_resource_may_exist() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        fs::write(&out, "\\\\?\\C:\\x\n").unwrap();
        let destroy = strings(&["rm", "{{command.app.stdout | slash}}"]);
        let plan = plan(&spec("app", cat(&out), Some(destroy)), dir.path(), &vars()).unwrap();
        let err = format!("{:#}", create(&plan).unwrap_err());
        // The cause first; the quoted stderr, if any, comes last.
        let cause = err.find("`slash`").expect(&err);
        assert!(cause < err.find("may have been created").expect(&err));
        let err = format!(
            "{:#}",
            super::destroy(&plan.record, dir.path()).unwrap_err()
        );
        assert!(err.contains("could not be used"), "{err}");
    }

    #[test]
    fn empty_output_defines_no_variable() {
        let mut vars = vars();
        let empty = Record {
            create: strings(&["x"]),
            stdout: Some(String::new()),
            file: None,
            destroy: None,
            destroy_unresolved: None,
        };
        export("net", &empty, &mut vars);
        let dir = tempfile::tempdir().unwrap();
        let destroy = strings(&["docker", "network", "rm", "bp-{{command.net.stdout}}"]);
        let err = plan(
            &spec("app", strings(&["x"]), Some(destroy)),
            dir.path(),
            &vars,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("unknown variable"), "{err:#}");

        let full = Record {
            stdout: Some("n1".into()),
            ..empty
        };
        export("net", &full, &mut vars);
        assert_eq!(vars.get("command.net.stdout"), Some("n1"));
    }

    #[test]
    fn relative_program_is_resolved_against_cwd() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("tools")).unwrap();
        let program = if cfg!(windows) {
            fs::write(
                dir.path().join("tools").join("hi.cmd"),
                "@echo hi-from-tool\r\n",
            )
            .unwrap();
            "./tools/hi.cmd"
        } else {
            let script = dir.path().join("tools").join("hi.sh");
            fs::write(&script, "#!/bin/sh\necho hi-from-tool\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            }
            "./tools/hi.sh"
        };
        // The test process runs elsewhere, so this only passes if `cwd` is used.
        assert!(!std::env::current_dir().unwrap().join("tools").exists());
        let plan = plan(&spec("app", strings(&[program]), None), dir.path(), &vars()).unwrap();
        let record = create(&plan).unwrap().record;
        assert_eq!(record.stdout.as_deref(), Some("hi-from-tool"));
        // Recorded as configured, not as resolved.
        assert_eq!(record.create, strings(&[program]));
    }

    #[test]
    fn program_resolution() {
        let cwd = Path::new("base");
        assert_eq!(resolve_program("docker", cwd), Path::new("docker"));
        assert_eq!(
            resolve_program("./a/b", cwd),
            Path::new("base").join("a").join("b")
        );
        assert_eq!(
            resolve_program("../x", cwd),
            Path::new("base").join("..").join("x")
        );
        if cfg!(windows) {
            assert_eq!(
                resolve_program(r"C:\t\x.exe", cwd),
                Path::new(r"C:\t\x.exe")
            );
            assert_eq!(resolve_program(r"\t\x.exe", cwd), Path::new(r"\t\x.exe"));
        } else {
            assert_eq!(resolve_program("/usr/bin/x", cwd), Path::new("/usr/bin/x"));
        }
    }

    #[test]
    fn missing_cwd_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let record = Record {
            create: strings(&["x"]),
            stdout: None,
            file: None,
            destroy: Some(strings(&["docker", "version"])),
            destroy_unresolved: None,
        };
        let err = format!("{:#}", destroy(&record, &gone).unwrap_err());
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn destroy_without_own_output_is_known_at_intent_time() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        fs::write(&out, "").unwrap();
        let destroy = strings(&["docker", "rm", "-f", "bp-{{pod.id}}"]);
        let plan = plan(&spec("app", cat(&out), Some(destroy)), dir.path(), &vars()).unwrap();
        assert_eq!(
            plan.record.destroy,
            Some(strings(&["docker", "rm", "-f", "bp-feature-x-1a2b3c4d"]))
        );
        // Empty output is fine when nothing depends on it.
        let record = create(&plan).unwrap().record;
        assert_eq!(record.stdout.as_deref(), Some(""));
        assert_eq!(record.destroy, plan.record.destroy);
    }

    #[test]
    fn empty_output_needed_by_destroy_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        fs::write(&out, " \n").unwrap();
        let destroy = strings(&["rm", "{{command.app.stdout}}"]);
        let plan = plan(&spec("app", cat(&out), Some(destroy)), dir.path(), &vars()).unwrap();
        let err = format!("{:#}", create(&plan).unwrap_err());
        assert!(err.contains("printed nothing"), "{err}");
    }

    #[test]
    fn invalid_utf8_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        // "id" in CP866 Cyrillic: what an OEM console would produce.
        fs::write(&out, b"\xa8\xa4\n").unwrap();
        let plan = plan(&spec("app", cat(&out), None), dir.path(), &vars()).unwrap();
        let err = format!("{:#}", create(&plan).unwrap_err());
        assert!(err.contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn non_zero_exit_fails_with_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(&spec("app", fail(3, "boom"), None), dir.path(), &vars()).unwrap();
        let err = create(&plan).unwrap_err();
        let failed = err.downcast_ref::<Failed>().expect("typed error");
        assert_eq!(failed.status.code(), Some(3));
        assert!(failed.stderr.contains("boom"), "{}", failed.stderr);
        let message = format!("{err:#}");
        assert!(
            message.contains("boom") && message.contains("command \"app\""),
            "{message}"
        );
    }

    #[test]
    fn stderr_in_the_message_is_limited_to_its_tail() {
        let failed = Failed {
            program: "x".into(),
            status: fail_status(),
            stderr: (1..=50).map(|i| format!("line {i}\n")).collect(),
        };
        let message = failed.to_string();
        assert!(!message.contains("line 30\n") && message.contains("line 31\n"));
        assert!(message.ends_with("line 50"), "{message}");
    }

    fn fail_status() -> ExitStatus {
        let argv = fail(1, "x");
        Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .unwrap()
            .status
    }

    #[test]
    fn missing_program_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(
            &spec("app", strings(&["bp-no-such-program-7f3a"]), None),
            dir.path(),
            &vars(),
        )
        .unwrap();
        let err = format!("{:#}", create(&plan).unwrap_err());
        assert!(err.contains("program not found"), "{err}");
        if cfg!(windows) {
            assert!(err.contains(".cmd"), "{err}");
        }
    }

    #[test]
    fn plan_rejects_bad_references_before_running() {
        let dir = tempfile::tempdir().unwrap();
        let create = strings(&["echo"]);
        for (create, destroy) in [
            (strings(&["echo", "{{command.app.stdout}}"]), None),
            (strings(&["echo", "{{port.web}}"]), None),
            (
                create.clone(),
                Some(strings(&["rm", "{{command.app.stdout}}", "{{port.web}}"])),
            ),
            (
                create.clone(),
                Some(strings(&["rm", "{{command.later.stdout}}"])),
            ),
        ] {
            assert!(plan(&spec("app", create, destroy), dir.path(), &vars()).is_err());
        }
    }

    #[test]
    fn destroy_runs_the_recorded_command_anywhere() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        fs::write(&out, "x").unwrap();
        let record = Record {
            create: strings(&["unused"]),
            stdout: Some("x".into()),
            file: None,
            destroy: Some(cat(&out)),
            destroy_unresolved: None,
        };
        // The worktree is gone: any existing directory will do.
        let elsewhere = tempfile::tempdir().unwrap();
        destroy(&record, elsewhere.path()).unwrap();

        let failing = Record {
            destroy: Some(fail(2, "gone")),
            ..record.clone()
        };
        let err = destroy(&failing, elsewhere.path()).unwrap_err();
        assert!(err.downcast_ref::<Failed>().is_some(), "{err:#}");
    }

    #[test]
    fn destroy_without_command_or_output() {
        let dir = tempfile::tempdir().unwrap();
        let nothing = Record {
            create: strings(&["x"]),
            stdout: None,
            file: None,
            destroy: None,
            destroy_unresolved: None,
        };
        destroy(&nothing, dir.path()).unwrap();

        let unresolved = Record {
            destroy_unresolved: Some(strings(&["rm", "{{command.app.stdout}}"])),
            ..nothing
        };
        let err = format!("{:#}", destroy(&unresolved, dir.path()).unwrap_err());
        assert!(
            err.contains("never captured") && err.contains("bp gc"),
            "{err}"
        );
    }

    #[test]
    fn record_round_trips_through_the_ledger_format() {
        let record = Record {
            create: strings(&["docker", "run", "-d", "--label", "bp.pod=x"]),
            stdout: Some("abc".into()),
            file: None,
            destroy: Some(strings(&["docker", "rm", "-f", "-v", "abc"])),
            destroy_unresolved: None,
        };
        let json = serde_json::to_value(&record).unwrap();
        assert!(json.get("destroy_unresolved").is_none(), "{json}");
        // Written and read as before `output = "file"`.
        assert!(json.get("file").is_none(), "{json}");
        assert_eq!(serde_json::from_value::<Record>(json).unwrap(), record);
    }

    /// BOM, CRLF, trailing spaces, a NUL and bytes that are not UTF-8.
    const RAW: &[u8] = b"\xef\xbb\xbfTOKEN=s3cr3t \r\nX=\xa8\xa4\0\r\n\n  ";

    #[test]
    fn file_output_is_kept_byte_for_byte_and_recorded_nowhere() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("pod-\u{e9}");
        fs::create_dir(&dir).unwrap();
        let out = dir.join("raw.txt");
        fs::write(&out, RAW).unwrap();

        let destroy = strings(&["rm", "bp-{{pod.id}}", "--env-file", "{{command.env.file}}"]);
        let plan = plan(&file_spec("env", cat(&out), Some(destroy)), &dir, &vars()).unwrap();
        // All of it is known at intent time, its own `destroy` included.
        let file = dir.join("outputs").join("env");
        let text = file.to_str().unwrap();
        assert_eq!(plan.record.file.as_deref(), Some(text));
        assert_eq!(
            plan.record.destroy,
            Some(strings(&[
                "rm",
                "bp-feature-x-1a2b3c4d",
                "--env-file",
                text
            ]))
        );
        assert_eq!(plan.record.destroy_unresolved, None);
        assert!(!file.exists());

        let record = create(&plan).unwrap().record;
        assert_eq!(record, plan.record);
        assert_eq!(fs::read(&file).unwrap(), RAW);
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("s3cr3t"), "{json}");
        assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), record);

        // Later commands get the path, with filters, and are told why there is no stdout.
        let mut later = vars();
        export("env", &record, &mut later);
        let plan = super::plan(
            &spec(
                "app",
                strings(&[
                    "docker",
                    "{{command.env.file}}",
                    "{{command.env.file | slash}}",
                    "{{command.env.file | json}}",
                ]),
                None,
            ),
            &dir,
            &later,
            &dir,
        )
        .unwrap();
        let slash = if cfg!(windows) {
            text.replace('\\', "/")
        } else {
            text.to_owned()
        };
        let json = serde_json::to_string(text).unwrap();
        assert_eq!(
            plan.record.create,
            ["docker", text, &slash, &json[1..json.len() - 1]]
        );
        let uses_stdout = spec("app", strings(&["x", "{{command.env.stdout}}"]), None);
        let err = super::plan(&uses_stdout, &dir, &later, &dir).unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("`command.env.stdout` is not available")
                && err.contains("use `command.env.file`"),
            "{err}"
        );
    }

    #[test]
    fn references_to_the_output_the_command_does_not_have() {
        let dir = tempfile::tempdir().unwrap();
        let error = |spec: &CommandSpec| {
            let err = plan(spec, dir.path(), &vars()).unwrap_err();
            format!("{err:#}")
        };
        let own_file = strings(&["x", "{{command.env.file}}"]);
        let own_stdout = strings(&["x", "{{command.env.stdout}}"]);
        for create in [&own_file, &own_stdout] {
            let err = error(&file_spec("env", create.clone(), None));
            assert!(err.contains("its own output"), "{err}");
        }
        let err = error(&file_spec("env", strings(&["x"]), Some(own_stdout)));
        assert!(err.contains("use `command.env.file`"), "{err}");
        let err = error(&spec("env", strings(&["x"]), Some(own_file)));
        assert!(err.contains("does not have output = \"file\""), "{err}");
    }

    #[test]
    fn blank_output_is_not_kept_in_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.txt");
        let file = dir.path().join("outputs").join("env");
        // Blank means what it means for `stdout`.
        for blank in ["", "\r\n \n", "\u{feff}\r\n", "\u{b}\u{a0}\n"] {
            assert_eq!(decode_stdout(blank.into()).unwrap(), "");
            fs::write(&out, blank).unwrap();
            let spec = file_spec("env", cat(&out), None);
            let err = create(&plan(&spec, dir.path(), &vars()).unwrap()).unwrap_err();
            let err = format!("{err:#}");
            assert!(
                err.contains("printed nothing") && err.contains("command \"env\""),
                "{err}"
            );
            assert!(!file.exists());
        }
        assert!(!is_blank(b"\xa0\n"), "not UTF-8");
    }

    #[test]
    fn a_failing_command_keeps_no_file_and_shows_only_its_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("raw.txt");
        fs::write(&out, RAW).unwrap();
        let script = if cfg!(windows) {
            let script = "type raw.txt & echo boom 1>&2 & exit 3";
            strings(&["cmd", "/d", "/c", script])
        } else {
            strings(&["sh", "-c", "cat raw.txt; echo boom >&2; exit 3"])
        };
        let plan = plan(&file_spec("env", script, None), dir.path(), &vars()).unwrap();
        let err = create(&plan).unwrap_err();
        assert!(err.downcast_ref::<Failed>().is_some(), "{err:#}");
        for text in [format!("{err:#}"), format!("{err:?}")] {
            assert!(text.contains("boom") && !text.contains("s3cr3t"), "{text}");
        }
        assert!(!dir.path().join("outputs").join("env").exists());
    }

    #[test]
    fn the_file_is_prepared_before_the_command_runs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("outputs"), "not a directory").unwrap();
        let script = if cfg!(windows) {
            strings(&["cmd", "/d", "/c", "echo x> ran.txt"])
        } else {
            strings(&["sh", "-c", "echo x > ran.txt"])
        };
        let plan = plan(&file_spec("env", script, None), dir.path(), &vars()).unwrap();
        let err = format!("{:#}", create(&plan).unwrap_err());
        assert!(err.contains("cannot create"), "{err}");
        assert!(!dir.path().join("ran.txt").exists());
    }

    #[test]
    fn the_file_path_is_absolute_even_with_a_relative_home() {
        let dir = tempfile::tempdir().unwrap();
        let spec = file_spec("env", strings(&["x"]), None);
        let plan = super::plan(&spec, dir.path(), &vars(), Path::new("home")).unwrap();
        let file = PathBuf::from(plan.record.file.unwrap());
        assert_eq!(
            file,
            std::env::current_dir()
                .unwrap()
                .join("home")
                .join("outputs")
                .join("env")
        );
    }
}
