//! `bp status` as a user runs it: what it says about services and a supervisor that are not as they
//! should be, as text and as JSON, and its exit code.

use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const WORKTREE_ENV: &str = "BP_TEST_STATUS_WORKTREE";

const HEALTHY: i32 = 0;
const DEGRADED: i32 = 4;
const BUSY: i32 = 5;

/// Not a test by itself. On Windows `bp up` refuses to run in the job of `cargo test`, which would
/// kill the supervisor; here, where everything ends with the test run anyway, it runs in a job of
/// our own that is only warned about.
#[test]
#[ignore = "a process of the tests here, which start it themselves"]
fn up_process() {
    let Some(worktree) = std::env::var_os(WORKTREE_ENV) else {
        return;
    };
    #[cfg(windows)]
    // SAFETY: plain calls; the handle is left to the system, which closes it when this process ends.
    unsafe {
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        assert!(!job.is_null());
        assert!(AssignProcessToJobObject(job, GetCurrentProcess()) != 0);
    }
    let status = Command::new(env!("CARGO_BIN_EXE_branchpod"))
        .arg("up")
        .arg(worktree)
        .status()
        .unwrap();
    std::process::exit(status.code().unwrap());
}

struct Pod {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    worktree: PathBuf,
    /// Killed on drop: one that its supervisor no longer knows outlives it on Unix.
    orphan: Option<u64>,
}

/// Tears the pod down even if the test fails, so that no service stays behind.
impl Drop for Pod {
    fn drop(&mut self) {
        let _ = self.bp(&["down".as_ref(), self.worktree.as_os_str()]);
        if let Some(pid) = self.orphan {
            kill(pid);
        }
    }
}

