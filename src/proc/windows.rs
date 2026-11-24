//! Process handles, Job Objects, creation flags and start times on Windows.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, PipeWriter};
use std::iter;
use std::marker::PhantomData;
use std::mem;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{
    AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, IntoRawHandle, OwnedHandle,
};
use std::os::windows::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ACCESS_DENIED,
    ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, FILETIME, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
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
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    GetExitCodeProcess, GetProcessTimes, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, OpenThread, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, THREAD_SUSPEND_RESUME, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::{POLL, ProcId, Program, Tether};

const CREATE_SUSPENDED: u32 = 0x0000_0004;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

const NUL: &str = r"\\.\NUL";

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

    pub(crate) fn leader_exit(&self) -> io::Result<LeaderExit> {
        self.child.as_handle().try_clone_to_owned().map(LeaderExit)
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

/// A handle of its own to the process.
pub(crate) struct LeaderExit(OwnedHandle);

impl LeaderExit {
    pub(crate) fn wait(self) -> io::Result<()> {
        // SAFETY: a valid process handle with all access.
        if unsafe { WaitForSingleObject(self.0.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
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

pub(crate) struct Detached {
    process: OwnedHandle,
    pid: u32,
}

impl Detached {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Waits for it to exit.
    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        let process = self.process.as_raw_handle();
        // SAFETY: a valid handle with all access, and an out-parameter.
        unsafe {
            if WaitForSingleObject(process, INFINITE) != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
            let mut code = 0;
            check(GetExitCodeProcess(process, &mut code))?;
            Ok(ExitStatus::from_raw(code))
        }
    }
}

/// `CreateProcessW` by hand: std's `Command` passes on every inheritable handle we have (a handle
/// list is unstable there), and `bp` may have some that it knows nothing about: `cmd` and
/// PowerShell can leave one on their own output to what they start. Held by a process that
/// outlives us, it would keep whoever waits for the end of that output waiting.
pub(crate) fn spawn_detached(
    program: &Program,
    cwd: &Path,
    stdout: PipeWriter,
    stderr: File,
) -> io::Result<Detached> {
    let stdin = File::open(NUL)?;
    // Copies, closed when we return, started or not. Meanwhile they are inheritable: a process
    // that another thread starts just then with std's `Command` gets them too, as std's lock does
    // not cover us. `bp` starts processes from one thread only; the tests from several, and a
    // readiness pipe or a log held open a little longer does them no harm.
    let stdio = [
        inheritable(stdin.as_handle())?,
        inheritable(stdout.as_handle())?,
        inheritable(stderr.as_handle())?,
    ];
    let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    // Leave the caller's Job Object if it allows that, so that the job closing (a hook's
    // process tree being cleaned up) does not take the supervisor with it. Where it does not,
    // the supervisor finds out (`own_tether`) and refuses or warns.
    match create_process(program, cwd, &stdio, flags | CREATE_BREAKAWAY_FROM_JOB) {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            create_process(program, cwd, &stdio, flags)
        }
        result => result,
    }
}

/// A copy of `handle` that processes we start can inherit.
fn inheritable(handle: BorrowedHandle) -> io::Result<OwnedHandle> {
    let mut copy = ptr::null_mut();
    // SAFETY: a valid handle and an out-parameter; the copy is owned by the returned value.
    unsafe {
        let this = GetCurrentProcess();
        check(DuplicateHandle(
            this,
            handle.as_raw_handle(),
            this,
            &mut copy,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        ))?;
        Ok(OwnedHandle::from_raw_handle(copy))
    }
}

/// Starts `program` with `stdio` as its standard handles and nothing else inherited. Handles that
/// are not inheritable, or twice the same one, are an error.
fn create_process(
    program: &Program,
    cwd: &Path,
    stdio: &[OwnedHandle; 3],
    flags: u32,
) -> io::Result<Detached> {
    let exe = wide(&executable(&program.exe))?;
    let mut command_line = command_line(program)?;
    let cwd = wide(&working_directory(cwd))?;
    let handles = stdio.each_ref().map(AsRawHandle::as_raw_handle);
    let mut attributes = Attributes::inherit_only(&handles)?;
    // SAFETY: plain data for which zeroes are valid.
    let (mut startup, mut created): (STARTUPINFOEXW, PROCESS_INFORMATION) =
        unsafe { (mem::zeroed(), mem::zeroed()) };
    startup.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    [
        startup.StartupInfo.hStdInput,
        startup.StartupInfo.hStdOutput,
        startup.StartupInfo.hStdError,
    ] = handles;
    startup.lpAttributeList = attributes.as_mut_ptr();
    // SAFETY: NUL-terminated strings, the command line writable; a startup structure of the size
    // it says, whose attribute list and the handles in it outlive the call; both handles returned
    // are owned. No environment: the child gets ours.
    unsafe {
        check(CreateProcessW(
            exe.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            flags | EXTENDED_STARTUPINFO_PRESENT,
            ptr::null(),
            cwd.as_ptr(),
            (&raw const startup).cast(),
            &mut created,
        ))?;
        drop(OwnedHandle::from_raw_handle(created.hThread));
        Ok(Detached {
            process: OwnedHandle::from_raw_handle(created.hProcess),
            pid: created.dwProcessId,
        })
    }
}

/// Attributes of a process to start.
struct Attributes<'a> {
    /// `usize` for the alignment.
    list: Vec<usize>,
    /// The list points to them.
    handles: PhantomData<&'a [HANDLE]>,
}

impl<'a> Attributes<'a> {
    /// Only `handles` are inherited.
    fn inherit_only(handles: &'a [HANDLE]) -> io::Result<Attributes<'a>> {
        let mut size = 0;
        // SAFETY: asks for the size only, by failing.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size) };
        let mut list = vec![0usize; size.div_ceil(mem::size_of::<usize>())];
        // SAFETY: a buffer of the size asked for.
        check(unsafe {
            InitializeProcThreadAttributeList(list.as_mut_ptr().cast(), 1, 0, &mut size)
        })?;
        // Initialized: from here on it is deleted on drop.
        let mut attributes = Attributes {
            list,
            handles: PhantomData,
        };
        // SAFETY: an initialized list, and a value of the size passed that outlives it.
        check(unsafe {
            UpdateProcThreadAttribute(
                attributes.as_mut_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                mem::size_of_val(handles),
                ptr::null_mut(),
                ptr::null(),
            )
        })?;
        Ok(attributes)
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.list.as_mut_ptr().cast()
    }
}

impl Drop for Attributes<'_> {
    fn drop(&mut self) {
        // SAFETY: an initialized list.
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

fn wide(text: &OsStr) -> io::Result<Vec<u16>> {
    terminated(text.encode_wide().collect())
}

/// Adds the terminating NUL; one that is there already is an error.
fn terminated(mut text: Vec<u16>) -> io::Result<Vec<u16>> {
    if text.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a NUL character in a path or an argument",
        ));
    }
    text.push(0);
    Ok(text)
}

/// `exe` in the form that may be longer than `MAX_PATH`, if it is long.
fn executable(exe: &Path) -> OsString {
    /// Well below `MAX_PATH` (260).
    const LONG: usize = 240;
    let path: Vec<u16> = exe.as_os_str().encode_wide().collect();
    // A device path (`\\.\`) only looks like a UNC path: it is left as it is.
    if path.len() < LONG || starts_with(&path, r"\\?\") || starts_with(&path, r"\\.\") {
        return exe.as_os_str().to_owned();
    }
    let (prefix, rest) = if starts_with(&path, r"\\") {
        (r"\\?\UNC\", &path[2..])
    } else {
        (r"\\?\", &path[..])
    };
    let mut verbatim = OsString::from(prefix);
    verbatim.push(OsString::from_wide(rest));
    verbatim
}

/// `cwd` without a `\\?\` prefix, which a working directory cannot have.
fn working_directory(cwd: &Path) -> OsString {
    const UNC: &str = r"\\?\UNC\";
    let path: Vec<u16> = cwd.as_os_str().encode_wide().collect();
    if starts_with(&path, UNC) {
        let mut plain = OsString::from(r"\\");
        plain.push(OsString::from_wide(&path[UNC.len()..]));
        return plain;
    }
    dunce::simplified(cwd).as_os_str().to_owned()
}

fn starts_with(path: &[u16], prefix: &str) -> bool {
    path.len() >= prefix.len()
        && prefix
            .encode_utf16()
            .eq(path[..prefix.len()].iter().copied())
}

/// The command line from which a program that follows the
/// [usual rules](https://learn.microsoft.com/en-us/cpp/c-language/parsing-c-command-line-arguments)
/// reads its own path and these arguments; NUL-terminated.
fn command_line(program: &Program) -> io::Result<Vec<u16>> {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    // The path is read up to the next quote, whatever is before it; a file name has none.
    let mut line = vec![QUOTE];
    line.extend(program.exe.as_os_str().encode_wide());
    line.push(QUOTE);
    for arg in &program.args {
        line.extend([b' ' as u16, QUOTE]);
        // Backslashes are only special before a quote: there they are doubled.
        let mut backslashes = 0;
        for unit in arg.encode_wide() {
            match unit {
                BACKSLASH => backslashes += 1,
                QUOTE => {
                    line.extend(iter::repeat_n(BACKSLASH, backslashes + 1));
                    backslashes = 0;
                }
                _ => backslashes = 0,
            }
            line.push(unit);
        }
        line.extend(iter::repeat_n(BACKSLASH, backslashes));
        line.push(QUOTE);
    }
    terminated(line)
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
    use std::fs;
    use std::io::{BufRead, BufReader};

    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

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

    /// Comes right before the arguments that [`reporting_process`] is to report.
    const REPORT_MARK: &str = "bp-test-report";

    /// Not a test by itself: reports its working directory, its arguments after [`REPORT_MARK`]
    /// and how many inheritable handles it has besides its standard ones.
    #[test]
    #[ignore = "a process of other tests, which start it themselves"]
    fn reporting_process() {
        let mut args = std::env::args_os().skip_while(|arg| arg != REPORT_MARK);
        if args.next().is_none() {
            return;
        }
        println!("cwd {:?}", std::env::current_dir().unwrap());
        for arg in args {
            println!("arg {:?}", arg.encode_wide().collect::<Vec<_>>());
        }
        // SAFETY: plain calls; a value that is no handle is an error.
        let (standard, inheritable) = unsafe {
            let standard =
                [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|w| GetStdHandle(w));
            // The values of handles are small multiples of four.
            let inheritable: Vec<HANDLE> = (4..0x10000)
                .step_by(4)
                .map(ptr::without_provenance_mut)
                .filter(|&handle| {
                    let mut flags = 0;
                    GetHandleInformation(handle, &mut flags) != 0
                        && flags & HANDLE_FLAG_INHERIT != 0
                })
                .collect();
            (standard, inheritable)
        };
        let of_standard = standard.iter().filter(|h| inheritable.contains(h)).count();
        println!(
            "standard {of_standard} other {}",
            inheritable.len() - of_standard
        );
        eprintln!("stderr");
        println!("end");
    }

    /// What [`reporting_process`], run as `exe` in `cwd`, reports on stdout and stderr.
    fn report(exe: &Path, args: &[OsString], cwd: &Path) -> (String, String) {
        let log = cwd.join("stderr.log");
        let program = Program {
            exe: exe.to_owned(),
            args: [
                "--exact",
                "proc::windows::tests::reporting_process",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
                // What follows are names of tests to run, whatever it looks like.
                "--",
                REPORT_MARK,
            ]
            .iter()
            .map(OsString::from)
            .chain(args.iter().cloned())
            .collect(),
        };
        let (mut child, stdout) =
            super::super::spawn_detached(&program, cwd, File::create(&log).unwrap()).unwrap();
        assert!(child.pid() != 0);
        // Up to its last line, not to the end: a process that another test starts just then may
        // hold the pipe too.
        let out: Vec<String> = BufReader::new(&stdout)
            .lines()
            .map(Result::unwrap)
            .take_while(|line| line != "end")
            .collect();
        let out = out.join("\n");
        let status = child.wait().unwrap();
        assert!(status.success(), "{status}: {out}");
        (out, fs::read_to_string(log).unwrap())
    }

    fn temp_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = dunce::canonicalize(tmp.path()).unwrap();
        (tmp, dir)
    }

    #[test]
    fn a_detached_process_inherits_only_its_standard_handles() {
        // NUL: whoever else inherits it does not matter.
        let nul = File::open(NUL).unwrap();
        let _stray = inheritable(nul.as_handle()).unwrap();
        let (_tmp, dir) = temp_dir();
        let (out, err) = report(&std::env::current_exe().unwrap(), &[], &dir);
        assert!(out.contains("standard 3 other 0"), "{out}");
        assert!(err.contains("stderr"), "{err}");
    }

    #[test]
    fn handles_that_are_not_inheritable_are_an_error() {
        let (_tmp, dir) = temp_dir();
        let program = Program {
            exe: std::env::current_exe().unwrap(),
            args: vec!["--list".into()],
        };
        let nul = || OwnedHandle::from(File::options().read(true).write(true).open(NUL).unwrap());
        let plain = [nul(), nul(), nul()];
        let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        let error = create_process(&program, &dir, &plain, flags).err().unwrap();
        assert_eq!(error.raw_os_error(), Some(ERROR_INVALID_PARAMETER as i32));
        // With one that is, still.
        let one = [inheritable(plain[0].as_handle()).unwrap(), nul(), nul()];
        assert!(create_process(&program, &dir, &one, flags).is_err());
        let all = plain.map(|handle| inheritable(handle.as_handle()).unwrap());
        let mut child = create_process(&program, &dir, &all, flags).unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn arguments_and_directory_arrive_as_they_are() {
        let args: Vec<OsString> = [
            "plain",
            "",
            " ",
            "with space",
            "caf\u{e9} \u{43f}\u{443}\u{442}\u{44c} \u{1f980}",
            r#"""#,
            r#"a"b"#,
            r#"a" b"#,
            r#"\""#,
            r#"\\""#,
            r#"a\\\"b c"#,
            r"\",
            r"ends with\",
            r"ends with\\",
            r"C:\dir\file",
            r"\\.\pipe\branchpod-x-y",
            "%PATH% ^& | > 'x' *?",
            "tab\tnew\nline",
            "--option",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let (_tmp, dir) = temp_dir();
        let cwd = dir.join("caf\u{e9} dir");
        fs::create_dir(&cwd).unwrap();
        let exe = std::env::current_exe().unwrap();
        // Without and with a `\\?\` prefix.
        for given in [cwd.clone(), fs::canonicalize(&cwd).unwrap()] {
            let (out, _) = report(&exe, &args, &given);
            let expected: Vec<String> = args
                .iter()
                .map(|arg| format!("arg {:?}", arg.encode_wide().collect::<Vec<_>>()))
                .collect();
            let got: Vec<&str> = out.lines().filter(|l| l.starts_with("arg ")).collect();
            assert_eq!(got, expected);
            assert!(out.contains(&format!("cwd {cwd:?}")), "{out}");
        }
    }

    #[test]
    fn a_nul_is_an_error() {
        let (_tmp, dir) = temp_dir();
        let program = Program {
            exe: std::env::current_exe().unwrap(),
            args: vec!["a\0b".into()],
        };
        let log = File::create(dir.join("log")).unwrap();
        let error = super::super::spawn_detached(&program, &dir, log).err();
        assert_eq!(error.unwrap().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_long_path_of_the_program_works() {
        let (_tmp, dir) = temp_dir();
        let mut long = dir.clone();
        while long.as_os_str().len() < 300 {
            long.push("caf\u{e9} and some more to make it long");
        }
        fs::create_dir_all(&long).unwrap();
        let exe = long.join("program with space.exe");
        fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let args = ["a b".into()];
        let (out, _) = report(&exe, &args, &dir);
        assert!(out.contains("arg [97, 32, 98]"), "{out}");
    }

    #[test]
    fn long_and_verbatim_paths_take_the_form_the_system_wants() {
        let long = "x".repeat(300);
        let executable = |path: &str| executable(Path::new(path)).into_string().unwrap();
        assert_eq!(executable(r"C:\a\b.exe"), r"C:\a\b.exe");
        let exe = format!(r"C:\{long}\b.exe");
        assert_eq!(executable(&exe), format!(r"\\?\{exe}"));
        assert_eq!(executable(&format!(r"\\?\{exe}")), format!(r"\\?\{exe}"));
        assert_eq!(
            executable(&format!(r"\\server\share\{long}")),
            format!(r"\\?\UNC\server\share\{long}")
        );
        let device = format!(r"\\.\C:\{long}\b.exe");
        assert_eq!(executable(&device), device);
        let directory = |path: &str| working_directory(Path::new(path)).into_string().unwrap();
        assert_eq!(directory(r"C:\a"), r"C:\a");
        assert_eq!(directory(r"\\?\C:\a"), r"C:\a");
        assert_eq!(directory(r"\\?\UNC\server\share\a"), r"\\server\share\a");
        assert_eq!(directory(r"\\server\share\a"), r"\\server\share\a");
    }
}
