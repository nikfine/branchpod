//! Whoever waits for the end of `bp up`'s output gets it when `bp up` exits, not when its
//! supervisor does (Windows).
//!
//! Each caller is started by a process of the tests ([`caller`]) that is in a job without limits,
//! so that the supervisor is not refused for the sake of the test runner's job.
#![cfg(windows)]

use std::fs;
use std::io::{PipeReader, Read, Write};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

/// The program for [`caller`] to run, and its arguments exactly as they go into the command line.
const PROGRAM_ENV: &str = "BP_TEST_PROGRAM";
const ARGS_ENV: &str = "BP_TEST_ARGS";
/// Instead of a program: run `bp up` with an extra inheritable pipe end.
const STRAY: &str = "stray pipe end";

/// For the callers' commands, so that no path has to be quoted for them.
const BP_ENV: &str = "BP_TEST_BP";
const WORKTREE_ENV: &str = "BP_TEST_WORKTREE";

/// Only for a failure: the end of output comes at once or not before the supervisor's.
const NEVER: Duration = Duration::from_secs(30);

/// Not a test by itself: runs the caller of a test in a job.
#[test]
#[ignore = "a process of the tests here, which start it themselves"]
fn caller() {
    let Ok(program) = std::env::var(PROGRAM_ENV) else {
        return;
    };
    // SAFETY: plain data for which zeroes are valid, of the size passed. The job's handle is left
    // to the system. What we pass on as standard handles is std's copies, as with any caller; our
    // own are not to be inherited besides.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        assert!(!job.is_null());
        let info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            std::mem::size_of_val(&info) as u32,
        );
        assert!(set != 0);
        assert!(AssignProcessToJobObject(job, GetCurrentProcess()) != 0);
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            assert!(SetHandleInformation(GetStdHandle(which), HANDLE_FLAG_INHERIT, 0) != 0);
        }
    }
    if program == STRAY {
        return with_a_stray_pipe_end();
    }
    Command::new(program)
        .raw_arg(std::env::var(ARGS_ENV).unwrap())
        .status()
        .unwrap();
}

/// Runs `bp up` with nothing on its standard handles and, besides, an inheritable pipe end.
fn with_a_stray_pipe_end() {
    let (reader, writer) = std::io::pipe().unwrap();
    let writer = OwnedHandle::from(writer);
    // SAFETY: a valid handle.
    unsafe {
        let set = SetHandleInformation(writer.as_raw_handle(), HANDLE_FLAG_INHERIT, 1);
        assert!(set != 0);
    }
    let status = Command::new(std::env::var_os(BP_ENV).unwrap())
        .arg("up")
        .arg(std::env::var_os(WORKTREE_ENV).unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    drop(writer);
    let end = end_of(reader).recv_timeout(NEVER);
    println!(
        "the stray pipe {}",
        if end.is_ok() { "ended" } else { "goes on" }
    );
    // Not waiting for the reading thread.
    std::process::exit(0);
}

/// What `reader` reads, once it ends.
fn end_of(mut reader: PipeReader) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = Vec::new();
        reader.read_to_end(&mut text).unwrap();
        // No one to tell: the test has failed.
        let _ = sender.send(String::from_utf8_lossy(&text).into_owned());
    });
    receiver
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
    fn new() -> Pod {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("wt caf\u{e9}");
        let init = Command::new("git")
            .args(["init", "-q"])
            .arg(&worktree)
            .status();
        assert!(init.unwrap().success());
        fs::write(
            worktree.join("branchpod.toml"),
            "version = 1\n[[service]]\nname = 'web'\nrun = ['ping', '-n', '300', '127.0.0.1']\n",
        )
        .unwrap();
        Pod {
            home: tmp.path().join("home"),
            worktree,
            _tmp: tmp,
        }
    }

    fn down(&self) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_branchpod"))
            .arg("down")
            .arg(&self.worktree)
            .env("BRANCHPOD_HOME", &self.home)
            .output()
            .unwrap();
        String::from_utf8_lossy(&[out.stdout, out.stderr].concat()).into_owned()
    }

    /// Pid of the recorded supervisor.
    fn supervisor(&self) -> Option<u32> {
        let pod = fs::read_dir(self.home.join("pods")).ok()?.next()?.ok()?;
        let ledger = fs::read_to_string(pod.path().join("ledger.jsonl")).ok()?;
        ledger.lines().find_map(|line| {
            let entry: serde_json::Value = serde_json::from_str(line).ok()?;
            let data = entry.get("data")?;
            data.get("endpoint")?;
            data.get("process")?.get("pid")?.as_u64()?.try_into().ok()
        })
    }
}

