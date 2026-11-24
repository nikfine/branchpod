//! Process groups, sessions and start times on Unix.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use super::{POLL, ProcId, identify};

pub(crate) fn new_tree(cmd: &mut Command) {
    cmd.process_group(0);
}

pub(crate) fn stop_tree(child: &mut Child, grace: Duration) -> io::Result<()> {
    let group = child.id() as libc::pid_t;
    stop_group(group, grace, || {
        // Reap the leader as soon as it exits: a zombie still counts as a group member.
        let _ = child.try_wait();
    })?;
    child.wait().map(drop)
}

pub(crate) fn stop_recorded_tree(leader: ProcId, grace: Duration) -> io::Result<()> {
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

pub(crate) fn kill(id: ProcId) -> io::Result<()> {
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

pub(crate) fn spawn_detached(cmd: &mut Command) -> io::Result<Child> {
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
pub(crate) fn start_token(pid: u32) -> Option<u64> {
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
pub(crate) fn start_token(pid: u32) -> Option<u64> {
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
pub(crate) fn start_token(pid: u32) -> Option<u64> {
    // SAFETY: signal 0 only checks that the process exists.
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
        || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    alive.then_some(0)
}
