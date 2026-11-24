//! Control channel over a named pipe.

use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::ptr;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, NMPWAIT_WAIT_FOREVER,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::Unreachable;

const BUFFER: u32 = 64 * 1024;

/// A connected pipe end. The server's end waits for the client to read everything before it
/// is closed: closing at once could discard an answer not read yet.
pub(crate) struct Stream {
    file: File,
    server: bool,
}

impl Stream {
    /// Cancels a read still waiting after `limit`; the read then fails. Synchronous pipe
    /// reads have no timeout of their own, so a watchdog thread cancels it.
    pub(crate) fn deadline(&self, limit: Duration) -> Result<Deadline> {
        let (done, wait) = mpsc::channel::<()>();
        let handle = self.file.as_raw_handle() as usize;
        let watchdog = std::thread::spawn(move || {
            if wait.recv_timeout(limit) == Err(mpsc::RecvTimeoutError::Timeout) {
                // SAFETY: the handle stays open until `Deadline` has joined this thread.
                unsafe { CancelIoEx(handle as HANDLE, ptr::null()) };
            }
        });
        Ok(Deadline {
            done: Some(done),
            watchdog: Some(watchdog),
        })
    }
}

/// Stops the watchdog when dropped, before the stream can be closed.
pub(crate) struct Deadline {
    done: Option<mpsc::Sender<()>>,
    watchdog: Option<JoinHandle<()>>,
}

impl Drop for Deadline {
    fn drop(&mut self) {
        drop(self.done.take());
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}

impl io::Read for &Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut &self.file, buf)
    }
}

impl io::Write for &Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut &self.file, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut &self.file)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if self.server {
            // SAFETY: a valid pipe handle owned by `file`.
            unsafe { FlushFileBuffers(self.file.as_raw_handle() as HANDLE) };
        }
    }
}

pub(crate) fn new_endpoint(pod: &str, nonce: &str) -> Result<String> {
    Ok(format!(r"\\.\pipe\branchpod-{pod}-{nonce}"))
}

pub(crate) fn remove_stale(_endpoint: &str) -> Result<()> {
    Ok(())
}

fn wide(text: &str) -> Vec<u16> {
    std::ffi::OsStr::new(text)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

/// A security descriptor granting access to the current user and SYSTEM only. The default
/// one of a named pipe also lets everyone, anonymous included, open it for reading.
struct Security(PSECURITY_DESCRIPTOR);

impl Security {
    fn current_user_only() -> io::Result<Security> {
        let sid = current_user_sid()?;
        let sddl = wide(&format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)"));
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated; the descriptor is freed with LocalFree on drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Security(descriptor))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

/// The SID of the user this process runs as, in string form (`S-1-5-21-...`).
fn current_user_sid() -> io::Result<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo handle of our own process needs no closing; the token is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        // TOKEN_USER plus the SID it points to; generously sized, the call says if not.
        let mut buffer = vec![0u64; 64];
        let mut needed = 0u32;
        let size = (buffer.len() * 8) as u32;
        // SAFETY: the buffer is writable for `size` bytes and 8-byte aligned.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                size,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: filled in by GetTokenInformation as a TOKEN_USER.
        let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
        let mut text: *mut u16 = ptr::null_mut();
        // SAFETY: a valid SID; the string is freed with LocalFree below.
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a NUL-terminated string from ConvertSidToStringSidW.
        let sid = unsafe {
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            sid
        };
        Ok(sid)
    })();
    // SAFETY: we own the token handle.
    unsafe { CloseHandle(token) };
    result
}

pub(crate) struct Server {
    name: Vec<u16>,
    security: Security,
    /// The instance the next client connects to.
    next: HANDLE,
}

impl Server {
    pub(crate) fn bind(endpoint: &str) -> io::Result<Server> {
        let name = wide(endpoint);
        let security = Security::current_user_only()?;
        let next = create_instance(&name, &security, true)?;
        Ok(Server {
            name,
            security,
            next,
        })
    }

    pub(crate) fn accept(&mut self) -> io::Result<Stream> {
        loop {
            // SAFETY: `next` is a valid pipe instance; synchronous call.
            if unsafe { ConnectNamedPipe(self.next, ptr::null_mut()) } != 0 {
                break;
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                // The client connected between creation and this call: fine.
                Some(code) if code == ERROR_PIPE_CONNECTED as i32 => break,
                // The client connected and is already gone: reset the instance and wait
                // for the next one, or every later call fails the same way.
                Some(code) if code == ERROR_NO_DATA as i32 => {
                    // SAFETY: a valid pipe instance.
                    unsafe { DisconnectNamedPipe(self.next) };
                }
                _ => return Err(e),
            }
        }
        // Open the spare instance before handing this one out.
        let spare = create_instance(&self.name, &self.security, false)?;
        let connected = std::mem::replace(&mut self.next, spare);
        // SAFETY: we own `connected`; `File` takes over closing it.
        let file = unsafe { File::from_raw_handle(connected as _) };
        Ok(Stream { file, server: true })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // SAFETY: we own the spare instance.
        unsafe { CloseHandle(self.next) };
    }
}

fn create_instance(name: &[u16], security: &Security, first: bool) -> io::Result<HANDLE> {
    let open_mode = PIPE_ACCESS_DUPLEX
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    let attributes = security.attributes();
    // SAFETY: `name` is NUL-terminated; `attributes` outlives the call.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER,
            BUFFER,
            0,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

pub(crate) fn connect(endpoint: &str) -> Result<Stream> {
    let name = wide(endpoint);
    loop {
        // Identification level only: a server that took the name of a dead supervisor cannot
        // act as us.
        // SAFETY: `name` is NUL-terminated.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: we own the handle; `File` takes over closing it.
            let file = unsafe { File::from_raw_handle(handle as _) };
            return Ok(Stream {
                file,
                server: false,
            });
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(code) if code == ERROR_FILE_NOT_FOUND as i32 => {
                return Err(Unreachable(endpoint.to_owned()).into());
            }
            Some(code) if code == ERROR_PIPE_BUSY as i32 => {
                // Every instance is taken while the server handles a request: wait for the
                // next one. Fails once the pipe is gone.
                // SAFETY: `name` is NUL-terminated.
                if unsafe { WaitNamedPipeW(name.as_ptr(), NMPWAIT_WAIT_FOREVER) } == 0 {
                    let e = io::Error::last_os_error();
                    if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
                        return Err(Unreachable(endpoint.to_owned()).into());
                    }
                }
            }
            _ => return Err(e).with_context(|| format!("cannot connect to {endpoint}")),
        }
    }
}
