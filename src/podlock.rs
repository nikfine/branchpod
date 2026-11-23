//! The pod lock: one `bp up` / `bp down` at a time per worktree.
//!
//! IDE hooks may fire `up` and `down` for the same worktree in quick succession; the second one waits
//! for the first instead of racing it over the ledger and the resources.
//!
//! The lock is an OS file lock (`flock` on Unix, `LockFileEx` on Windows) on `locks/<pod id>.lock` in
//! the `bp` home. The OS releases it whenever the holder exits, crashes or is killed, so a lock is
//! never stale and nothing has to detect or take one over. The handle is not inherited by child
//! processes (std opens files close-on-exec / non-inheritable), so a command or service started
//! while the lock is held does not keep it after `bp` exits.
//!
//! Lock files live outside `pods/<id>/` and are never deleted: removing a lock file while another
//! process waits on it would let that process lock the unlinked file while a third one creates a new
//! file, and both would hold "the" lock at once.
//!
//! Waiting has no timeout: a waiting process cannot tell a long `up` (first image pull, dependency
//! install) from a hung one. Steps bound their own run time instead; the waiter reports who holds the
//! lock so that a hung holder can be found and stopped.
//!
//! Lock order: the pod lock is taken before the port registry lock, never while holding it.

use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::fsutil;
use crate::pod::Store;

const HOLDER_VERSION: u32 = 1;

/// Who holds a pod lock, as recorded by the holder in `locks/<pod id>.holder`.
///
/// Informational only, for the message a waiting process prints: the lock itself is the OS lock.
/// It is a separate file because Windows locks are mandatory, so a locked file cannot be read by
/// another process. A holder that crashed leaves it behind; the next holder overwrites it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub v: u32,
    pub pid: u32,
    /// The `bp` subcommand holding the lock (`up`, `down`, ...).
    pub command: String,
    /// When the lock was taken, in seconds since the Unix epoch.
    pub since: u64,
}

impl fmt::Display for Holder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bp {} (pid {}", self.command, self.pid)?;
        if let Some(secs) = now().checked_sub(self.since) {
            write!(f, ", running for {secs}s")?;
        }
        write!(f, ")")
    }
}

/// A held pod lock; released when dropped.
#[derive(Debug)]
pub struct PodLock {
    holder: PathBuf,
    // Kept open for the lifetime of the lock; closing the handle unlocks it.
    _file: File,
}

impl Drop for PodLock {
    fn drop(&mut self) {
        // Still locked here (fields are dropped after this body), so this never removes the record
        // of the next holder.
        let _ = fs::remove_file(&self.holder);
    }
}

impl Store {
    pub fn lock_path(&self, id: &str) -> PathBuf {
        self.home().join("locks").join(format!("{id}.lock"))
    }

    fn holder_path(&self, id: &str) -> PathBuf {
        self.home().join("locks").join(format!("{id}.holder"))
    }

