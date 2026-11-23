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
//! Recording in the ledger is the caller's job (`bp up` / `bp down`).

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::CommandSpec;
use crate::vars::{self, Vars};

/// Lines of stderr quoted in an error message; the full text goes to the run log.
const STDERR_TAIL_LINES: usize = 20;

/// Ledger data of a command resource: everything teardown needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The `create` command as it was run.
    pub create: Vec<String>,
    /// Trimmed stdout of `create`, the value of `{{command.<name>.stdout}}`. Set once created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// The `destroy` command, fully rendered. Known at intent time unless it references this
    /// command's own output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destroy: Option<Vec<String>>,
    /// The `destroy` command as configured, kept while it cannot be rendered yet because the
    /// output it references was never captured (an interrupted or failed creation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destroy_unresolved: Option<Vec<String>>,
}

/// A rendered command resource, ready to be created.
#[derive(Debug)]
pub struct Plan {
    pub name: String,
    /// Ledger data for the intent.
    pub record: Record,
    cwd: PathBuf,
    vars: Vars,
}

/// Successful run of `create` or `destroy`.
#[derive(Debug)]
pub struct Output {
    /// Ledger data (for `create`; the input record for `destroy`).
    pub record: Record,
    /// stderr of the command, decoded lossily, for the run log.
    pub stderr: String,
}

