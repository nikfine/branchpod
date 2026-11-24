//! Copies that were edited in the worktree: teardown moves them here instead of deleting them.
//!
//! `saved/<pod id>/<time of the teardown>/<path in the worktree>` in the `bp` home, outside the pod's
//! state, which goes with the pod. Nothing here is ever overwritten or removed by teardown, except
//! for an unfinished copy of its own.

use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::{fsutil, inuse};

/// Where one run of teardown keeps the edited copies of a pod.
pub(crate) struct Saved {
    home: PathBuf,
    /// `<home>/saved/<pod id>`.
    pod: PathBuf,
    /// Name of this run's directory.
    run: String,
}

impl Saved {
    pub(crate) fn new(home: &Path, pod: &str) -> Saved {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Saved {
            home: home.to_path_buf(),
            pod: home.join("saved").join(pod),
            run: millis.to_string(),
        }
    }

    /// A path for the file at `relative` in the worktree, where nothing exists yet.
    pub(crate) fn choose(&self, relative: &Path) -> Result<PathBuf> {
        if !is_plain(relative) {
            bail!("not a path inside the worktree: {}", relative.display());
        }
        let mut run = self.run.clone();
        for n in 1.. {
            let target = self.pod.join(&run).join(relative);
            match fs::symlink_metadata(&target) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(target),
                Ok(_) => run = format!("{}-{n}", self.run),
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot read {}", target.display()));
                }
            }
        }
        unreachable!("the numbers do not run out")
    }

    /// Moves the file `from` to `target`, a path from [`Saved::choose`]. After a failure `from` is
    /// still in place.
    pub(crate) fn take(&self, from: &Path, target: &Path) -> Result<()> {
        self.take_with(from, target, |from, to| fs::rename(from, to), copy)
    }

    fn take_with(
        &self,
        from: &Path,
        target: &Path,
        rename: impl Fn(&Path, &Path) -> io::Result<()>,
        copy: impl Fn(&Path, &Path) -> io::Result<()>,
    ) -> Result<()> {
        self.create_parents(target)?;
        match fsutil::retry(|| rename(from, target)) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {}
            Err(e) => return Err(inuse::error(from, e)),
        }

        // Another volume: a complete copy on disk first, only then the original goes.
        let before = Stamp::of(&fs::symlink_metadata(from)?);
        let part = part(target);
        fsutil::remove_file_if_exists(&part)?;
        let copied = copy(from, &part).and_then(|()| fsutil::retry(|| fs::rename(&part, target)));
        if let Err(e) = copied {
            let _ = fsutil::remove_file_if_exists(&part);
            return Err(e).with_context(|| format!("cannot copy it to {}", target.display()));
        }
        fsutil::sync_parent(target)?;
        let removed = remove_unchanged(from, &before)
            .map_err(|e| inuse::error(from, e))
            .with_context(|| format!("copied to {}, but", target.display()))?;
        if !removed {
            bail!(
                "left in place: it was changed while it was copied to {}",
                target.display()
            );
        }
        Ok(())
    }

    /// Whether `target`, which comes from the ledger, is a path [`Saved::choose`] could have given.
    pub(crate) fn contains(&self, target: &Path) -> bool {
        target.strip_prefix(&self.pod).is_ok_and(is_plain)
    }

    /// Removes what an interrupted [`Saved::take`] to `target` left unfinished.
    pub(crate) fn discard_unfinished(&self, target: &Path) -> Result<()> {
        if !self.contains(target) {
            return Ok(());
        }
        let part = part(target);
        fsutil::remove_file_if_exists(&part)
            .with_context(|| format!("cannot remove {}", part.display()))
    }

    /// The directories above `target`, each closed to other users (edited copies may hold secrets).
    fn create_parents(&self, target: &Path) -> Result<()> {
        let relative = target
            .strip_prefix(&self.home)
            .with_context(|| format!("{} is outside the bp home", target.display()))?;
        fs::create_dir_all(&self.home)
            .with_context(|| format!("cannot create {}", self.home.display()))?;
        let mut dir = self.home.clone();
        for component in relative.parent().into_iter().flat_map(Path::components) {
            dir.push(component);
            fsutil::create_private_dir(&dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        Ok(())
    }
}

/// Whether `path` consists of names only: not empty, relative, and no `..`.
fn is_plain(path: &Path) -> bool {
    path.components().next().is_some()
        && path.components().all(|c| matches!(c, Component::Normal(_)))
}

/// The name under which a copy to `target` is written until it is complete.
fn part(target: &Path) -> PathBuf {
    let mut name = target.file_name().map(OsString::from).unwrap_or_default();
    name.push(".bp-part");
    target.with_file_name(name)
}

/// Copies `from` to the new file `to`, on disk when it returns, with the permissions of `from`.
fn copy(from: &Path, to: &Path) -> io::Result<()> {
    let mut source = File::open(from)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    fsutil::owner_only(&mut options);
    let mut file = options.open(to)?;
    io::copy(&mut source, &mut file)?;
    file.set_permissions(source.metadata()?.permissions())?;
    file.sync_all()
}

/// Tells, without reading it, whether a file was written to or replaced between two looks.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    is_file: bool,
    size: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    pub(crate) fn of(meta: &Metadata) -> Stamp {
        Stamp {
            is_file: meta.file_type().is_file(),
            size: meta.len(),
            modified: meta.modified().ok(),
        }
    }
}

