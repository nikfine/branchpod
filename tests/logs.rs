//! `bp logs` as a user runs it: what it prints, its exit codes, and that it leaves the service alone.

use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const WORKTREE_ENV: &str = "BP_TEST_LOGS_WORKTREE";

/// `bp up` of the worktree in [`WORKTREE_ENV`]: this test binary re-running [`up_process`].
fn up_command() -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "up_process",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    cmd
}

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
}

/// Tears the pod down even if the test fails, so that no service stays behind.
impl Drop for Pod {
    fn drop(&mut self) {
        let _ = self.bp(&["down".as_ref(), self.worktree.as_os_str()]);
    }
}

impl Pod {
    fn new() -> Pod {
        let tmp = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(tmp.path()).unwrap();
        let worktree = base.join("wt-\u{e9}");
        fs::create_dir(&worktree).unwrap();
        Pod {
            home: base.join("home"),
            worktree,
            _tmp: tmp,
        }
    }

    /// A pod with one recorded service `web` whose process is long gone.
    fn recorded() -> Pod {
        let pod = Pod::new();
        fs::create_dir_all(pod.dir().join("logs")).unwrap();
        let meta = json!({ "v": 1, "id": "wt-00000000", "worktree": pod.worktree });
        fs::write(pod.dir().join("pod.json"), meta.to_string()).unwrap();
        // No process has this start token.
        let data = json!({
            "run": ["x"], "cwd": pod.worktree,
            "process": { "pid": std::process::id(), "start": 1 },
        });
        pod.ledger(&[("intent", data.clone()), ("created", data)]);
        pod
    }

    /// A pod brought up for real, with the service `web`.
    fn up(run: &[&str]) -> Pod {
        Pod::up_as("web", run)
    }

    fn up_as(service: &str, run: &[&str]) -> Pod {
        let pod = Pod::new();
        let git = Command::new("git")
            .arg("-C")
            .arg(&pod.worktree)
            .args(["init", "-q"])
            .status();
        assert!(git.unwrap().success());
        let run: Vec<String> = run.iter().map(|arg| json!(arg).to_string()).collect();
        let config = format!(
            "version = 1\n[[service]]\nname = \"{service}\"\nrun = [{}]\n",
            run.join(", ")
        );
        fs::write(pod.worktree.join("branchpod.toml"), config).unwrap();
        pod.run_up();
        pod
    }

    /// Output goes to a file, not to a pipe: should the supervisor hold the pipe, its end would come
    /// only with the supervisor's.
    fn run_up(&self) {
        let path = self.home.with_file_name("up.log");
        let log = fs::File::create(&path).unwrap();
        let status = up_command()
            .env(WORKTREE_ENV, &self.worktree)
            .env("BRANCHPOD_HOME", &self.home)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .status()
            .unwrap();
        assert!(status.success(), "{}", text(&fs::read(&path).unwrap()));
    }

    fn dir(&self) -> PathBuf {
        self.home.join("pods").join("wt-00000000")
    }

    fn log(&self) -> PathBuf {
        self.dir().join("logs").join("web.log")
    }

    fn ledger(&self, events: &[(&str, Value)]) {
        let lines: String = events
            .iter()
            .map(|(op, data)| {
                let entry = json!({
                    "v": 1, "ts_ms": 1, "kind": "service", "name": "web", "op": op,
                    "data": data, "error": "boom",
                });
                format!("{entry}\n")
            })
            .collect();
        // Appended in one write, as `bp` does: a follower never sees an empty ledger.
        let mut options = fs::OpenOptions::new();
        let path = self.dir().join("ledger.jsonl");
        let mut file = options.create(true).append(true).open(path).unwrap();
        file.write_all(lines.as_bytes()).unwrap();
    }

    fn command(&self, args: &[&OsStr]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_branchpod"));
        cmd.args(args).env("BRANCHPOD_HOME", &self.home);
        cmd
    }

