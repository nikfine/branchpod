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

#[cfg(unix)]
mod sys {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{POLL, ProcId, identify};

    pub fn new_tree(cmd: &mut Command) {
        cmd.process_group(0);
    }

    pub fn stop_tree(child: &mut Child, grace: Duration) -> io::Result<()> {
        let group = child.id() as libc::pid_t;
        stop_group(group, grace, || {
            // Reap the leader as soon as it exits: a zombie still counts as a group member.
            let _ = child.try_wait();
        })?;
        child.wait().map(drop)
    }

    pub fn stop_recorded_tree(leader: ProcId, grace: Duration) -> io::Result<()> {
        match identify(leader.pid) {
            // A pid is not reused while a process group with that id exists: another process
            // with the pid means the group is gone.
            Some(now) if now != leader => Ok(()),
            // The leader itself, or the leader gone and perhaps members of its group left. (A
            // process that reused the pid and is already a zombie also lands here; signalling a
            // group of that id then reaches nothing that is still running.)
            _ => stop_group(leader.pid as libc::pid_t, grace, || {}),
        }
    }

    /// `SIGTERM` to the group, up to `grace` for it to empty, then `SIGKILL`.
    fn stop_group(group: libc::pid_t, grace: Duration, mut reap: impl FnMut()) -> io::Result<()> {
        if !signal_group(group, libc::SIGTERM)? {
            reap();
            return Ok(());
        }
        let deadline = Instant::now() + grace;
        loop {
            reap();
            if !signal_group(group, 0)? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                signal_group(group, libc::SIGKILL)?;
                return Ok(());
            }
            thread::sleep(POLL);
        }
    }

    /// Sends `signal` (0: only checks) to process group `group`; whether the group exists.
    fn signal_group(group: libc::pid_t, signal: libc::c_int) -> io::Result<bool> {
        // SAFETY: plain system call; `group` > 0, so this never signals our own group or all
        // processes.
        if group <= 0 {
            return Ok(false);
        }
        if unsafe { libc::killpg(group, signal) } == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            // Exists, but not ours to signal: some member changed its credentials.
            Some(libc::EPERM) if signal == 0 => Ok(true),
            _ => Err(e),
        }
    }

    pub fn kill(id: ProcId) -> io::Result<()> {
        if identify(id.pid) != Some(id) {
            return Ok(());
        }
        // SAFETY: plain system call on a pid we just identified.
        if unsafe { libc::kill(id.pid as libc::pid_t, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ESRCH) => Ok(()),
            _ => Err(e),
        }
    }

    pub fn spawn_detached(cmd: &mut Command) -> io::Result<Child> {
        // SAFETY: `setsid` is async-signal-safe, as `pre_exec` requires.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn()
    }

    /// Clock ticks since boot at which the process started (`/proc/<pid>/stat`, field 22).
    #[cfg(target_os = "linux")]
    pub fn start_token(pid: u32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name (field 2) is in parentheses and may contain anything, `)` included.
        let (_, rest) = stat.rsplit_once(')')?;
        let mut fields = rest.split_whitespace();
        let state = fields.next()?;
        if state == "Z" || state == "X" {
            return None;
        }
        // Field 3 was the state; field 22 is the start time.
        fields.nth(18)?.parse().ok()
    }

    /// Start time in microseconds, from `proc_pidinfo`.
    #[cfg(target_os = "macos")]
    pub fn start_token(pid: u32) -> Option<u64> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the buffer is a `proc_bsdinfo` of the size passed.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        if n != size || info.pbi_status == libc::SZOMB {
            return None;
        }
        Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    /// Elsewhere: existence only, no protection against pid reuse.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn start_token(pid: u32) -> Option<u64> {
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        alive.then_some(0)
    }
}

#[cfg(windows)]
mod sys {
    use std::io;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, FILETIME, HANDLE,
        HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    };

    use super::ProcId;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    pub fn new_tree(cmd: &mut Command) {
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    /// Terminates the service process itself; its children survive until #14 (Job Object).
    pub fn stop_tree(child: &mut Child, _grace: Duration) -> io::Result<()> {
        child.kill()?;
        child.wait().map(drop)
    }

    pub fn stop_recorded_tree(leader: ProcId, _grace: Duration) -> io::Result<()> {
        kill(leader)
    }

    pub fn kill(id: ProcId) -> io::Result<()> {
        let process = match Process::open(id.pid, PROCESS_TERMINATE) {
            Ok(Some(process)) => process,
            Ok(None) => return Ok(()),
            // Not ours to terminate: fine if the pid now belongs to another process, an error
            // if it is still the recorded one (it must not be recorded as gone).
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                return match start_token(id.pid) {
                    Some(start) if start == id.start => Err(e),
                    _ => Ok(()),
                };
            }
            Err(e) => return Err(e),
        };
        if process.start_token() != Some(id.start) {
            return Ok(());
        }
        // SAFETY: a valid process handle opened with PROCESS_TERMINATE.
        if unsafe { TerminateProcess(process.0, 1) } == 0 {
            let e = io::Error::last_os_error();
            // Already exiting.
            if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) && !process.running() {
                return Ok(());
            }
            return Err(e);
        }
        Ok(())
    }

    pub fn spawn_detached(cmd: &mut Command) -> io::Result<Child> {
        // Our standard handles may be an IDE hook's pipes: inherited by a process that outlives
        // us, they would keep the hook waiting for the end of our output.
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: plain calls on our own standard handles; failures leave the flag as it was.
            unsafe {
                let handle = GetStdHandle(which);
                if !handle.is_null() && handle as isize != -1 {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
        let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        // Leave the caller's Job Object if it allows that, so that the job closing (a hook's
        // process tree being cleaned up) does not take the supervisor with it.
        cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                cmd.creation_flags(flags);
                cmd.spawn()
            }
            result => result,
        }
    }

    /// `None` also when the process is not ours to query: then it is not one we started.
    pub fn start_token(pid: u32) -> Option<u64> {
        let process = Process::open(pid, 0).ok()??;
        if !process.running() {
            return None;
        }
        process.start_token()
    }

    /// An open process handle, closed on drop.
    struct Process(HANDLE);

    impl Process {
        /// `None` when there is no such process. Access denied is an error for the caller to
        /// judge: the pid may belong to someone else's process, or be ours and protected.
        fn open(pid: u32, access: u32) -> io::Result<Option<Process>> {
            let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | access;
            // SAFETY: plain call; the handle is owned by the returned value.
            let handle = unsafe { OpenProcess(access, 0, pid) };
            if handle.is_null() {
                let e = io::Error::last_os_error();
                return match e.raw_os_error() {
                    Some(code) if code == ERROR_INVALID_PARAMETER as i32 => Ok(None),
                    _ => Err(e),
                };
            }
            Ok(Some(Process(handle)))
        }

        /// Not signalled yet. The exit code is set as soon as termination starts, while the
        /// process still holds its handles (a named pipe accepts connections); the process object
        /// is signalled only once they are closed.
        fn running(&self) -> bool {
            // SAFETY: valid handle opened with PROCESS_SYNCHRONIZE.
            unsafe { WaitForSingleObject(self.0, 0) == WAIT_TIMEOUT }
        }

        /// Creation time as a FILETIME.
        fn start_token(&self) -> Option<u64> {
            let zero = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
            // SAFETY: valid handle with query access; four FILETIME out-parameters.
            let ok = unsafe {
                GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user)
            } != 0;
            ok.then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: we own the handle.
            unsafe { CloseHandle(self.0) };
        }
    }
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
