//! Service resources: long-running processes under the pod's supervisor.
//!
//! [`plan`] renders a service (`run` and `env` take `{{var}}`; `cwd` is a plain path relative to
//! the worktree root) into the request the supervisor gets and the ledger data. The environment is
//! not recorded: it may hold secrets, and teardown does not need it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::ServiceSpec;
use crate::proc::{self, ProcId};
use crate::supervisor::Start;
use crate::vars::Vars;

/// Ledger data of a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// The command as it was started.
    pub(crate) run: Vec<String>,
    pub(crate) cwd: PathBuf,
    /// Where its output goes. Not recorded by older versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) log: Option<PathBuf>,
    /// Its own process, set once started. On Unix also its process group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) process: Option<ProcId>,
    /// The boot of the system in which it was started ([`proc::boot_id`]). Not recorded by older
    /// versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) boot: Option<String>,
}

impl Record {
    /// The record of a planned service that was started as `process`.
    pub(crate) fn started(&self, process: ProcId) -> Record {
        Record {
            process: Some(process),
            boot: proc::boot_id(),
            ..self.clone()
        }
    }

    /// Whether the system was rebooted since: then nothing of it runs any more, and whatever has its
    /// pid or process group is somebody else's.
    pub(crate) fn is_of_another_boot(&self) -> bool {
        self.boot.is_some() && proc::boot_id().is_some_and(|now| self.boot != Some(now))
    }
}

/// A rendered service: the supervisor's request and the intent's ledger data.
#[derive(Debug)]
pub(crate) struct Plan {
    pub(crate) start: Start,
    pub(crate) record: Record,
}

/// Where the output of service `name` goes, in the pod directory `pod_dir`. The prefix keeps it
/// apart from the supervisor's own log and from Windows device names (`con`, `nul`).
pub(crate) fn log_path(pod_dir: &Path, name: &str) -> PathBuf {
    pod_dir.join("logs").join(format!("service-{name}.log"))
}

/// Where the output of a recorded service is: as recorded, or where versions that did not record
/// it wrote it.
pub(crate) fn recorded_log(record: Option<&Record>, pod_dir: &Path, name: &str) -> PathBuf {
    record
        .and_then(|r| r.log.clone())
        .unwrap_or_else(|| pod_dir.join("logs").join(format!("{name}.log")))
}

pub(crate) fn plan(spec: &ServiceSpec, root: &Path, vars: &Vars, pod_dir: &Path) -> Result<Plan> {
    plan_inner(spec, root, vars, pod_dir).with_context(|| format!("service {:?}", spec.name))
}

fn plan_inner(spec: &ServiceSpec, root: &Path, vars: &Vars, pod_dir: &Path) -> Result<Plan> {
    let run = vars.render_all(&spec.run).context("run")?;
    let env = spec
        .env
        .iter()
        .map(|(key, value)| {
            let value = vars.render(value).with_context(|| format!("env.{key}"))?;
            Ok((key.clone(), value))
        })
        .collect::<Result<_>>()?;
    let cwd = match &spec.cwd {
        Some(dir) => root.join(dir),
        None => root.to_path_buf(),
    };
    let log = log_path(pod_dir, &spec.name);
    Ok(Plan {
        start: Start {
            name: spec.name.clone(),
            run: run.clone(),
            cwd: cwd.clone(),
            env,
            log: log.clone(),
        },
        record: Record {
            run,
            cwd,
            log: Some(log),
            process: None,
            boot: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn renders_run_and_env_but_records_no_env() {
        let mut vars = Vars::new();
        vars.set("port.web", "8003");
        let spec = ServiceSpec {
            name: "vite".into(),
            run: vec![
                "npx".into(),
                "vite".into(),
                "--port".into(),
                "{{port.web}}".into(),
            ],
            cwd: Some("frontend".into()),
            env: BTreeMap::from([("SECRET".into(), "s-{{port.web}}".into())]),
        };
        let root = Path::new("/wt");
        let plan = plan(&spec, root, &vars, Path::new("/home/pods/p")).unwrap();
        assert_eq!(plan.start.run[3], "8003");
        assert_eq!(plan.start.env["SECRET"], "s-8003");
        assert_eq!(plan.start.cwd, root.join("frontend"));
        assert_eq!(
            plan.start.log,
            Path::new("/home/pods/p")
                .join("logs")
                .join("service-vite.log")
        );
        assert_eq!(plan.record.log.as_ref(), Some(&plan.start.log));
        let json = serde_json::to_string(&plan.record).unwrap();
        assert!(!json.contains("s-8003"), "{json}");
    }

    #[test]
    fn a_record_without_a_log_means_the_old_place() {
        let dir = Path::new("/home/pods/p");
        let old: Record = serde_json::from_str(r#"{ "run": ["x"], "cwd": "/wt" }"#).unwrap();
        let old_place = dir.join("logs").join("web.log");
        assert_eq!(recorded_log(Some(&old), dir, "web"), old_place);
        assert_eq!(recorded_log(None, dir, "web"), old_place);
        let new = Record {
            log: Some(log_path(dir, "web")),
            ..old
        };
        assert_eq!(recorded_log(Some(&new), dir, "web"), log_path(dir, "web"));
    }

    #[test]
    fn unknown_variables_name_the_service() {
        let spec = ServiceSpec {
            name: "vite".into(),
            run: vec!["x".into(), "{{port.nope}}".into()],
            cwd: None,
            env: BTreeMap::new(),
        };
        let err = format!(
            "{:#}",
            plan(&spec, Path::new("/wt"), &Vars::new(), Path::new("/p")).unwrap_err()
        );
        assert!(
            err.contains("service \"vite\"") && err.contains("port.nope"),
            "{err}"
        );
    }
}
