//! Process handles, Job Objects, creation flags and start times on Windows.

use std::io;
use std::mem;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, FILETIME, HANDLE,
    HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT,
    JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOBOBJECTINFOCLASS,
    JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenThread, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, ResumeThread, THREAD_SUSPEND_RESUME, TerminateProcess,
    WaitForSingleObject,
};

use super::{POLL, ProcId, Tether};

const CREATE_SUSPENDED: u32 = 0x0000_0004;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// How long [`kill`] waits for a process that is already being terminated.
const EXITING_WAIT: Duration = Duration::from_secs(10);

/// A service: its process, in a job that everything it starts stays in.
pub(crate) struct Tree {
    child: Child,
    job: Job,
}

impl Tree {
    /// The process is created suspended and put into its job before it runs, so nothing it
    /// starts escapes. Should we be killed in between, it dies with us in the job of
    /// [`contain`], where it was created.
    pub(crate) fn spawn(cmd: &mut Command) -> io::Result<Tree> {
        let job = Job::new()?;
        let mut child = cmd
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED)
            .spawn()?;
        let started = job
            .assign(child.as_raw_handle())
            .and_then(|()| resume(child.id()));
        if let Err(e) = started {
            // Not killed, it stays suspended: waiting for it would never end.
            if child.kill().is_ok() {
                let _ = child.wait();
            }
            return Err(e);
        }
        Ok(Tree { child, job })
    }

    pub(crate) fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Waits for the processes themselves, not only for the job to count none: it stops counting
    /// one before that process is gone. One that appears in neither list (created after the first
    /// and no longer counted by the second, or not to be opened) is only waited for by the count.
    pub(crate) fn stop(&mut self, _leader: ProcId, grace: Duration) -> io::Result<()> {
        let deadline = Instant::now() + grace;
        let left = || deadline.saturating_duration_since(Instant::now());
        loop {
            let mut processes = self.job.processes()?;
            self.job.terminate()?;
            processes.extend(self.job.processes()?);
            let mut alive = 0;
            for process in &processes {
                if !process.exited_within(left()) {
                    alive += 1;
                }
            }
            let active = self.job.active()?;
            if alive == 0 && active == 0 {
                break;
            }
            if left().is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{} of its processes did not exit after being terminated",
                        active.max(alive)
                    ),
                ));
            }
            thread::sleep(POLL);
        }
        self.child.wait().map(drop)
    }
}

/// Puts this process into a job of its own. The only handle to it is ours, is not inherited and
/// is never closed: the system closes it when we end, and then kills what is in the job. The job
/// does not allow breakaway.
pub(crate) fn contain() -> io::Result<()> {
    let job = Job::new()?;
    // SAFETY: a pseudo handle that is always valid.
    job.assign(unsafe { GetCurrentProcess() })?;
    // Closing it now would kill us.
    let _ = job.0.into_raw_handle();
    Ok(())
}

pub(crate) fn detached_tether() -> io::Result<Tether> {
    Ok(match current_job_limits()? {
        // It starts outside the job (CREATE_BREAKAWAY_FROM_JOB), though perhaps in an outer one.
        Some(limits) if can_leave(limits) => Tether::Free,
        limits => tether(limits),
    })
}

pub(crate) fn own_tether() -> io::Result<Tether> {
    current_job_limits().map(tether)
}

fn can_leave(limits: JOB_OBJECT_LIMIT) -> bool {
    limits & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK) != 0
}

/// What being in a job with `limits` (`None`: in no job) means for a process that stays there.
fn tether(limits: Option<JOB_OBJECT_LIMIT>) -> Tether {
    match limits {
        None => Tether::Free,
        Some(limits) if limits & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0 => Tether::Kills(
            "the supervisor would be killed together with the program that runs `bp up`: that \
             program's Job Object kills everything in it when it is closed \
             (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE) and does not let `bp` out of it; run `bp up` \
             outside of that job (from a terminal, say), or have the program allow breakaway \
             (JOB_OBJECT_LIMIT_BREAKAWAY_OK)",
        ),
        Some(_) => Tether::Unclear(
            "the supervisor runs in a Job Object it cannot leave: that job is not set to kill it, \
             but whoever owns the job, or an outer job that Windows does not show, still can, and \
             the services with it; if they disappear, run `bp up` outside of that job",
        ),
    }
}

/// Limits of the job this process is in, `None` if in no job. With nested jobs only the innermost
/// one can be queried.
fn current_job_limits() -> io::Result<Option<JOB_OBJECT_LIMIT>> {
    let mut in_job = 0;
    // SAFETY: a pseudo handle, no job (any of them) and an out-parameter.
    check(unsafe { IsProcessInJob(GetCurrentProcess(), ptr::null_mut(), &mut in_job) })?;
    if in_job == 0 {
        return Ok(None);
    }
    // SAFETY: the class matches the structure; null is the current process's job.
    let info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
        unsafe { query(ptr::null_mut(), JobObjectExtendedLimitInformation)? };
    Ok(Some(info.BasicLimitInformation.LimitFlags))
}