impl Pod {
    fn new() -> Pod {
        let tmp = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(tmp.path()).unwrap();
        let worktree = base.join("wt-\u{e9}");
        fs::create_dir(&worktree).unwrap();
        let git = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["init", "-q"])
            .status();
        assert!(git.unwrap().success());
        Pod {
            home: base.join("home"),
            worktree,
            orphan: None,
            _tmp: tmp,
        }
    }

    /// A pod brought up for real, with the service `web`.
    fn up(run: &[&str]) -> Pod {
        let pod = Pod::new();
        let config = format!(
            "version = 1\n[[service]]\nname = \"web\"\nrun = {}\n",
            json!(run)
        );
        fs::write(pod.worktree.join("branchpod.toml"), config).unwrap();
        // To a file: should the supervisor hold a pipe, its end would come only with it.
        let path = pod.home.with_file_name("up.log");
        let log = fs::File::create(&path).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "up_process", "--ignored", "--nocapture"])
            .arg("--test-threads=1")
            .env(WORKTREE_ENV, &pod.worktree)
            .env("BRANCHPOD_HOME", &pod.home)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .status()
            .unwrap();
        assert!(status.success(), "{}", text(&fs::read(&path).unwrap()));
        pod
    }

    fn bp(&self, args: &[&OsStr]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_branchpod"))
            .args(args)
            .env("BRANCHPOD_HOME", &self.home)
            .output()
            .unwrap()
    }

    /// Exit code and text.
    fn text(&self) -> (i32, String) {
        let out = self.bp(&["status".as_ref(), self.worktree.as_os_str()]);
        assert_eq!(text(&out.stderr), "");
        (out.status.code().unwrap(), text(&out.stdout))
    }

    /// Exit code and JSON.
    fn json(&self) -> (i32, Value) {
        let args = [
            "status".as_ref(),
            "--json".as_ref(),
            self.worktree.as_os_str(),
        ];
        let out = self.bp(&args);
        assert_eq!(text(&out.stderr), "");
        (
            out.status.code().unwrap(),
            serde_json::from_slice(&out.stdout).unwrap(),
        )
    }

    /// Waits for the only service to be in `state`.
    fn wait_for(&self, state: &str) -> (i32, Value) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (code, status) = self.json();
            if resource(&status, "service")["runtime"]["state"] == state {
                return (code, status);
            }
            assert!(Instant::now() < deadline, "never {state}: {status:#}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits for the log of the service to contain `text`.
    fn wait_for_log(&self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let args = ["logs".as_ref(), "web".as_ref(), self.worktree.as_os_str()];
            let out = text(&self.bp(&args).stdout);
            if out.contains(expected) {
                return;
            }
            assert!(Instant::now() < deadline, "no {expected:?} in {out:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The only resource of a kind.
fn resource<'a>(status: &'a Value, kind: &str) -> &'a Value {
    let mut found = status["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["kind"] == kind);
    let resource = found.next().unwrap();
    assert!(found.next().is_none());
    resource
}

fn pid(resource: &Value) -> u64 {
    resource["data"]["process"]["pid"].as_u64().unwrap()
}

fn kill(pid: u64) {
    let pid = pid.to_string();
    let mut cmd = if cfg!(windows) {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/F", "/PID", &pid]);
        cmd
    } else {
        let mut cmd = Command::new("kill");
        cmd.args(["-KILL", &pid]);
        cmd
    };
    let _ = cmd.output().unwrap();
}

/// Prints a line and writes nothing more.
fn quiet() -> Vec<&'static str> {
    if cfg!(windows) {
        let script = "echo started& ping -n 300 127.0.0.1 > nul";
        vec!["cmd", "/d", "/c", script]
    } else {
        vec!["sh", "-c", "echo started; exec sleep 300"]
    }
}

#[test]
fn a_pod_whose_services_run_is_healthy() {
    let pod = Pod::up(&quiet());
    let (code, status) = pod.json();
    assert_eq!(code, HEALTHY, "{status:#}");
    assert_eq!(status["schema"], 1);
    assert_eq!(
        status["health"],
        json!({ "state": "healthy", "problems": [] })
    );
    // The record and what is added to it.
    let service = resource(&status, "service");
    assert_eq!(service["status"], "live");
    assert_eq!(service["data"]["run"], json!(quiet()));
    assert_eq!(service["runtime"], json!({ "state": "running" }));

    let (code, out) = pod.text();
    assert_eq!(code, HEALTHY);
    let lines: Vec<Vec<&str>> = out
        .lines()
        .map(|line| line.split_whitespace().collect())
        .collect();
    assert_eq!(lines[1], ["supervisor", "supervisor", "running"], "{out}");
    assert_eq!(lines[2], ["service", "web", "running"], "{out}");
    assert_eq!(lines[3], ["healthy"], "{out}");
    assert_eq!(lines.len(), 4, "{out}");
}

#[test]
fn a_service_that_exited_degrades_the_pod() {
    let pod = Pod::up(&if cfg!(windows) {
        let script = "ping -n 2 127.0.0.1 > nul& echo last words& exit 3";
        vec!["cmd", "/d", "/c", script]
    } else {
        vec!["sh", "-c", "sleep 1; echo last words; exit 3"]
    });
    pod.wait_for_log("last words");
    let (code, status) = pod.wait_for("exited");
    assert_eq!(code, DEGRADED);
    let runtime = &resource(&status, "service")["runtime"];
    assert_eq!(runtime["exit"], json!({ "code": 3 }));
    assert!(runtime["exited_at_ms"].as_u64().unwrap() > 1_700_000_000_000);
    assert_eq!(runtime["log"]["tail"], json!(["last words"]));
    assert_eq!(runtime["log"]["truncated"], false);
    assert!(PathBuf::from(runtime["log"]["path"].as_str().unwrap()).is_file());
    let health = &status["health"];
    assert_eq!(health["state"], "degraded");
    assert_eq!(
        health["problems"],
        json!([{ "kind": "service", "name": "web", "state": "exited" }])
    );
    assert!(health["hint"].as_str().unwrap().contains("`bp down`"));

    let (code, out) = pod.text();
    assert_eq!(code, DEGRADED);
    assert!(out.contains("exited (code 3) "), "{out}");
    assert!(out.contains("\ndegraded: run `bp down`"), "{out}");
    assert!(out.ends_with("\n  | last words\n"), "{out}");
}

#[test]
fn a_killed_supervisor_degrades_the_pod() {
    let mut pod = Pod::up(&quiet());
    pod.wait_for_log("started");
    let (_, status) = pod.json();
    let service = pid(resource(&status, "service"));
    pod.orphan = Some(service);
    kill(pid(resource(&status, "supervisor")));

    if cfg!(unix) {
        // It writes nothing, so it outlives its supervisor.
        let (code, status) = pod.wait_for("orphaned");
        assert_eq!(code, DEGRADED);
        assert_eq!(resource(&status, "supervisor")["runtime"]["state"], "gone");
        assert!(pod.text().1.contains("orphaned"));
        kill(service);
    }
    let (code, status) = pod.wait_for("gone");
    assert_eq!(code, DEGRADED);
    let supervisor = resource(&status, "supervisor");
    assert_eq!(supervisor["runtime"], json!({ "state": "gone" }));
    assert_eq!(
        resource(&status, "service")["runtime"]["log"]["tail"],
        json!(["started"])
    );
    assert_eq!(status["health"]["hint"], "run `bp down`, then `bp up`");
    let (code, out) = pod.text();
    assert_eq!(code, DEGRADED);
    assert!(out.contains("\n  | started\n"), "{out}");

    // Nothing was changed by looking.
    assert_eq!(resource(&pod.json().1, "service")["status"], "live");
    // And the hint is right.
    let down = pod.bp(&["down".as_ref(), pod.worktree.as_os_str()]);
    assert!(down.status.success(), "{}", text(&down.stderr));
}

#[test]
fn a_pod_that_is_worked_on_is_busy() {
    let pod = Pod::up(&quiet());
    let locks = pod.home.join("locks");
    let id = pod.json().1["pod"]["id"].as_str().unwrap().to_owned();
    let lock = fs::OpenOptions::new()
        .write(true)
        .open(locks.join(format!("{id}.lock")))
        .unwrap();
    lock.lock().unwrap();
    let holder = locks.join(format!("{id}.holder"));
    let record = json!({ "v": 1, "pid": 5, "command": "down", "since": 1 });
    fs::write(&holder, record.to_string()).unwrap();

    let (code, status) = pod.json();
    assert_eq!(code, BUSY);
    assert_eq!(status["health"]["state"], "busy");
    let hint = status["health"]["hint"].as_str().unwrap();
    assert!(hint.contains("bp down (pid 5"), "{hint}");
    let (code, out) = pod.text();
    assert_eq!(code, BUSY);
    assert!(out.contains("\nbusy: bp down (pid 5"), "{out}");

    // The record of one that crashed does not make the pod busy.
    drop(lock);
    assert_eq!(pod.json().0, HEALTHY);
    fs::remove_file(holder).unwrap();
}

#[test]
fn no_pod_is_not_a_failure() {
    let pod = Pod::new();
    let (code, status) = pod.json();
    assert_eq!(code, 0);
    assert_eq!(
        status,
        json!({ "schema": 1, "pod": null, "resources": [], "health": null })
    );
    let (code, out) = pod.text();
    assert_eq!(code, 0);
    assert!(out.starts_with("no pod for "), "{out}");
}
