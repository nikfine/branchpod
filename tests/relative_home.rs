//! A relative `BRANCHPOD_HOME`: it is resolved against the directory `bp` is called from, also for
//! the supervisor and the services, which run elsewhere.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Non-ASCII and with a space on purpose.
const HOME: &str = "bp home-\u{fc}";

/// On Windows `bp up` refuses to start services in the job of `cargo test`, which kills what is in
/// it. Only the innermost job counts, so this process enters a harmless one and `bp` inherits it.
#[cfg(windows)]
fn enter_harmless_job() {
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    static ENTERED: std::sync::Once = std::sync::Once::new();
    ENTERED.call_once(|| {
        // SAFETY: plain calls. The handle is left to the system, which closes it when this process
        // ends; a job without limits does nothing then.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            assert!(!job.is_null());
            assert!(AssignProcessToJobObject(job, GetCurrentProcess()) != 0);
        }
    });
}

#[cfg(not(windows))]
fn enter_harmless_job() {}

struct Setup {
    _tmp: tempfile::TempDir,
    /// Where `bp` is called from.
    cwd: PathBuf,
    worktree: PathBuf,
}

/// Whatever fails, no supervisor is left running. No panic here: while unwinding it would abort the
/// process, and the other tests could not clean up.
impl Drop for Setup {
    fn drop(&mut self) {
        let _ = self.try_bp(HOME.as_ref(), &["down"]);
    }
}

impl Setup {
    /// A worktree with a service `web` and an empty directory to call `bp` from.
    fn new() -> Setup {
        enter_harmless_job();
        let tmp = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(tmp.path()).unwrap();
        let (cwd, worktree) = (base.join("cwd"), base.join("wt"));
        fs::create_dir(&cwd).unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .arg(&worktree)
            .status();
        assert!(init.unwrap().success());
        let run = if cfg!(windows) {
            "['ping', '-n', '300', '127.0.0.1']"
        } else {
            "['sleep', '300']"
        };
        let config = format!("version = 1\n[[service]]\nname = 'web'\nrun = {run}\n");
        fs::write(worktree.join("branchpod.toml"), config).unwrap();
        Setup {
            _tmp: tmp,
            cwd,
            worktree,
        }
    }

    /// Runs `bp` in `cwd`: exit code and all it printed. The output goes to a file, not to a pipe:
    /// the supervisor may inherit the pipe (#76), whose end would come only with the supervisor's.
    fn try_bp(&self, home: &OsStr, args: &[&str]) -> io::Result<(Option<i32>, String)> {
        let path = self.worktree.with_file_name("bp.log");
        let log = fs::File::create(&path)?;
        let status = Command::new(env!("CARGO_BIN_EXE_branchpod"))
            .args(args)
            .arg(&self.worktree)
            .current_dir(&self.cwd)
            .env("BRANCHPOD_HOME", home)
            .stdout(log.try_clone()?)
            .stderr(log)
            .status()?;
        let out = String::from_utf8_lossy(&fs::read(&path)?).into_owned();
        Ok((status.code(), out))
    }

    fn bp(&self, home: &OsStr, args: &[&str]) -> (i32, String) {
        let (code, out) = self.try_bp(home, args).unwrap();
        (code.unwrap(), out)
    }
}

/// Every file below `dir`, relative to it, with `/`.
fn files(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, found);
            } else {
                let rel = path.strip_prefix(root).unwrap();
                found.push(rel.to_str().unwrap().replace('\\', "/"));
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, dir, &mut found);
    found.sort();
    found
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// `up`, `status` and `down` with `home` as the variable's value, which means `cwd/HOME`.
fn round_trip(s: &Setup, home: &OsStr) {
    let (code, out) = s.bp(home, &["up"]);
    assert_eq!(code, 0, "{out}");

    // The service's log, which the supervisor opens, is in the pod's directory and nowhere else.
    let all = files(&s.cwd);
    let logs: Vec<&String> = all
        .iter()
        .filter(|f| f.ends_with("/service-web.log"))
        .collect();
    assert_eq!(logs.len(), 1, "{all:?}");
    let pods = s.cwd.join(HOME).join("pods");
    let [pod] = &names(&pods)[..] else {
        panic!("{all:?}")
    };
    assert_eq!(*logs[0], format!("{HOME}/pods/{pod}/logs/service-web.log"));

    let (code, out) = s.bp(home, &["status", "--json"]);
    assert_eq!(code, 0, "{out}");
    let status: serde_json::Value = serde_json::from_str(&out).unwrap();
    let resources: Vec<_> = status["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        resources,
        [("supervisor", "live"), ("web", "live")],
        "{out}"
    );

    let (code, out) = s.bp(home, &["down"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("clean"), "{out}");
    // Lock files are kept on purpose; no pod is left and no second home inside the home.
    assert_eq!(names(&s.cwd), [HOME]);
    assert_eq!(names(&s.cwd.join(HOME)), ["locks", "pods"]);
    assert_eq!(files(&s.cwd.join(HOME)), [format!("locks/{pod}.lock")]);
    assert!(names(&pods).is_empty());
}

#[test]
fn nothing_is_left_after_down() {
    let s = Setup::new();
    round_trip(&s, HOME.as_ref());
}

/// The home does not exist before `up`, so it cannot be canonicalized.
#[test]
fn dots_in_the_path_are_resolved() {
    let s = Setup::new();
    let spelled = Path::new("..").join("cwd").join(".").join(HOME);
    round_trip(&s, spelled.as_os_str());
}

/// Relative to the current directory of a drive, and to its root.
#[cfg(windows)]
#[test]
fn drive_and_root_relative_paths_are_resolved() {
    let s = Setup::new();
    let cwd = s.cwd.to_str().unwrap();
    let (drive, rooted) = cwd.split_at(2);
    assert!(drive.ends_with(':') && rooted.starts_with('\\'), "{cwd}");
    round_trip(&s, format!("{drive}{HOME}").as_ref());
    // Worked before as well (the supervisor runs on the same drive): kept as a check that resolving
    // does not break it.
    round_trip(&s, format!("{rooted}\\{HOME}").as_ref());
}

/// A home that is a link stays a link and is used through it.
#[cfg(unix)]
#[test]
fn a_linked_home_works() {
    let s = Setup::new();
    let real = s.cwd.with_file_name("real");
    fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, s.cwd.join(HOME)).unwrap();
    round_trip(&s, HOME.as_ref());
    assert_eq!(names(&real), ["locks", "pods"]);
}
