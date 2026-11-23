//! Small file-system helpers shared by the state files.

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

/// Writes `bytes` to `dst` atomically: a temporary file next to it, then a rename over it.
///
/// On Windows a rename over a file fails with "access denied" while another process (an
/// antivirus scanner, the search indexer) briefly holds it open; such failures are retried for a
/// short while instead of surfacing as a random error.
pub fn write_atomic(dst: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = Path::new(&tmp);
    fs::write(tmp, bytes)?;

    let mut delay = Duration::from_millis(10);
    let mut attempts = 0;
    loop {
        match fs::rename(tmp, dst) {
            Err(e)
                if e.kind() == io::ErrorKind::PermissionDenied && cfg!(windows) && attempts < 8 =>
            {
                attempts += 1;
                std::thread::sleep(delay);
                delay *= 2;
            }
            result => return result,
        }
    }
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
}