    /// Takes the lock of pod `id` for `command`, waiting as long as another process holds it.
    ///
    /// If the lock is busy, `on_wait` is called once before waiting, with the current holder when
    /// it is known (it may be missing or outdated for a moment right after the lock changes hands).
    pub fn lock_pod(
        &self,
        id: &str,
        command: &str,
        on_wait: impl FnOnce(Option<&Holder>),
    ) -> Result<PodLock> {
        let path = self.lock_path(id);
        let file = open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                on_wait(self.holder(id).as_ref());
                file.lock()
                    .with_context(|| format!("cannot lock {}", path.display()))?;
            }
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("cannot lock {}", path.display()));
            }
        }
        self.record_holder(id, command, file)
    }

    /// Takes the lock of pod `id` only if it is free right now.
    pub fn try_lock_pod(&self, id: &str, command: &str) -> Result<Option<PodLock>> {
        let path = self.lock_path(id);
        let file = open(&path)?;
        match file.try_lock() {
            Ok(()) => self.record_holder(id, command, file).map(Some),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("cannot lock {}", path.display()))
            }
        }
    }

    /// The recorded holder of pod `id`'s lock, if any. Unreadable or damaged records count as none.
    pub fn holder(&self, id: &str) -> Option<Holder> {
        let bytes = fs::read(self.holder_path(id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn record_holder(&self, id: &str, command: &str, file: File) -> Result<PodLock> {
        let lock = PodLock {
            holder: self.holder_path(id),
            _file: file,
        };
        let holder = Holder {
            v: HOLDER_VERSION,
            pid: std::process::id(),
            command: command.to_owned(),
            since: now(),
        };
        // The record only improves a waiting process's message; failing to write it must not
        // fail the command that now holds the lock.
        if let Err(e) = fsutil::write_atomic(&lock.holder, &serde_json::to_vec(&holder)?) {
            eprintln!("bp: warning: cannot write {}: {e}", lock.holder.display());
        }
        Ok(lock)
    }
}

fn open(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const POD: &str = "feature-0badc0de";

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().to_path_buf());
        (dir, store)
    }

    /// Polls until the lock is free, so that a regression fails the test instead of hanging it.
    fn wait_free(store: &Store, id: &str) -> PodLock {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(lock) = store.try_lock_pod(id, "test").unwrap() {
                return lock;
            }
            assert!(Instant::now() < deadline, "lock of {id} was never released");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn second_lock_waits_for_the_first() {
        let (dir, store) = store();
        let first = store.lock_pod(POD, "up", |_| panic!("free lock")).unwrap();
        assert_eq!(store.holder(POD).unwrap().command, "up");
        assert_eq!(store.holder(POD).unwrap().pid, std::process::id());

        let home = dir.path().to_path_buf();
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (locked_tx, locked_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            // A separate store and file handle, as a separate process would have.
            let store = Store::at(home);
            let lock = store
                .lock_pod(POD, "down", |holder| {
                    waiting_tx.send(holder.cloned()).unwrap();
                })
                .unwrap();
            locked_tx.send(()).unwrap();
            drop(lock);
        });

        let seen = waiting_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(seen.unwrap().command, "up");
        assert!(locked_rx.recv_timeout(Duration::from_millis(200)).is_err());

        drop(first);
        locked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        waiter.join().unwrap();
        assert_eq!(store.holder(POD), None);
        assert!(
            store.lock_path(POD).exists(),
            "lock files are never removed"
        );
    }

    #[test]
    fn pods_do_not_block_each_other() {
        let (_dir, store) = store();
        let _a = store.lock_pod("a-00000000", "up", |_| panic!()).unwrap();
        let b = store.try_lock_pod("b-00000000", "up").unwrap();
        assert!(b.is_some());
        assert!(store.try_lock_pod("a-00000000", "down").unwrap().is_none());
    }

    #[test]
    fn missing_or_damaged_holder_record_is_tolerated() {
        let (_dir, store) = store();
        let lock = store.lock_pod(POD, "up", |_| panic!()).unwrap();
        fs::write(store.holder_path(POD), b"{").unwrap();
        assert_eq!(store.holder(POD), None);
        drop(lock);
        let _lock = store.lock_pod(POD, "down", |_| panic!()).unwrap();
        assert_eq!(store.holder(POD).unwrap().command, "down");
    }

    #[test]
    fn holder_message_names_command_and_pid() {
        let holder = Holder {
            v: HOLDER_VERSION,
            pid: 4242,
            command: "up".into(),
            since: now() - 5,
        };
        let text = holder.to_string();
        assert!(text.starts_with("bp up (pid 4242, running for "), "{text}");
    }

    // Cross-process tests: the test binary re-runs itself as a lock holder (the ignored test
    // `holder_process` below), so the lock is held by a real separate process.

    const HOME_ENV: &str = "BP_TEST_PODLOCK_HOME";
    const MODE_ENV: &str = "BP_TEST_PODLOCK_MODE";

    fn spawn_self(test: &str, envs: &[(&str, &Path)], mode: &str) -> Child {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            test,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(MODE_ENV, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd.spawn().unwrap()
    }

    /// Reads the child's stdout until a line containing `marker`; returns the rest of that line.
    fn wait_line(child: &mut Child, marker: &str) -> String {
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let prefix = marker.to_owned();
        std::thread::spawn(move || {
            let marker = prefix;
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // libtest prints the test name on the same line before the output.
                if let Some((_, rest)) = line.split_once(&marker) {
                    let _ = tx.send(rest.trim().to_owned());
                    break;
                }
            }
        });
        rx.recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| panic!("child never printed {marker}"))
    }

    fn kill(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn lock_is_released_when_the_holder_is_killed() {
        let (dir, store) = store();
        let mut holder = spawn_self(
            "podlock::tests::holder_process",
            &[(HOME_ENV, dir.path())],
            "hold",
        );
        wait_line(&mut holder, "BP-LOCKED");

        assert!(store.try_lock_pod(POD, "down").unwrap().is_none());
        let recorded = store.holder(POD).unwrap();
        assert_eq!(recorded.pid, holder.id());
        assert_eq!(recorded.command, "up");

        kill(&mut holder);
        let _lock = wait_free(&store, POD);
        // The killed holder left its record behind; the new holder replaced it.
        assert_eq!(store.holder(POD).unwrap().command, "test");
    }

    #[test]
    fn child_processes_do_not_inherit_the_lock() {
        let (dir, store) = store();
        let mut holder = spawn_self(
            "podlock::tests::holder_process",
            &[(HOME_ENV, dir.path())],
            "spawn",
        );
        let grandchild: u32 = wait_line(&mut holder, "BP-GRANDCHILD").parse().unwrap();
        kill(&mut holder);

        // The grandchild was started while the lock was held and is still running.
        let lock = wait_free(&store, POD);
        drop(lock);
        kill_pid(grandchild);
    }

    fn kill_pid(pid: u32) {
        let pid = pid.to_string();
        let _ = if cfg!(windows) {
            Command::new("taskkill").args(["/F", "/PID", &pid]).output()
        } else {
            Command::new("kill").args(["-9", &pid]).output()
        };
    }

    /// Not a test by itself: the lock holder process for the cross-process tests.
    #[test]
    #[ignore]
    fn holder_process() {
        let Some(home) = std::env::var_os(HOME_ENV) else {
            return;
        };
        let store = Store::at(PathBuf::from(home));
        let _lock = store
            .lock_pod(POD, "up", |_| panic!("lock is busy"))
            .unwrap();
        // Started with the lock held; it must not keep the lock after we are gone.
        let mut child = if std::env::var(MODE_ENV).as_deref() == Ok("spawn") {
            let child = spawn_self("podlock::tests::sleeper_process", &[], "sleep");
            println!("BP-GRANDCHILD {}", child.id());
            Some(child)
        } else {
            println!("BP-LOCKED");
            None
        };
        // Held until killed by the parent test.
        std::thread::sleep(Duration::from_secs(120));
        if let Some(child) = &mut child {
            kill(child);
        }
    }

    /// Not a test by itself: a long-running grandchild for `child_processes_do_not_inherit_the_lock`.
    #[test]
    #[ignore]
    fn sleeper_process() {
        if std::env::var(MODE_ENV).as_deref() == Ok("sleep") {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}
