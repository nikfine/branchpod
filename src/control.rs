//! The local control channel of a supervisor: a Unix domain socket, a named pipe on Windows.
//!
//! One connection per request: the client writes one line, the server answers with one line and
//! closes the connection. The server handles connections one at a time.
//!
//! - Unix: the socket lives in a directory only the user can enter (`$XDG_RUNTIME_DIR/branchpod`,
//!   otherwise `<temp>/branchpod-<uid>`), not in the pod directory: socket paths are limited to
//!   about 104 bytes, which a data directory such as `~/Library/Application Support/...` exceeds.
//! - Windows: `\\.\pipe\branchpod-<pod>-<nonce>`, created with `FILE_FLAG_FIRST_PIPE_INSTANCE`
//!   (nobody else can have created that name first) and rejecting remote clients. A spare pipe
//!   instance is always open while the server lives, so a client never mistakes the moment between
//!   two connections for a server that is gone.
//!
//! Endpoints carry a random part, so a stale endpoint of an earlier supervisor is never reused.

use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// Longest line accepted, so that a broken peer cannot make us read without end.
const MAX_LINE: u64 = 1 << 20;

/// Nobody listens on the endpoint: the supervisor is gone (or its endpoint was removed).
#[derive(Debug)]
pub struct Unreachable(pub String);

impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "nothing listens on {}", self.0)
    }
}

impl std::error::Error for Unreachable {}

/// A fresh endpoint for a supervisor of pod `pod`.
pub fn new_endpoint(pod: &str) -> Result<String> {
    sys::new_endpoint(pod, &nonce(pod))
}