/// A command ran and exited unsuccessfully. Kept as a typed error so that the run log can
/// write the full stderr; the message quotes only its tail.
#[derive(Debug)]
pub struct Failed {
    pub program: String,
    pub status: ExitStatus,
    pub stderr: String,
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} exited with {}", self.program, self.status)?;
        let lines: Vec<&str> = self.stderr.trim_end().lines().collect();
        if !lines.is_empty() {
            let tail = &lines[lines.len().saturating_sub(STDERR_TAIL_LINES)..];
            write!(f, "; stderr:")?;
            for line in tail {
                write!(f, "\n  {line}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for Failed {}

/// The variable holding the captured output of command `name`.
pub fn stdout_var(name: &str) -> String {
    format!("command.{name}.stdout")
}

/// Renders a command resource. `create` runs in `cwd` (the worktree root).
///
/// `destroy` is rendered now when it does not depend on this command's own output, so that an
/// interrupted creation can still be undone from the ledger alone.
pub fn plan(spec: &CommandSpec, cwd: &Path, vars: &Vars) -> Result<Plan> {
    plan_inner(spec, cwd, vars).with_context(|| format!("command {:?}", spec.name))
}

fn plan_inner(spec: &CommandSpec, cwd: &Path, vars: &Vars) -> Result<Plan> {
    let own = stdout_var(&spec.name);
    if spec.create.iter().any(|arg| vars::references(arg, &own)) {
        bail!("`create` cannot reference its own output `{own}`");
    }
    let create = vars.render_all(&spec.create).context("create")?;

    let (destroy, destroy_unresolved) = match &spec.destroy {
        None => (None, None),
        Some(argv) if argv.iter().any(|arg| vars::references(arg, &own)) => {
            // Check every other reference now rather than after the resource exists.
            let mut probe = vars.clone();
            probe.set(own.as_str(), "x");
            probe.render_all(argv).context("destroy")?;
            (None, Some(argv.clone()))
        }
        Some(argv) => (Some(vars.render_all(argv).context("destroy")?), None),
    };

    Ok(Plan {
        name: spec.name.clone(),
        record: Record {
            create,
            stdout: None,
            destroy,
            destroy_unresolved,
        },
        cwd: cwd.to_path_buf(),
        vars: vars.clone(),
    })
}

/// Runs `create` and returns the final record for the ledger. A non-zero exit is an error
/// ([`Failed`]); so is output that is not UTF-8, or empty output that `destroy` depends on.
pub fn create(plan: &Plan) -> Result<Output> {
    create_inner(plan).with_context(|| format!("command {:?}: create", plan.name))
}

/// The command succeeded, so the resource may exist although creation is reported as failed.
const MAY_EXIST: &str = "the resource may have been created; `bp gc` finds it by label";

fn create_inner(plan: &Plan) -> Result<Output> {
    let run = run(&plan.record.create, &plan.cwd)?;
    let stdout = decode_stdout(run.stdout).context(MAY_EXIST)?;

    let mut record = plan.record.clone();
    if let Some(argv) = record.destroy_unresolved.take() {
        if stdout.is_empty() {
            bail!("printed nothing, but `destroy` needs its output; {MAY_EXIST}");
        }
        let mut vars = plan.vars.clone();
        vars.set(stdout_var(&plan.name), stdout.as_str());
        record.destroy = Some(vars.render_all(&argv).context("destroy")?);
    }
    record.stdout = Some(stdout);
    Ok(Output {
        record,
        stderr: run.stderr,
    })
}

/// Runs the recorded `destroy` in `cwd`: the worktree root while it exists, otherwise any
/// directory of `bp`'s own. A resource without a `destroy` command has nothing to undo.
///
/// Fails when `destroy` depends on output that was never captured: the resource may exist and
/// is left for `bp gc`.
pub fn destroy(record: &Record, cwd: &Path) -> Result<Output> {
    let Some(argv) = &record.destroy else {
        if let Some(argv) = &record.destroy_unresolved {
            bail!(
                "cannot run destroy {argv:?}: the output it needs was never captured; \
                 the resource may still exist, `bp gc` finds it by label"
            );
        }
        return Ok(Output {
            record: record.clone(),
            stderr: String::new(),
        });
    };
    let run = run(argv, cwd).context("destroy")?;
    Ok(Output {
        record: record.clone(),
        stderr: run.stderr,
    })
}

struct Run {
    stdout: Vec<u8>,
    stderr: String,
}

/// Runs `argv` without a shell, stdin closed, stdout and stderr captured.
fn run(argv: &[String], cwd: &Path) -> Result<Run> {
    let (program, args) = argv.split_first().context("empty argument array")?;
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| spawn_error(program, cwd, e))?;
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

fn spawn_error(program: &str, cwd: &Path, e: io::Error) -> anyhow::Error {
    if e.kind() == io::ErrorKind::NotFound {
        if !cwd.is_dir() {
            return anyhow::anyhow!(
                "cannot run {program}: directory {} does not exist",
                cwd.display()
            );
        }
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

/// UTF-8, with a leading byte order mark dropped (Windows PowerShell writes one) and surrounding
/// whitespace, including the trailing newline, trimmed.
fn decode_stdout(bytes: Vec<u8>) -> Result<String> {
    let text = String::from_utf8(bytes).map_err(|e| {
        anyhow::anyhow!(
            "output is not valid UTF-8 (invalid byte at offset {})",
            e.utf8_error().valid_up_to()
        )
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    Ok(text.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Prints the contents of `file` byte for byte, using only what every CI runner has.
    fn cat(file: &Path) -> Vec<String> {
        let file = file.to_string_lossy().into_owned();
        if cfg!(windows) {
            vec!["cmd".into(), "/d".into(), "/c".into(), "type".into(), file]
        } else {
            vec!["cat".into(), file]
        }
    }

    /// Writes `message` to stderr and exits with `code`.
    fn fail(code: i32, message: &str) -> Vec<String> {
        if cfg!(windows) {
            let script = format!("echo {message} 1>&2 & exit {code}");
            vec!["cmd".into(), "/d".into(), "/c".into(), script]
        } else {
            let script = format!("echo {message} >&2; exit {code}");
            vec!["sh".into(), "-c".into(), script]
        }
    }

    fn spec(name: &str, create: Vec<String>, destroy: Option<Vec<String>>) -> CommandSpec {
        CommandSpec {
            name: name.into(),
            create,
            destroy,
            gc: None,
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
        fs::write(&out, "\u{feff}  abc123\r\nsecond line \r\n\r\n").unwrap();

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
        assert_eq!(record.stdout.as_deref(), Some("abc123\r\nsecond line"));
        assert_eq!(
            record.destroy.unwrap(),
            strings(&[
                "docker",
                "rm",
                "-f",
                "abc123\r\nsecond line",
                "feature-x-1a2b3c4d"
            ])
        );
        assert_eq!(record.destroy_unresolved, None);
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
            destroy: Some(strings(&["docker", "rm", "-f", "-v", "abc"])),
            destroy_unresolved: None,
        };
        let json = serde_json::to_value(&record).unwrap();
        assert!(json.get("destroy_unresolved").is_none(), "{json}");
        assert_eq!(serde_json::from_value::<Record>(json).unwrap(), record);
    }
}
