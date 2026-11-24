//! Helpers shared by the tests of several modules.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::pod::Store;
use crate::proc::{self, ProcId};
use crate::supervisor::{self, Launcher};

/// Runs `git` in `dir`; it must succeed.
pub(crate) fn git_ok(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// Writes `message` to stderr and exits with `code`.
pub(crate) fn fail(code: i32, message: &str) -> Vec<String> {
    if cfg!(windows) {
        let script = format!("echo {message} 1>&2 & exit {code}");
        vec!["cmd".into(), "/d".into(), "/c".into(), script]
    } else {
        let script = format!("echo {message} >&2; exit {code}");
        vec!["sh".into(), "-c".into(), script]
    }
}

/// Name of the env var that makes [`supervisor_process`] run.
const ENDPOINT_ENV: &str = "BP_TEST_SUPERVISOR_ENDPOINT";

/// Starts supervisors as this test binary re-running [`supervisor_process`].
pub(crate) fn launcher() -> Launcher {
    let exe = std::env::current_exe().unwrap();
    Launcher::new(move |endpoint| {
        let mut cmd = Command::new(&exe);
        cmd.args([
            "--exact",
            "test_support::supervisor_process",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ENDPOINT_ENV, endpoint);
        cmd
    })
}

/// Kills recorded processes (and their trees) on drop, so that a failing test leaves no
/// supervisor or service running (on Windows one would also keep the test binary in use).
#[derive(Default)]
pub(crate) struct Reaper(pub(crate) Vec<ProcId>);

impl Drop for Reaper {
    fn drop(&mut self) {
        // Services were recorded after their supervisor: stop them first.
        for process in self.0.iter().rev() {
            let _ = proc::stop_recorded_tree(*process, Duration::from_millis(500));
            let _ = proc::kill(*process);
        }
    }
}

/// A [`Reaper`] for every process recorded in pod `id`'s ledger.
pub(crate) fn reap_pod(store: &Store, id: &str) -> Reaper {
    let processes = store
        .resources(id)
        .unwrap_or_default()
        .iter()
        .filter_map(|r| serde_json::from_value(r.data.get("process")?.clone()).ok())
        .collect();
    Reaper(processes)
}

/// Not a test by itself: the supervisor process for tests.
#[test]
#[ignore = "the supervisor process of other tests, which start it themselves"]
fn supervisor_process() {
    if let Some(endpoint) = std::env::var_os(ENDPOINT_ENV) {
        supervisor::serve(endpoint.to_str().unwrap()).unwrap();
    }
}

/// Prints a line, then sleeps for a long time, using what every CI runner has.
pub(crate) fn long_running(marker: &str) -> Vec<String> {
    if cfg!(windows) {
        // `ping` waits about one second between echoes.
        vec![
            "cmd".into(),
            "/d".into(),
            "/c".into(),
            format!("echo {marker}& ping -n 300 127.0.0.1 > nul"),
        ]
    } else {
        vec![
            "sh".into(),
            "-c".into(),
            format!("echo {marker}; exec sleep 300"),
        ]
    }
}