/// # Safety
/// `T` must be the plain structure that `class` stands for; `job` a job handle or null.
unsafe fn query<T>(job: HANDLE, class: JOBOBJECTINFOCLASS) -> io::Result<T> {
    // SAFETY: plain data for which zeroes are valid, of the size passed.
    unsafe {
        let mut info: T = mem::zeroed();
        check(QueryInformationJobObject(
            job,
            class,
            (&raw mut info).cast(),
            mem::size_of::<T>() as u32,
            ptr::null_mut(),
        ))?;
        Ok(info)
    }
}

fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A Job Object whose processes are killed when its handle, which is not inherited, is closed.
struct Job(OwnedHandle);

impl Job {
    fn new() -> io::Result<Job> {
        // SAFETY: plain call; the handle is owned by the returned value. No attributes: not
        // inherited.
        let job = unsafe {
            let handle = CreateJobObjectW(ptr::null(), ptr::null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            Job(OwnedHandle::from_raw_handle(handle))
        };
        // SAFETY: plain data for which zeroes are valid, of the size passed.
        unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            check(SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                mem::size_of_val(&info) as u32,
            ))?;
        }
        Ok(job)
    }

    fn assign(&self, process: HANDLE) -> io::Result<()> {
        // SAFETY: two valid handles.
        check(unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), process) })
    }

    /// Starts terminating everything in it; does not wait.
    fn terminate(&self) -> io::Result<()> {
        // SAFETY: a valid handle.
        check(unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) })
    }

    /// The processes in it, as far as they can be opened.
    fn processes(&self) -> io::Result<Vec<Process>> {
        const IDS: usize = mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList);
        let mut capacity = 64;
        loop {
            // `usize`: as the structure's alignment and its ids.
            let mut buffer = vec![0usize; IDS.div_ceil(mem::size_of::<usize>()) + capacity];
            // SAFETY: a valid handle and a buffer of the size passed, aligned for the structure.
            let listed = check(unsafe {
                QueryInformationJobObject(
                    self.0.as_raw_handle(),
                    JobObjectBasicProcessIdList,
                    buffer.as_mut_ptr().cast(),
                    mem::size_of_val(buffer.as_slice()) as u32,
                    ptr::null_mut(),
                )
            });
            match listed {
                Err(e) if e.raw_os_error() == Some(ERROR_MORE_DATA as i32) => {
                    capacity *= 2;
                    continue;
                }
                result => result?,
            }
            let list = buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            // SAFETY: the call filled in the structure, with this many ids after its header.
            let (ids, assigned) = unsafe {
                let n = (*list).NumberOfProcessIdsInList as usize;
                let first = (&raw const (*list).ProcessIdList).cast::<usize>();
                (
                    std::slice::from_raw_parts(first, n),
                    (*list).NumberOfAssignedProcesses as usize,
                )
            };
            if ids.len() < assigned && capacity < assigned {
                capacity = assigned * 2;
                continue;
            }
            return Ok(ids
                .iter()
                .filter_map(|&pid| Process::open(pid as u32, 0).ok().flatten())
                // The pid may have been reused since.
                .filter(|process| self.contains(process))
                .collect());
        }
    }

    fn contains(&self, process: &Process) -> bool {
        let mut inside = 0;
        // SAFETY: two valid handles and an out-parameter.
        unsafe {
            IsProcessInJob(process.0, self.0.as_raw_handle(), &mut inside) != 0 && inside != 0
        }
    }

    /// How many processes are in it.
    fn active(&self) -> io::Result<u32> {
        // SAFETY: a valid handle; the class matches the structure.
        let info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
            unsafe { query(self.0.as_raw_handle(), JobObjectBasicAccountingInformation)? };
        Ok(info.ActiveProcesses)
    }
}

/// Lets a process created suspended run: its main thread, which std gives no handle to. Others
/// (injected by security software) may come and go meanwhile: they are not suspended.
fn resume(pid: u32) -> io::Result<()> {
    // SAFETY: plain calls with the structure's size set; every handle is owned and closed.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snapshot = OwnedHandle::from_raw_handle(snapshot);
        let mut entry: THREADENTRY32 = mem::zeroed();
        entry.dwSize = mem::size_of::<THREADENTRY32>() as u32;
        let mut resumed = false;
        let mut error = io::Error::other("the new process has no suspended thread");
        let mut more = Thread32First(snapshot.as_raw_handle(), &mut entry);
        while more != 0 {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    error = io::Error::last_os_error();
                } else {
                    let thread = OwnedHandle::from_raw_handle(thread);
                    // The previous suspend count: 0 is a thread that was running already.
                    match ResumeThread(thread.as_raw_handle()) {
                        u32::MAX => error = io::Error::last_os_error(),
                        previous => resumed |= previous > 0,
                    }
                }
            }
            more = Thread32Next(snapshot.as_raw_handle(), &mut entry);
        }
        if resumed { Ok(()) } else { Err(error) }
    }
}

