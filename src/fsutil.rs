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
pub fn write_atomic(dst: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = Path::new(&tmp);

    let mut file = File::create(tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    retry(|| fs::rename(tmp, dst))?;

    #[cfg(unix)]
    if let Some(dir) = dst.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Runs `op`, retrying with backoff (about 2.5 s in all) while it fails with an error that
/// [`is_transient`] attributes to another process briefly holding a file open.
pub fn retry<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
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
pub fn is_transient(e: &io::Error) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    matches!(
        e.raw_os_error(),
        Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
    )
}

#[cfg(not(windows))]
pub fn is_transient(_: &io::Error) -> bool {
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

    #[cfg(windows)]
    #[test]
    fn sharing_violation_is_transient() {
        assert!(is_transient(&io::Error::from_raw_os_error(32)));
        assert!(!is_transient(&io::Error::from_raw_os_error(2)));
    }
}