fn runs(pid: u32) -> bool {
    // SAFETY: plain calls; the handle is closed.
    unsafe {
        let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if process.is_null() {
            return false;
        }
        let state = WaitForSingleObject(process, 0);
        CloseHandle(process);
        state == WAIT_TIMEOUT
    }
}

/// Runs the caller, which must see the end of its output while the supervisor runs; the output.
fn ends(program: &str, args: &str) -> String {
    let pod = Pod::new();
    fs::write(
        pod.worktree.with_file_name("hook.cmd"),
        format!("@\"%{BP_ENV}%\" up \"%{WORKTREE_ENV}%\"\r\n"),
    )
    .unwrap();
    let (reader, writer) = std::io::pipe().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "caller",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(pod.worktree.parent().unwrap())
        .env(PROGRAM_ENV, program)
        .env(ARGS_ENV, args)
        .env(BP_ENV, env!("CARGO_BIN_EXE_branchpod"))
        .env(WORKTREE_ENV, &pod.worktree)
        .env("BRANCHPOD_HOME", &pod.home)
        .stdin(Stdio::null())
        .stdout(writer.try_clone().unwrap())
        .stderr(writer)
        .status()
        .unwrap();
    let out = end_of(reader).recv_timeout(NEVER);
    let supervisor = pod.supervisor();
    let running = supervisor.is_some_and(runs);
    let down = pod.down();
    let out = out.expect("the end of output does not come while the supervisor runs");
    assert!(status.success(), "{out}");
    assert!(running, "the supervisor does not run: {out}");
    assert!(down.contains("clean") && down.contains("web"), "{down}");
    out
}

fn cmd(args: &str) {
    ends("cmd", &format!("/d /s /c \"{args}\""));
}

fn powershell(command: &str) {
    let root = std::env::var_os("SystemRoot").unwrap();
    let exe = Path::new(&root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    if !exe.is_file() {
        // Not with `eprintln!`, which a passing test keeps to itself.
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED, no Windows PowerShell at {}: {command}",
            exe.display()
        );
        return;
    }
    ends(
        exe.to_str().unwrap(),
        &format!("-NoProfile -NonInteractive -Command \"{command}\""),
    );
}

const CMD_UP: &str = "\"%BP_TEST_BP%\" up \"%BP_TEST_WORKTREE%\"";
const POWERSHELL_UP: &str = "& $env:BP_TEST_BP up $env:BP_TEST_WORKTREE";

#[test]
fn a_direct_start() {
    let bp = env!("CARGO_BIN_EXE_branchpod");
    // The one worktree there is.
    ends(bp, "up \"wt caf\u{e9}\"");
}

#[test]
fn a_stray_pipe_end() {
    let out = ends(STRAY, "");
    assert!(out.contains("the stray pipe ended"), "{out}");
}

#[test]
fn cmd_without_redirections() {
    cmd(CMD_UP);
}

#[test]
fn cmd_with_stderr_on_stdout() {
    cmd(&format!("{CMD_UP} 2>&1"));
}

#[test]
fn cmd_with_stdout_on_nul() {
    cmd(&format!("{CMD_UP} >nul"));
}

#[test]
fn cmd_with_a_pipe() {
    cmd(&format!("{CMD_UP} | findstr ."));
}

#[test]
fn cmd_calling_a_hook() {
    cmd(r"call .\hook.cmd 2>&1");
}

#[test]
fn powershell_with_stdout_as_it_is() {
    powershell(POWERSHELL_UP);
}

#[test]
fn powershell_with_everything_in_a_pipeline() {
    powershell(&format!("{POWERSHELL_UP} 2>&1 | Out-String"));
}