/// Removes the file `path`, but not if it is another one than when `stamp` was taken: then the
/// result is `false`. Looked at again before every attempt, as the program that holds the file may be
/// writing to it. What is left is the moment between the look and the removal. A file that is gone
/// counts as removed.
pub(crate) fn remove_unchanged(path: &Path, stamp: &Stamp) -> io::Result<bool> {
    let removed = fsutil::retry(|| {
        if Stamp::of(&fs::symlink_metadata(path)?) != *stamp {
            return Ok(false);
        }
        fs::remove_file(path).map(|()| true)
    });
    match removed {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Setup {
        _tmp: tempfile::TempDir,
        saved: Saved,
        file: PathBuf,
    }

    fn setup() -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("caf\u{e9}.env");
        fs::write(&file, "edited").unwrap();
        Setup {
            saved: Saved::new(&tmp.path().join("home"), "wt-00000000"),
            file,
            _tmp: tmp,
        }
    }

    fn text(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn other_volume(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::ErrorKind::CrossesDevices.into())
    }

    #[test]
    fn moves_into_private_directories_and_never_onto_something() {
        let s = setup();
        let relative = Path::new("conf").join("na\u{ef}ve").join(".env");
        let target = s.saved.choose(&relative).unwrap();
        s.saved.take(&s.file, &target).unwrap();
        assert!(!s.file.exists());
        assert_eq!(text(&target), "edited");
        // Up to `saved` itself.
        #[cfg(unix)]
        for dir in target.ancestors().skip(1).take(5) {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", dir.display());
        }

        let next = s.saved.choose(&relative).unwrap();
        assert_ne!(next, target);
        fs::write(&s.file, "again").unwrap();
        s.saved.take(&s.file, &next).unwrap();
        assert_eq!(text(&target), "edited");
        assert_eq!(text(&next), "again");

        assert!(s.saved.choose(Path::new("../.env")).is_err());
        assert!(s.saved.choose(Path::new("")).is_err());
    }

    #[test]
    fn another_volume_gets_a_copy_before_the_original_goes() {
        let s = setup();
        let target = s.saved.choose(Path::new(".env")).unwrap();
        // An earlier run was interrupted while it copied.
        s.saved.create_parents(&target).unwrap();
        fs::write(part(&target), "edi").unwrap();
        let mut permissions = fs::metadata(&s.file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&s.file, permissions.clone()).unwrap();

        s.saved
            .take_with(&s.file, &target, other_volume, copy)
            .unwrap();
        assert!(!s.file.exists());
        assert_eq!(text(&target), "edited");
        assert_eq!(fs::metadata(&target).unwrap().permissions(), permissions);
        let dir = target.parent().unwrap();
        assert_eq!(fs::read_dir(dir).unwrap().count(), 1, "nothing unfinished");
    }

    #[test]
    fn a_failed_copy_leaves_the_original_and_nothing_else() {
        let s = setup();
        let target = s.saved.choose(Path::new(".env")).unwrap();
        let full = |_: &Path, to: &Path| {
            fs::write(to, "edi")?;
            Err(io::ErrorKind::StorageFull.into())
        };
        let err = s
            .saved
            .take_with(&s.file, &target, other_volume, full)
            .unwrap_err();
        assert!(format!("{err:#}").contains("cannot copy"), "{err:#}");
        assert_eq!(text(&s.file), "edited");
        let dir = target.parent().unwrap();
        assert_eq!(fs::read_dir(dir).unwrap().count(), 0);
    }

    #[test]
    fn a_file_changed_while_it_is_copied_stays() {
        let s = setup();
        let target = s.saved.choose(Path::new(".env")).unwrap();
        let racing = |from: &Path, to: &Path| {
            copy(from, to)?;
            fs::write(from, "edited once more")
        };
        let err = s
            .saved
            .take_with(&s.file, &target, other_volume, racing)
            .unwrap_err();
        assert!(format!("{err:#}").contains("left in place"), "{err:#}");
        assert_eq!(text(&s.file), "edited once more");
    }

    #[test]
    fn a_file_changed_since_it_was_looked_at_is_not_removed() {
        let s = setup();
        let stamp = || Stamp::of(&fs::symlink_metadata(&s.file).unwrap());
        let before = stamp();
        fs::write(&s.file, "edited once more").unwrap();
        assert!(!remove_unchanged(&s.file, &before).unwrap());
        assert_eq!(text(&s.file), "edited once more");
        let now = stamp();
        assert!(remove_unchanged(&s.file, &now).unwrap());
        assert!(!s.file.exists());
        // Already gone.
        assert!(remove_unchanged(&s.file, &now).unwrap());
    }

    /// A program writes to the file and closes it while the removal is being retried.
    #[cfg(windows)]
    #[test]
    fn a_file_written_to_by_whoever_holds_it_is_not_removed() {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        let s = setup();
        let stamp = Stamp::of(&fs::symlink_metadata(&s.file).unwrap());
        let mut open = OpenOptions::new()
            .append(true)
            .share_mode(FILE_SHARE_READ)
            .open(&s.file)
            .unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            open.write_all(b" once more").unwrap();
        });
        assert!(!remove_unchanged(&s.file, &stamp).unwrap());
        writer.join().unwrap();
        assert_eq!(text(&s.file), "edited once more");
    }

    #[test]
    fn only_names_of_our_own_are_discarded() {
        let s = setup();
        let foreign = s.file.with_file_name("x.bp-part");
        fs::write(&foreign, "not ours").unwrap();
        let outside = s.saved.pod.join("..").join("..").join("..").join("x");
        for recorded in [s.file.with_file_name("x"), outside] {
            s.saved.discard_unfinished(&recorded).unwrap();
        }
        assert!(foreign.exists());

        let target = s.saved.choose(Path::new(".env")).unwrap();
        s.saved.create_parents(&target).unwrap();
        fs::write(part(&target), "edi").unwrap();
        s.saved.discard_unfinished(&target).unwrap();
        assert!(!part(&target).exists());
        // Already gone, its directory too.
        fs::remove_dir_all(&s.saved.pod).unwrap();
        s.saved.discard_unfinished(&target).unwrap();
    }
}
