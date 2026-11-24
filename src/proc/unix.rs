//! Process groups, sessions and start times on Unix.

use std::fs::File;
use std::io::{self, PipeWriter};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::{POLL, ProcId, Program, Tether, identify};

/// The leader of a process group.
pub(crate) struct Tree(Child);

impl Tree {
    pub(crate) fn spawn(cmd: &mut Command) -> io::Result<Tree> {
        cmd.process_group(0).spawn().map(Tree)
    }

    pub(crate) fn child(&mut self) -> &mut Child {
        &mut self.0
    }

    pub(crate) fn leader_exit(&self) -> io::Result<LeaderExit> {
        Ok(LeaderExit(self.0.id()))
    }

    /// Once the leader is reaped its pid may be reused, so what is left of its tree is then
    /// stopped by its recorded identity, never by the bare pid.
    pub(crate) fn stop(&mut self, leader: ProcId, grace: Duration) -> io::Result<()> {
        if self.0.try_wait().ok().flatten().is_some() {
            stop_recorded_tree(leader, grace)
        } else {
            stop_tree(&mut self.0, grace)
        }
    }
}

/// A pid of a child of ours.
pub(crate) struct LeaderExit(u32);

impl LeaderExit {
    /// Leaves the child a zombie (`WNOWAIT`), so this does not race with who reaps it: that one
    /// gets the status, and if it came first there is no such child any more. Should the pid be
    /// reused for another child just then, this waits for that one, which harms nobody.
    pub(crate) fn wait(self) -> io::Result<()> {
        loop {
            // SAFETY: plain data for which zeroes are valid, filled in by the call.
            let done = unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                libc::waitid(
                    libc::P_PID,
                    self.0 as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if done == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => {}
                Some(libc::ECHILD) => return Ok(()),
                _ => return Err(e),
            }
        }
    }
}

/// `SIGCHLD` may be ignored, which survives `exec`: the system then reaps our children, and their
/// status is lost.
pub(crate) fn collect_exits() -> io::Result<()> {
    // SAFETY: plain system call; no handler is installed.
    if unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Not yet: #15 (Linux), #16 (macOS).
pub(crate) fn contain() -> io::Result<()> {
    Ok(())
}

/// A session of its own is all it takes.
pub(crate) fn detached_tether() -> io::Result<Tether> {
    Ok(Tether::Free)
}

pub(crate) fn own_tether() -> io::Result<Tether> {
    Ok(Tether::Free)
}

fn stop_tree(child: &mut Child, grace: Duration) -> io::Result<()> {
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

pub(crate) struct Detached(Child);

impl Detached {
    pub(crate) fn pid(&self) -> u32 {
        self.0.id()
    }

    /// Waits for it to exit.
    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.0.wait()
    }
}

pub(crate) fn spawn_detached(
    program: &Program,
    cwd: &Path,
    stdout: PipeWriter,
    stderr: File,
) -> io::Result<Detached> {
    let mut cmd = Command::new(&program.exe);
    cmd.args(&program.args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    // SAFETY: `setsid` is async-signal-safe, as `pre_exec` requires.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().map(Detached)
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
