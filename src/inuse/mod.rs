//! Who holds a path open, named in the report when teardown cannot remove a file or directory.
//!
//! Best effort: an empty list means "unknown", not "nobody". On Windows the Restart Manager lists
//! the processes holding given files; on Unix open files do not block removal, so nothing is
//! reported.

use std::fmt;
use std::path::Path;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Holder {
    pub(crate) pid: u32,
    /// Application name as the OS reports it.
    pub(crate) name: String,
}

impl fmt::Display for Holder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (pid {})", self.name, self.pid)
    }
}

/// Processes holding `path` open, or for a directory, files inside it.
pub(crate) fn holders(path: &Path) -> Vec<Holder> {
    sys::holders(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_holds_a_closed_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        assert!(holders(dir.path()).is_empty());
        assert!(holders(&dir.path().join("missing")).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn names_this_process_while_it_holds_a_file_inside() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub").join("open.txt");
        std::fs::create_dir(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "x").unwrap();
        let open = std::fs::File::open(&file).unwrap();
        let found = holders(dir.path());
        drop(open);
        assert!(
            found.iter().any(|h| h.pid == std::process::id()),
            "{found:?}"
        );
    }
}
