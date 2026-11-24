//! `bp logs` as a user runs it: what it prints, its exit codes, and that it leaves the service alone.

use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

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
        let pod = Pod::new();
        let git = Command::new("git")
            .arg("-C")
            .arg(&pod.worktree)
            .args(["init", "-q"])
            .status();
        assert!(git.unwrap().success());
        let run: Vec<String> = run.iter().map(|arg| json!(arg).to_string()).collect();
        let config = format!(
            "version = 1\n[[service]]\nname = \"web\"\nrun = [{}]\n",
            run.join(", ")
        );
        fs::write(pod.worktree.join("branchpod.toml"), config).unwrap();
        let out = pod.bp(&["up".as_ref(), pod.worktree.as_os_str()]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        pod
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

/// Whether any pod is left.
fn is_there(pods: &Path) -> bool {
    fs::read_dir(pods).is_ok_and(|mut entries| entries.next().is_some())
}
