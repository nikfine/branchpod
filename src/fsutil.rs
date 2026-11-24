//! Small file-system helpers shared by the state files and teardown.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

/// Writes `bytes` to `dst` atomically and durably: a temporary file next to it, flushed to disk,
/// then renamed over it; on Unix the directory is flushed as well, so the rename survives a crash.
///
/// On Windows the rename fails transiently while another process (an antivirus scanner, the
/// search indexer) briefly holds the destination or the freshly written temporary file open;
/// such failures are retried for a short while instead of surfacing as a random error.
///
/// After a failure the temporary file is removed as far as possible.
pub(crate) fn write_atomic(dst: &Path, bytes: &[u8]) -> io::Result<()> {
    write_through(dst, bytes, |tmp| File::create(tmp))
}

/// Like [`write_atomic`], for contents only the user may read: on Unix the temporary file, and so
/// `dst`, has mode 0600 from the moment it is created. On Windows both inherit the permissions of
/// their directory.
pub(crate) fn write_atomic_private(dst: &Path, bytes: &[u8]) -> io::Result<()> {
    write_through(dst, bytes, |tmp| {
        // Created anew: a mode has no effect on a file left behind by an interrupted write.
        remove_file_if_exists(tmp)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(tmp)
    })
}

fn write_through(
    dst: &Path,
    bytes: &[u8],
    create: impl FnOnce(&Path) -> io::Result<File>,
) -> io::Result<()> {
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = Path::new(&tmp);

    let mut file = create(tmp)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = written.and_then(|()| retry(|| fs::rename(tmp, dst))) {
        let _ = remove_file_if_exists(tmp);
        return Err(e);
    }

    #[cfg(unix)]
    if let Some(dir) = dst.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Creates the directory `dir` if needed (not its parents) and, on Unix, closes it to everyone but
/// the user (mode 0700). On Windows it inherits the permissions of its parent. A link is an error,
/// also one to a directory.
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(dir) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(dir)?.is_dir() {
                return Err(e);
            }
        }
        result => result?,
    }
    // Also when it already existed.
    #[cfg(unix)]
    fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    Ok(())
}

/// Removes the file `path`; nothing to do if there is none.
pub(crate) fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match retry(|| fs::remove_file(path)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Runs `op`, retrying with backoff (about 2.5 s in all) while it fails with an error that
/// [`is_transient`] attributes to another process briefly holding a file open.
pub(crate) fn retry<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut delay = Duration::from_millis(10);
    let mut attempts = 0;
    loop {
        match op() {
            Err(e) if is_transient(&e) && attempts < 8 => {
                attempts += 1;
                std::thread::sleep(delay);
                delay *= 2;
            }
            result => return result,
        }
    }
}

/// Whether `e` is what Windows reports while another process holds the file open (or, for a
/// directory, a file inside it or its working directory there). A scanner or indexer does so
/// for a moment; an editor or a dev server until it is closed. Never true on Unix, where open
/// files do not block renaming or removal.
#[cfg(windows)]
pub(crate) fn is_transient(e: &io::Error) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    matches!(
        e.raw_os_error(),
        Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
    )
}

#[cfg(not(windows))]
pub(crate) fn is_transient(_: &io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert!(!dir.path().join("state.json.tmp").exists());
    }

    #[test]
    fn private_files_replace_whatever_an_interrupted_write_left() {
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("outputs");
        fs::create_dir(&private).unwrap();
        let path = private.join("env");
        // Readable by others, as an older or foreign file might be.
        fs::write(private.join("env.tmp"), "stale").unwrap();
        fs::write(&path, "stale").unwrap();

        create_private_dir(&private).unwrap();
        write_atomic_private(&path, b"secret").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"secret");
        assert_eq!(fs::read_dir(&private).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!((mode(&private), mode(&path)), (0o700, 0o600));
        }

        // A file in place of the directory is not one.
        assert!(create_private_dir(&path).is_err());

        remove_file_if_exists(&path).unwrap();
        remove_file_if_exists(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_write_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        // A directory with something in it cannot be replaced by a file.
        let path = dir.path().join("env");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("x"), "").unwrap();
        assert!(write_atomic_private(&path, b"secret").is_err());
        assert!(path.join("x").exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_link_is_not_a_private_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("outputs");
        fs::create_dir(&target).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
        }
        #[cfg(windows)]
        junction::create(&target, &link).unwrap();
        assert!(create_private_dir(&link).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "the target is left alone");
        }
    }

    #[cfg(windows)]
    #[test]
    fn sharing_violation_is_transient() {
        assert!(is_transient(&io::Error::from_raw_os_error(32)));
        assert!(!is_transient(&io::Error::from_raw_os_error(2)));
    }
}