/// The leader only: the rest of the tree is in the supervisor's jobs and dies with it.
pub(crate) fn stop_recorded_tree(leader: ProcId, grace: Duration) -> io::Result<()> {
    if let Some(process) = terminate(leader, grace)?
        && !process.exited_within(grace)
    {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("pid {} did not exit after being terminated", leader.pid),
        ));
    }
    Ok(())
}

pub(crate) fn kill(id: ProcId) -> io::Result<()> {
    terminate(id, EXITING_WAIT).map(drop)
}

/// Terminates `id` if it is still that process, without waiting for it to exit. `None` if there
/// is nothing left to wait for. A process that is already being terminated (with its job) refuses
/// with "access denied", like one that is not ours to terminate: that is fine only if it does
/// exit within `limit`.
fn terminate(id: ProcId, limit: Duration) -> io::Result<Option<Process>> {
    let process = match Process::open(id.pid, PROCESS_TERMINATE) {
        Ok(Some(process)) => process,
        Ok(None) => return Ok(None),
        // Not ours to terminate: fine if the pid now belongs to another process, an error
        // if it is still the recorded one (it must not be recorded as gone).
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            return match start_token(id.pid) {
                Some(start) if start == id.start => Err(e),
                _ => Ok(None),
            };
        }
        Err(e) => return Err(e),
    };
    if process.start_token() != Some(id.start) {
        return Ok(None);
    }
    // SAFETY: a valid process handle opened with PROCESS_TERMINATE.
    if unsafe { TerminateProcess(process.0, 1) } == 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) && process.exited_within(limit) {
            return Ok(None);
        }
        return Err(e);
    }
    Ok(Some(process))
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
    // process tree being cleaned up) does not take the supervisor with it. Where it does not,
    // the supervisor finds out (`own_tether`) and refuses or warns.
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

    /// Waits up to `limit` for it to exit; whether it did.
    fn exited_within(&self, limit: Duration) -> bool {
        let ms = u32::try_from(limit.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: valid handle opened with PROCESS_SYNCHRONIZE.
        unsafe { WaitForSingleObject(self.0, ms) == WAIT_OBJECT_0 }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_tell_what_becomes_of_a_process_that_stays_in_the_job() {
        assert_eq!(tether(None), Tether::Free);
        assert!(matches!(tether(Some(0)), Tether::Unclear(_)));
        assert!(matches!(
            tether(Some(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)),
            Tether::Kills(_)
        ));
        assert!(!can_leave(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE));
        assert!(can_leave(JOB_OBJECT_LIMIT_BREAKAWAY_OK));
        assert!(can_leave(
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK
        ));
    }

    /// Name of the env var that makes [`contained_process`] run.
    const CONTAIN_ENV: &str = "BP_TEST_CONTAIN";

    /// Not a test by itself.
    #[test]
    #[ignore = "a process of another test, which starts it itself"]
    fn contained_process() {
        if std::env::var_os(CONTAIN_ENV).is_none() {
            return;
        }
        // A job of known limits around it, whatever the test runner has.
        let outer = Job::new().unwrap();
        // SAFETY: plain data for which zeroes are valid, of the size passed.
        unsafe {
            let info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
            check(SetInformationJobObject(
                outer.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                mem::size_of_val(&info) as u32,
            ))
            .unwrap();
            outer.assign(GetCurrentProcess()).unwrap();
        }
        assert!(matches!(own_tether().unwrap(), Tether::Unclear(_)));
        assert!(matches!(detached_tether().unwrap(), Tether::Unclear(_)));
        contain().unwrap();
        assert_eq!(
            current_job_limits().unwrap(),
            Some(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)
        );
        println!("contained");
    }

    #[test]
    fn contain_ties_what_we_start_to_us() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "proc::windows::tests::contained_process",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CONTAIN_ENV, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("contained"),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_process_that_is_exiting_already_is_waited_for() {
        let mut child = Command::new("ping")
            .args(["-n", "300", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let id = super::super::identify(child.id()).unwrap();
        child.kill().unwrap();
        // Terminated, perhaps not gone yet; then gone, but the handle keeps the pid taken.
        stop_recorded_tree(id, Duration::from_secs(10)).unwrap();
        child.wait().unwrap();
        stop_recorded_tree(id, Duration::from_secs(10)).unwrap();
        kill(id).unwrap();
    }

    #[test]
    fn a_job_with_many_processes_is_listed_whole() {
        let job = Job::new().unwrap();
        let mut children: Vec<Child> = (0..70)
            .map(|_| {
                Command::new("ping")
                    .args(["-n", "300", "127.0.0.1"])
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in &children {
            job.assign(child.as_raw_handle()).unwrap();
        }
        let listed = job.processes().unwrap().len();
        job.terminate().unwrap();
        for child in &mut children {
            child.wait().unwrap();
        }
        assert_eq!(listed, children.len());
    }
}
