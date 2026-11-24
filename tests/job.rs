//! `bp up` in Job Objects that limit or kill the supervisor (Windows).
//!
//! The jobs are the tests' own: the test runner may have one too (`cargo test` does), so nothing
//! is concluded from that one.
#![cfg(windows)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Limits of the jobs [`wrapped_bp`] enters, outermost first, separated by commas.
const JOBS_ENV: &str = "BP_TEST_JOBS";
const WORKTREE_ENV: &str = "BP_TEST_WORKTREE";

/// Not a test by itself: enters the jobs and runs `bp up` in them.
#[test]
#[ignore = "a process of the tests here, which start it themselves"]
fn wrapped_bp() {
    let Ok(jobs) = std::env::var(JOBS_ENV) else {
        return;
    };
    for limits in jobs.split(',') {
        // SAFETY: plain data for which zeroes are valid, of the size passed. The handle is left
        // to the system, which closes it when this process ends.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            assert!(!job.is_null());
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = limits.parse().unwrap();
            let set = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                std::mem::size_of_val(&info) as u32,
            );
            assert!(set != 0);
            assert!(AssignProcessToJobObject(job, GetCurrentProcess()) != 0);
        }
    }
    let status = Command::new(env!("CARGO_BIN_EXE_branchpod"))
        .arg("up")
        .arg(std::env::var_os(WORKTREE_ENV).unwrap())
        .status()
        .unwrap();
    std::process::exit(status.code().unwrap());
}

struct Pod {
    _tmp: tempfile::TempDir,
    worktree: PathBuf,
    home: PathBuf,
}

/// Whatever fails, no supervisor is left running.
impl Drop for Pod {
    fn drop(&mut self) {
        self.down();
    }
}

impl Pod {
    fn new(config: &str) -> Pod {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("wt-caf\u{e9}");
        let init = Command::new("git")
            .args(["init", "-q"])
            .arg(&worktree)
            .status();
        assert!(init.unwrap().success());
        fs::write(worktree.join("branchpod.toml"), config).unwrap();
        Pod {
            home: tmp.path().join("home"),
            worktree,
            _tmp: tmp,
        }
    }

    /// `bp up` in nested jobs with these limits, outermost first. Its output goes to a file, not
    /// to a pipe: the wrapper hands `bp` more than one copy of its standard handles, and the
    /// supervisor may keep one, so that the end of a pipe would not come before its own end.
    fn up_in(&self, jobs: &[u32]) -> (i32, String) {
        let jobs: Vec<String> = jobs.iter().map(u32::to_string).collect();
        let path = self.home.with_file_name("up.log");
        let log = fs::File::create(&path).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "wrapped_bp",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(JOBS_ENV, jobs.join(","))
            .env(WORKTREE_ENV, &self.worktree)
            .env("BRANCHPOD_HOME", &self.home)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .status()
            .unwrap();
        let out = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
        (status.code().unwrap(), out)
    }

    fn down(&self) -> (i32, String) {
        let args: [&OsStr; 2] = ["down".as_ref(), self.worktree.as_os_str()];
        text(
            Command::new(env!("CARGO_BIN_EXE_branchpod"))
                .args(args)
                .env("BRANCHPOD_HOME", &self.home)
                .output()
                .unwrap(),
        )
    }

    fn marker(&self) -> PathBuf {
        self.worktree.join("marker")
    }
}

fn text(out: Output) -> (i32, String) {
    let text = [out.stdout, out.stderr].concat();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&text).into_owned(),
    )
}

const PLAIN: &str = "version = 1\n\
     [port.web]\nrange = [42000, 42999]\n\
     [[command]]\nname = 'mark'\ncreate = ['cmd', '/d', '/c', 'echo x> marker']\n";

fn with_service() -> String {
    format!("{PLAIN}[[service]]\nname = 'web'\nrun = ['ping', '-n', '300', '127.0.0.1']\n")
}

fn has_pods(home: &Path) -> bool {
    home.join("pods").exists()
}

#[test]
fn a_job_that_kills_and_allows_no_breakaway_is_refused_before_anything_is_created() {
    let pod = Pod::new(&with_service());
    let (code, out) = pod.up_in(&[JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("cannot run services"), "{out}");
    assert!(
        out.contains("KILL_ON_JOB_CLOSE") && out.contains("outside of that job"),
        "{out}"
    );
    assert!(!pod.marker().exists(), "a command ran");
    assert!(!has_pods(&pod.home), "a pod was recorded");
}

#[test]
fn a_pod_without_services_does_not_care() {
    let pod = Pod::new(PLAIN);
    let (code, out) = pod.up_in(&[JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE]);
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("warning"), "{out}");
    assert!(pod.marker().exists());
}

/// `bp` sees that it may leave its job; only the supervisor sees where it ended up.
#[test]
fn an_outer_job_that_kills_is_refused_by_the_supervisor() {
    let pod = Pod::new(&with_service());
    let (code, out) = pod.up_in(&[
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    ]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("KILL_ON_JOB_CLOSE"), "{out}");
    assert!(
        pod.marker().exists(),
        "refused by the supervisor, not by `bp up`"
    );
    assert!(out.contains("bp down"), "{out}");
}

#[test]
fn a_job_that_neither_kills_nor_allows_breakaway_is_warned_about() {
    let pod = Pod::new(&with_service());
    let (code, out) = pod.up_in(&[0]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("bp: warning: the supervisor runs in a Job Object it cannot leave"),
        "{out}"
    );
    let (code, out) = pod.down();
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("clean") && out.contains("web"), "{out}");
}
