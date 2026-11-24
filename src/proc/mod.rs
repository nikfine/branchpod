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
//! - Windows: the service gets its own process group (console control events). Only the service
//!   process itself is terminated for now; its children survive it until services run in a Job
//!   Object (#14).

use std::io;
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
pub struct ProcId {
    pub pid: u32,
    pub start: u64,
}

/// The process with `pid` as it is now, if one is running. A zombie (exited, not reaped) is not.
pub fn identify(pid: u32) -> Option<ProcId> {
    sys::start_token(pid).map(|start| ProcId { pid, start })
}

/// Whether `id` is still running as the same process.
pub fn is_running(id: ProcId) -> bool {
    identify(id.pid) == Some(id)
}

/// Waits up to `limit` for `id` to exit; whether it did.
pub fn wait_gone(id: ProcId, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while is_running(id) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL);
    }
    true
}

/// Sets `cmd` up to start a new process tree that [`stop_tree`] stops as a whole.
pub fn new_tree(cmd: &mut Command) {
    sys::new_tree(cmd);
}

/// Stops the tree of `child`, started with [`new_tree`]: asks it to exit, waits up to `grace`,
/// then kills it. The child is reaped.
pub fn stop_tree(child: &mut Child, grace: Duration) -> io::Result<()> {
    sys::stop_tree(child, grace)
}

/// Stops a tree that is not our child, known by its recorded leader: the supervisor that started
/// it is gone or does not answer. Nothing happens if the leader's pid now belongs to another
/// process.
pub fn stop_recorded_tree(leader: ProcId, grace: Duration) -> io::Result<()> {
    sys::stop_recorded_tree(leader, grace)
}

/// Kills `id` at once, if it is still that process.
pub fn kill(id: ProcId) -> io::Result<()> {
    sys::kill(id)
}

/// Spawns `cmd` detached from the caller, to outlive it: its own session (Unix); no console
/// window, its own process group and, where the caller's Job Object allows it, outside that job
/// (Windows). The caller's own standard handles are not inherited, so a caller waiting for the end
/// of our output (an IDE hook) is not kept waiting by the detached process.
pub fn spawn_detached(cmd: &mut Command) -> io::Result<Child> {
    sys::spawn_detached(cmd)
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
