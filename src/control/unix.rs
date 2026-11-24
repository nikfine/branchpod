//! Control channel over a Unix domain socket.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Unreachable;

pub struct Stream(UnixStream);

impl Stream {
    /// Each read gives up after `limit` without data.
    pub fn deadline(&self, limit: std::time::Duration) -> Result<Deadline> {
        self.0
            .set_read_timeout(Some(limit))
            .context("cannot set a read timeout")?;
        Ok(Deadline)
    }
}

/// Nothing to undo: the timeout belongs to the socket, used for one request.
pub struct Deadline;

impl io::Read for &Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut &self.0, buf)
    }
}

impl io::Write for &Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut &self.0, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut &self.0)
    }
}

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
    let meta =
        fs::symlink_metadata(dir).with_context(|| format!("cannot inspect {}", dir.display()))?;
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
        self.listener.accept().map(|(stream, _)| Stream(stream))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn connect(endpoint: &str) -> Result<Stream> {
    match UnixStream::connect(endpoint) {
        Ok(stream) => Ok(Stream(stream)),
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
