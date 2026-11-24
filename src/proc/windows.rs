//! Process handles, creation flags and start times on Windows.

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

pub(crate) fn new_tree(cmd: &mut Command) {
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

/// Terminates the service process itself; its children survive until #14 (Job Object).
pub(crate) fn stop_tree(child: &mut Child, _grace: Duration) -> io::Result<()> {
    child.kill()?;
    child.wait().map(drop)
}

pub(crate) fn stop_recorded_tree(leader: ProcId, _grace: Duration) -> io::Result<()> {
    kill(leader)
}

pub(crate) fn kill(id: ProcId) -> io::Result<()> {
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

pub(crate) fn spawn_detached(cmd: &mut Command) -> io::Result<Child> {
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
pub(crate) fn start_token(pid: u32) -> Option<u64> {
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
        let ok =
            unsafe { GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) }
                != 0;
        ok.then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe { CloseHandle(self.0) };
    }
}
