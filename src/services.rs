//! Service resources: long-running processes under the pod's supervisor.
//!
//! [`plan`] renders a service (`run` and `env` take `{{var}}`; `cwd` is a plain path relative to
//! the worktree root) into the request the supervisor gets and the ledger data. The environment is
//! not recorded: it may hold secrets, and teardown does not need it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::ServiceSpec;
use crate::proc::ProcId;
use crate::supervisor::Start;
use crate::vars::Vars;

/// Ledger data of a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// The command as it was started.
    pub(crate) run: Vec<String>,
    pub(crate) cwd: PathBuf,
    /// Its own process, set once started. On Unix also its process group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) process: Option<ProcId>,
}

/// A rendered service: the supervisor's request and the intent's ledger data.
#[derive(Debug)]
pub(crate) struct Plan {
    pub(crate) start: Start,
    pub(crate) record: Record,
}

/// Where the output of service `name` goes, in the pod directory `pod_dir`.
pub(crate) fn log_path(pod_dir: &Path, name: &str) -> PathBuf {
    pod_dir.join("logs").join(format!("{name}.log"))
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
    Ok(Plan {
        start: Start {
            name: spec.name.clone(),
            run: run.clone(),
            cwd: cwd.clone(),
            env,
            log: log_path(pod_dir, &spec.name),
        },
        record: Record {
            run,
            cwd,
            process: None,
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
            Path::new("/home/pods/p").join("logs").join("vite.log")
        );
        let json = serde_json::to_string(&plan.record).unwrap();
        assert!(!json.contains("s-8003"), "{json}");
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