    fn bp(&self, args: &[&OsStr]) -> Output {
        self.command(args).output().unwrap()
    }

    fn logs(&self, args: &[&str]) -> Output {
        self.logs_command(args).output().unwrap()
    }

    fn logs_command(&self, args: &[&str]) -> Command {
        let mut cmd = self.command(&["logs".as_ref()]);
        cmd.args(args).arg("web").arg(&self.worktree);
        cmd
    }

    /// Waits until the log of `service` contains `text`.
    fn wait_for(&self, service: &str, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let args = ["logs".as_ref(), service.as_ref(), self.worktree.as_os_str()];
            let out = String::from_utf8_lossy(&self.bp(&args).stdout).into_owned();
            if out.contains(text) {
                return out;
            }
            assert!(Instant::now() < deadline, "no {text:?} in {out:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The only file in the pods' log directories whose name is `name`.
    fn log_file(&self, name: &str) -> String {
        let pod = fs::read_dir(self.home.join("pods"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        text(&fs::read(pod.path().join("logs").join(name)).unwrap())
    }

    fn down(&self) -> Output {
        self.bp(&["down".as_ref(), self.worktree.as_os_str()])
    }

    fn service_pid(&self) -> u64 {
        let args = [
            "status".as_ref(),
            self.worktree.as_os_str(),
            "--json".as_ref(),
        ];
        let status: Value = serde_json::from_slice(&self.bp(&args).stdout).unwrap();
        let service = status["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["kind"] == "service")
            .unwrap()
            .clone();
        service["data"]["process"]["pid"].as_u64().unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Waits for `child` to exit; kills it and fails if it does not.
fn exit_code(child: &mut Child) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("bp logs did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Prints `tick` every second or so, for ever.
fn ticking() -> Vec<&'static str> {
    if cfg!(windows) {
        let script = "for /L %i in (1,1,300) do @(echo tick& ping -n 2 127.0.0.1 > nul)";
        vec!["cmd", "/d", "/c", script]
    } else {
        vec!["sh", "-c", "while :; do echo tick; sleep 1; done"]
    }
}

#[test]
fn prints_the_bytes_as_they_are_across_segments() {
    let pod = Pod::recorded();
    fs::write(pod.dir().join("logs/web.log.1"), b"one\ntw").unwrap();
    fs::write(pod.log(), b"o\n\xff\xfe\r\nlast").unwrap();

    let out = pod.logs(&[]);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"one\ntwo\n\xff\xfe\r\nlast");
    assert_eq!(text(&out.stderr), "");
    assert_eq!(pod.logs(&["-n", "3"]).stdout, b"two\n\xff\xfe\r\nlast");
    assert_eq!(pod.logs(&["-n", "0"]).stdout, b"");
    assert_eq!(pod.logs(&["-n", "x"]).status.code(), Some(2));
}

#[test]
fn says_so_when_only_the_last_lines_are_printed() {
    let pod = Pod::recorded();
    let lines: String = (0..300).map(|i| format!("line {i}\n")).collect();
    fs::write(pod.log(), &lines).unwrap();

    let out = pod.logs(&[]);
    assert!(text(&out.stdout).starts_with("line 100\n"));
    assert!(text(&out.stderr).contains("--lines all"));
    let out = pod.logs(&["--lines", "all"]);
    assert_eq!(text(&out.stdout), lines);
    assert_eq!(text(&out.stderr), "");
}

#[test]
fn what_cannot_be_printed_is_an_error() {
    let pod = Pod::recorded();
    let path = pod.worktree.as_os_str();

    let out = pod.bp(&["logs".as_ref(), "wbe".as_ref(), path]);
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(
        err.contains("no service \"wbe\"") && err.contains("web"),
        "{err}"
    );

    let out = pod.logs(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("has no log"));

    pod.ledger(&[("intent", json!({})), ("failed", Value::Null)]);
    let out = pod.logs(&["-f"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("boom"));

    let other = pod.home.join("other");
    let out = pod.bp(&["logs".as_ref(), "web".as_ref(), other.as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("no pod"));
}

#[test]
fn following_a_service_that_has_exited_prints_its_log_and_ends() {
    let pod = Pod::recorded();
    fs::write(pod.log(), "last words\n").unwrap();
    let out = pod.logs(&["-f"]);
    assert!(out.status.success());
    assert_eq!(text(&out.stdout), "last words\n");
    assert!(text(&out.stderr).contains("not running"));
}

#[test]
fn a_service_that_was_never_started_is_waited_for_only_while_up_runs() {
    let pod = Pod::recorded();
    let data = json!({ "run": ["x"], "cwd": pod.worktree });
    pod.ledger(&[("intent", data.clone())]);

    // Nobody holds the pod lock: `bp up` died half way.
    let out = pod.logs(&["-f"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("bp down"),
        "{}",
        text(&out.stderr)
    );

    // `bp up` is at work.
    fs::create_dir_all(pod.home.join("locks")).unwrap();
    let lock = fs::File::create(pod.home.join("locks").join("wt-00000000.lock")).unwrap();
    lock.lock().unwrap();
    let mut child = pod
        .logs_command(&["-f"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert!(child.try_wait().unwrap().is_none());
    pod.ledger(&[("intent", data), ("failed", Value::Null)]);
    drop(lock);
    assert_eq!(exit_code(&mut child), Some(1));
    let mut err = String::new();
    child.stderr.unwrap().read_to_string(&mut err).unwrap();
    assert!(err.contains("waiting") && err.contains("boom"), "{err}");
}

#[test]
fn a_service_not_recorded_yet_is_waited_for_only_while_up_runs() {
    let pod = Pod::recorded();
    fs::remove_file(pod.dir().join("ledger.jsonl")).unwrap();
    let locks = pod.home.join("locks");
    fs::create_dir_all(&locks).unwrap();
    let lock = fs::File::create(locks.join("wt-00000000.lock")).unwrap();
    lock.lock().unwrap();
    let follower = || {
        pod.logs_command(&["-f"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let stderr = |child: &mut Child| {
        let mut err = String::new();
        let mut pipe = child.stderr.take().unwrap();
        pipe.read_to_string(&mut err).unwrap();
        err
    };
    // Without `-f` there is nothing to wait for.
    assert_eq!(pod.logs(&[]).status.code(), Some(1));

    // `up` ends without having recorded the service.
    let mut never = follower();
    std::thread::sleep(Duration::from_secs(1));
    assert!(never.try_wait().unwrap().is_none());
    lock.unlock().unwrap();
    assert_eq!(exit_code(&mut never), Some(1));
    let err = stderr(&mut never);
    assert!(
        err.contains("no service web yet") && err.contains("has no services"),
        "{err}"
    );

    // The service is recorded, and then fails.
    lock.lock().unwrap();
    let mut failed = follower();
    std::thread::sleep(Duration::from_secs(1));
    assert!(failed.try_wait().unwrap().is_none());
    pod.ledger(&[("intent", json!({ "run": ["x"], "cwd": pod.worktree }))]);
    std::thread::sleep(Duration::from_secs(1));
    assert!(failed.try_wait().unwrap().is_none());
    pod.ledger(&[("failed", Value::Null)]);
    assert_eq!(exit_code(&mut failed), Some(1));
    let err = stderr(&mut failed);
    assert!(err.contains("boom"), "{err}");

    // `down` holds the lock: nothing will be recorded.
    fs::remove_file(pod.dir().join("ledger.jsonl")).unwrap();
    let holder = json!({ "v": 1, "pid": 1, "command": "down", "since": 1 });
    fs::write(locks.join("wt-00000000.holder"), holder.to_string()).unwrap();
    assert_eq!(exit_code(&mut follower()), Some(1));
}

#[test]
fn a_reader_that_has_seen_enough_is_not_an_error() {
    let pod = Pod::recorded();
    // More than a pipe holds.
    fs::write(pod.log(), "some line\n".repeat(200_000)).unwrap();
    let mut child = pod
        .logs_command(&["-n", "all"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    stdout.read_line(&mut line).unwrap();
    drop(stdout);
    assert_eq!(exit_code(&mut child), Some(0));
    let mut err = String::new();
    child.stderr.unwrap().read_to_string(&mut err).unwrap();
    assert_eq!(err, "");
}

#[test]
fn killing_a_follower_leaves_the_service_running_and_down_ends_one() {
    let pod = Pod::up(&ticking());
    let pid = pod.service_pid();
    let follower = || {
        let mut child = pod
            .logs_command(&["-f"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        (child, stdout)
    };

    let (mut killed, stdout) = follower();
    let seen: Vec<String> = stdout.lines().take(2).map(Result::unwrap).collect();
    assert!(seen.iter().all(|line| line.trim() == "tick"), "{seen:?}");
    killed.kill().unwrap();
    killed.wait().unwrap();

    // Still the same process, and it still writes.
    assert_eq!(pod.service_pid(), pid);
    let (mut child, mut stdout) = follower();
    let mut line = String::new();
    for _ in 0..4 {
        line.clear();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "tick");
    }

    let down = pod.down();
    assert!(down.status.success(), "{}", text(&down.stderr));
    // Read on, so that the follower is not kept by a full pipe.
    stdout.read_to_string(&mut line).unwrap();
    assert_eq!(exit_code(&mut child), Some(0));
    assert!(!is_there(&pod.home.join("pods")));
}

/// stdout and stderr share a pipe, so a log has them in the order they were written.
#[test]
fn stdout_and_stderr_keep_their_order() {
    let pod = Pod::up(&if cfg!(windows) {
        vec![
            "cmd",
            "/d",
            "/c",
            "echo o1& echo e2 1>&2& echo o3& echo e4 1>&2& ping -n 300 127.0.0.1 > nul",
        ]
    } else {
        vec![
            "sh",
            "-c",
            "echo o1; echo e2 >&2; echo o3; echo e4 >&2; exec sleep 300",
        ]
    });
    let out = pod.wait_for("web", "e4");
    let words: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(words, ["o1", "e2", "o3", "e4"]);
    assert!(pod.log_file("service-web.log").contains("o1"));
}

/// #73: its log is not the supervisor's own.
#[test]
fn a_service_named_supervisor_has_a_log_of_its_own() {
    let pod = Pod::up_as("supervisor", &ticking());
    pod.wait_for("supervisor", "tick");
    assert!(pod.log_file("service-supervisor.log").contains("tick"));
    assert!(!pod.log_file("supervisor.log").contains("tick"));
}

/// #72: MSYS programs cannot write to an append-only handle. Skipped without an MSYS `bash`.
#[cfg(windows)]
#[test]
fn msys_programs_have_a_log() {
    let exec_path = Command::new("git").arg("--exec-path").output().unwrap();
    let exec_path = PathBuf::from(text(&exec_path.stdout).trim());
    let Some(bash) = exec_path
        .ancestors()
        .map(|dir| dir.join("usr").join("bin").join("bash.exe"))
        .find(|bash| bash.is_file())
    else {
        eprintln!("skipped: no bash next to git");
        return;
    };
    let script = "echo from-bash; echo err-bash >&2; sleep 300";
    let pod = Pod::up(&[bash.to_str().unwrap(), "-c", script]);
    let out = pod.wait_for("web", "err-bash");
    assert!(out.contains("from-bash"), "{out}");
}

/// Whether any pod is left.
fn is_there(pods: &Path) -> bool {
    fs::read_dir(pods).is_ok_and(|mut entries| entries.next().is_some())
}
