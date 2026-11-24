//! Processes: identity, process trees and detached starts, one implementation per platform.
//!
//! A process is identified by its pid together with a start token, an opaque number the OS gives
//! for the moment the process started (not wall-clock time). A pid alone may have been reused by
//! another process since it was recorded; a pid with a different start token is somebody else.
//!
//! A service runs as a process tree that is stopped as a whole:
//!
//! - Unix: the service leads its own process group; stopping sends `SIGTERM` to the group, waits
//!   up to a grace period and then sends `SIGKILL`.
//! - Windows: the service runs in a Job Object of its own, nested in the supervisor's
//!   ([`contain`]); both kill what is in them when the supervisor is gone, however it ended.
//!   Stopping terminates the job at once and waits until nothing runs in it.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, PipeReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;

/// How often a stop polls for the processes to be gone.
const POLL: Duration = Duration::from_millis(25);

/// A process: pid plus start token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcId {
    pub(crate) pid: u32,
    pub(crate) start: u64,
}

/// The process with `pid` as it is now, if one is running. A zombie (exited, not reaped) is not.
pub(crate) fn identify(pid: u32) -> Option<ProcId> {
    sys::start_token(pid).map(|start| ProcId { pid, start })
}

/// Whether `id` is still running as the same process.
pub(crate) fn is_running(id: ProcId) -> bool {
    identify(id.pid) == Some(id)
}

/// Waits up to `limit` for `id` to exit; whether it did.
pub(crate) fn wait_gone(id: ProcId, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while is_running(id) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL);
    }
    true
}

/// A process tree started by us. On Windows dropping it kills what is left of it.
pub(crate) struct Tree(sys::Tree);

impl Tree {
    /// Starts `cmd` as the leader of a new tree.
    pub(crate) fn spawn(cmd: &mut Command) -> io::Result<Tree> {
        sys::Tree::spawn(cmd).map(Tree)
    }

    /// The leader.
    pub(crate) fn child(&mut self) -> &mut Child {
        self.0.child()
    }

    /// For another thread to wait for the leader to exit, while this tree is used as before.
    pub(crate) fn leader_exit(&self) -> io::Result<LeaderExit> {
        self.0.leader_exit().map(LeaderExit)
    }

    /// Stops the tree, whether or not its leader has exited already, and reaps the leader. On
    /// Unix it is asked to exit and killed after `grace`; on Windows it is killed at once, and
    /// `grace` limits the wait for all of it to be gone. `leader` is the leader's identity: its
    /// pid may have been reused.
    pub(crate) fn stop(&mut self, leader: ProcId, grace: Duration) -> io::Result<()> {
        self.0.stop(leader, grace)
    }
}

/// The exit of a tree's leader, to wait for.
pub(crate) struct LeaderExit(sys::LeaderExit);

impl LeaderExit {
    /// Returns once the leader has exited. It is not reaped: its exit status is still there to be
    /// collected, and on Unix its pid stays taken until then.
    pub(crate) fn wait(self) -> io::Result<()> {
        self.0.wait()
    }
}

/// Makes sure that the exit status of what this process starts can be collected, whatever the caller
/// left us with. For the supervisor, before it starts anything.
pub(crate) fn collect_exits() -> io::Result<()> {
    sys::collect_exits()
}

/// Ties whatever this process starts from now on to its own life, where the platform can: on
/// Windows they are killed when it ends, however it ends.
pub(crate) fn contain() -> io::Result<()> {
    sys::contain()
}

/// What may end a process of ours together with the caller, against our will.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(unix, allow(dead_code))]
pub(crate) enum Tether {
    /// Nothing that can be seen.
    Free,
    /// Nothing is set to, but something could: the text says what.
    Unclear(&'static str),
    /// It will be killed: the text says why, and what to do.
    Kills(&'static str),
}

/// The tether of a process started with [`spawn_detached`] now, as far as it can be told
/// beforehand; the process itself knows better ([`own_tether`]).
pub(crate) fn detached_tether() -> io::Result<Tether> {
    sys::detached_tether()
}

/// The tether of this process. Meaningless after [`contain`].
pub(crate) fn own_tether() -> io::Result<Tether> {
    sys::own_tether()
}

/// Stops a tree that is not our child, known by its recorded leader: the supervisor that started
/// it is gone or does not answer. Nothing happens if the leader's pid now belongs to another
/// process. On Windows only the leader is stopped and waited for: the rest dies with the
/// supervisor.
pub(crate) fn stop_recorded_tree(leader: ProcId, grace: Duration) -> io::Result<()> {
    sys::stop_recorded_tree(leader, grace)
}

/// Kills `id` at once, if it is still that process.
pub(crate) fn kill(id: ProcId) -> io::Result<()> {
    sys::kill(id)
}

/// A program to start and its arguments. Its environment is ours.
#[derive(Debug)]
pub(crate) struct Program {
    /// Absolute.
    pub(crate) exe: PathBuf,
    pub(crate) args: Vec<OsString>,
}

/// A process started with [`spawn_detached`]. Dropping it leaves the process running.
pub(crate) use sys::Detached;

/// Spawns `program` detached from the caller, to outlive it: its own session (Unix); no console
/// window, its own process group and, where the caller's Job Object allows it, outside that job
/// (Windows; [`Tether`] tells where it does not). Its stdin is empty, its stdout the pipe whose
/// other end is returned, its stderr the given file.
///
/// Our own standard handles are not inherited, so that a caller waiting for the end of our output
/// (an IDE hook) is not kept waiting by the detached process. On Windows nothing else is either;
/// on Unix a descriptor above 2 that the caller left open for us still is (#80).
pub(crate) fn spawn_detached(
    program: &Program,
    cwd: &Path,
    stderr: File,
) -> io::Result<(Detached, PipeReader)> {
    let (reader, writer) = io::pipe()?;
    let process = sys::spawn_detached(program, cwd, writer, stderr)?;
    Ok((process, reader))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_this_process() {
        let me = identify(std::process::id()).expect("we are running");
        assert!(is_running(me));
        assert!(!is_running(ProcId {
            pid: me.pid,
            start: me.start.wrapping_add(1)
        }));
        // Killing a process that is not that one any more does nothing.
        kill(ProcId {
            pid: me.pid,
            start: me.start.wrapping_add(1),
        })
        .unwrap();
    }
}
