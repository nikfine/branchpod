//! The mark of a pod: a file in the git directory of its worktree that goes with the worktree, so
//! that a new one at the same path is not taken for it.
//!
//! The only thing `bp` writes there. It is recorded in `pod.json`, with its path, before it is
//! written, so whatever is left of it is found also when the worktree is gone or another one.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::worktree::Worktree;
use crate::{control, fsutil, saved};

/// Git looks for refs in this directory, too: a name with a leading dot cannot be one.
const NAME: &str = ".branchpod";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Mark {
    path: PathBuf,
    id: String,
}

impl Mark {
    /// A new mark for the worktree of pod `pod`. Nothing is written yet.
    pub(crate) fn new(pod: &str, wt: &Worktree) -> Result<Mark> {
        Ok(Mark {
            path: wt.git_dir()?.join(NAME),
            id: control::nonce(pod),
        })
    }

    /// Replaces any other mark, as that of a pod whose state was lost.
    pub(crate) fn write(&self) -> Result<()> {
        fsutil::write_atomic(&self.path, format!("{}\n", self.id).as_bytes())
            .with_context(|| format!("cannot write {}", self.path.display()))
    }

    /// Whether `wt` has this mark.
    pub(crate) fn is_on(&self, wt: &Worktree) -> Result<bool> {
        self.is_in(&wt.git_dir()?.join(NAME))
    }

    /// Removes the mark; nothing to do if it is gone, or another one is in its place.
    pub(crate) fn remove(&self) -> Result<()> {
        // Of a write that was interrupted, maybe halfway.
        let tmp = self.path.with_file_name(format!("{NAME}.tmp"));
        if fs::read(&tmp).is_ok_and(|text| self.id.as_bytes().starts_with(text.trim_ascii())) {
            let _ = fsutil::remove_file_if_exists(&tmp);
        }
        if self.is_in(&self.path)? {
            fsutil::remove_file_if_exists(&self.path)
                .with_context(|| format!("cannot remove {}", self.path.display()))?;
        }
        Ok(())
    }

    fn is_in(&self, path: &Path) -> Result<bool> {
        match fsutil::retry(|| fs::read(path)) {
            Ok(text) => Ok(text.trim_ascii() == self.id.as_bytes()),
            Err(e) if saved::is_absent(&e) => Ok(false),
            Err(e) if e.kind() == io::ErrorKind::IsADirectory => Ok(false),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_own_mark_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(NAME);
        let mark = |id: &str| Mark {
            path: path.clone(),
            id: id.into(),
        };
        let (one, two) = (mark("1"), mark("2"));
        one.remove().unwrap();
        two.write().unwrap();
        one.remove().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "2\n");
        // Replaced, and no temporary file is left.
        one.write().unwrap();
        assert!(one.is_in(&path).unwrap() && !two.is_in(&path).unwrap());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        // Writing it again was interrupted; the other one is somebody else's.
        let tmp = dir.path().join(".branchpod.tmp");
        for (text, left) in [("", 0), ("1", 0), ("2\n", 1)] {
            one.write().unwrap();
            fs::write(&tmp, text).unwrap();
            one.remove().unwrap();
            assert!(!path.exists());
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), left, "{text}");
        }

        // The git directory is gone.
        let gone = Mark {
            path: dir.path().join("gone").join(NAME),
            id: "1".into(),
        };
        gone.remove().unwrap();
    }
}