/// 16 hex digits that differ between supervisors of the same pod.
fn nonce(pod: &str) -> String {
    // The clock alone may not advance between two calls (100 ns steps on Windows).
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let digest = Sha256::digest(format!("{pod}\0{}\0{nanos}\0{count}", std::process::id()));
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Server(sys::Server);

impl Server {
    pub fn bind(endpoint: &str) -> Result<Server> {
        sys::Server::bind(endpoint)
            .map(Server)
            .with_context(|| format!("cannot listen on {endpoint}"))
    }

    /// Waits for the next client.
    pub fn accept(&mut self) -> Result<Conn> {
        self.0
            .accept()
            .map(Conn)
            .context("cannot accept a connection")
    }
}

/// One connection, for one request and its answer.
pub struct Conn(sys::Stream);

impl Conn {
    pub fn read_line(&self) -> Result<String> {
        let mut line = String::new();
        BufReader::new((&self.0).take(MAX_LINE))
            .read_line(&mut line)
            .context("cannot read from the control channel")?;
        if !line.ends_with('\n') {
            bail!("the control channel closed before a complete line");
        }
        line.pop();
        Ok(line)
    }

    pub fn write_line(&self, line: &str) -> Result<()> {
        let mut out = line.as_bytes().to_vec();
        out.push(b'\n');
        (&self.0)
            .write_all(&out)
            .and_then(|()| (&self.0).flush())
            .context("cannot write to the control channel")
    }
}

/// Connects to `endpoint`. Fails with [`Unreachable`] when nothing listens there.
pub fn connect(endpoint: &str) -> Result<Conn> {
    sys::connect(endpoint).map(Conn)
}

/// Removes what a server that died without cleaning up left at `endpoint` (a socket file on
/// Unix; a pipe disappears with its last handle). Call only once the server is known to be gone.
pub fn remove_stale(endpoint: &str) -> Result<()> {
    sys::remove_stale(endpoint)
}

#[cfg(unix)]
mod sys {
    use std::fs;
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, bail};

    use super::Unreachable;

    pub type Stream = UnixStream;

    pub fn new_endpoint(_pod: &str, nonce: &str) -> Result<String> {
        let dir = runtime_dir()?;
        let path = dir.join(format!("{nonce}.sock"));
        let Some(text) = path.to_str() else {
            bail!("socket directory {} is not valid UTF-8", dir.display());
        };
        Ok(text.to_owned())
    }

    /// A directory only we can enter, created if needed.
    fn runtime_dir() -> Result<PathBuf> {
        // SAFETY: plain system call without arguments.
        let uid = unsafe { libc::getuid() };
        let dir = match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
            Some(base) if base.is_absolute() => base.join("branchpod"),
            _ => std::env::temp_dir().join(format!("branchpod-{uid}")),
        };
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", dir.display())),
        }
        check_private(&dir, uid)?;
        Ok(dir)
    }

    /// A shared temp directory: someone else may have created the directory first.
    fn check_private(dir: &Path, uid: libc::uid_t) -> Result<()> {
        let meta = fs::symlink_metadata(dir)
            .with_context(|| format!("cannot inspect {}", dir.display()))?;
        if !meta.is_dir() || meta.uid() != uid || meta.permissions().mode() & 0o077 != 0 {
            bail!(
                "{} must be a directory owned by the current user and closed to others",
                dir.display()
            );
        }
        Ok(())
    }

    pub struct Server {
        listener: UnixListener,
        path: PathBuf,
    }

    impl Server {
        pub fn bind(endpoint: &str) -> io::Result<Server> {
            let path = PathBuf::from(endpoint);
            Ok(Server {
                listener: UnixListener::bind(&path)?,
                path,
            })
        }

        pub fn accept(&mut self) -> io::Result<Stream> {
            self.listener.accept().map(|(stream, _)| stream)
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    pub fn connect(endpoint: &str) -> Result<Stream> {
        match UnixStream::connect(endpoint) {
            Ok(stream) => Ok(stream),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                Err(Unreachable(endpoint.to_owned()).into())
            }
            Err(e) => Err(e).with_context(|| format!("cannot connect to {endpoint}")),
        }
    }

    pub fn remove_stale(endpoint: &str) -> Result<()> {
        use std::os::unix::fs::FileTypeExt;
        match fs::symlink_metadata(endpoint) {
            Ok(meta) if meta.file_type().is_socket() => {
                fs::remove_file(endpoint).with_context(|| format!("cannot remove {endpoint}"))
            }
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("cannot inspect {endpoint}")),
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::ptr;

    use anyhow::{Context, Result};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
        GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, OPEN_EXISTING,
        PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, NMPWAIT_WAIT_FOREVER, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        WaitNamedPipeW,
    };

    use super::Unreachable;

    const BUFFER: u32 = 64 * 1024;

    /// A connected pipe end. The server's end waits for the client to read everything before it
    /// is closed: closing at once could discard an answer not read yet.
    pub struct Stream {
        file: File,
        server: bool,
    }

    impl io::Read for &Stream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            (&self.file).read(buf)
        }
    }

    impl io::Write for &Stream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            (&self.file).write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            (&self.file).flush()
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

    pub fn new_endpoint(pod: &str, nonce: &str) -> Result<String> {
        Ok(format!(r"\\.\pipe\branchpod-{pod}-{nonce}"))
    }

    pub fn remove_stale(_endpoint: &str) -> Result<()> {
        Ok(())
    }

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    pub struct Server {
        name: Vec<u16>,
        /// The instance the next client connects to.
        next: HANDLE,
    }

    impl Server {
        pub fn bind(endpoint: &str) -> io::Result<Server> {
            let name = wide(endpoint);
            let next = create_instance(&name, true)?;
            Ok(Server { name, next })
        }

        pub fn accept(&mut self) -> io::Result<Stream> {
            // SAFETY: `next` is a valid pipe instance; synchronous call.
            if unsafe { ConnectNamedPipe(self.next, ptr::null_mut()) } == 0 {
                let e = io::Error::last_os_error();
                // The client connected between creation and this call: fine.
                if e.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                    return Err(e);
                }
            }
            // Open the spare instance before handing this one out.
            let spare = create_instance(&self.name, false)?;
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

    fn create_instance(name: &[u16], first: bool) -> io::Result<HANDLE> {
        let open_mode = PIPE_ACCESS_DUPLEX
            | if first {
                FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                0
            };
        // SAFETY: `name` is NUL-terminated; default security (full control for the owner).
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER,
                BUFFER,
                0,
                ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(handle)
    }

    pub fn connect(endpoint: &str) -> Result<Stream> {
        let name = wide(endpoint);
        loop {
            // SAFETY: `name` is NUL-terminated.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    0,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_request_and_answer_per_connection() {
        let endpoint = new_endpoint("test-pod").unwrap();
        let mut server = Server::bind(&endpoint).unwrap();
        let client = std::thread::spawn({
            let endpoint = endpoint.clone();
            move || {
                let mut answers = Vec::new();
                for word in ["ping", "pong", "stop"] {
                    let conn = connect(&endpoint).unwrap();
                    conn.write_line(word).unwrap();
                    answers.push(conn.read_line().unwrap());
                }
                answers
            }
        });
        loop {
            let conn = server.accept().unwrap();
            let line = conn.read_line().unwrap();
            conn.write_line(&format!("got {line}")).unwrap();
            if line == "stop" {
                break;
            }
        }
        assert_eq!(client.join().unwrap(), ["got ping", "got pong", "got stop"]);
        drop(server);
        let err = connect(&endpoint).err().expect("nothing listens any more");
        assert!(err.downcast_ref::<Unreachable>().is_some(), "{err:#}");
    }

    #[test]
    fn a_second_server_cannot_take_the_endpoint() {
        let endpoint = new_endpoint("test-pod").unwrap();
        let _server = Server::bind(&endpoint).unwrap();
        assert!(Server::bind(&endpoint).is_err());
    }

    #[test]
    fn endpoints_differ() {
        assert_ne!(new_endpoint("p").unwrap(), new_endpoint("p").unwrap());
    }
}
