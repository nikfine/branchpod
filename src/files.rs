//! File resources: git-ignored files carried over from the main checkout into a worktree.
//!
//! Creation is split in two so that the write-ahead ledger can record what is about to be
//! created: [`plan`] checks everything that can fail without touching the disk (the source
//! exists, the destination is free, a template renders) and yields a [`Record`]; [`create`] then
//! makes it. The record alone is enough to undo a creation, even an interrupted one.
//!
//! Teardown ([`remove`]) deletes exactly what a record describes, and only while it still is what
//! was created: a link is removed as a link and never followed, a path that has changed type is
//! left in place and reported, and nothing is reached through a link. `bp down` removes every
//! link before any directory, so no recursive delete can reach into the main checkout through one.
//! A single file whose content is not what was created any more is kept: see [`crate::saved`].

use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{FileMode, FileSpec};
use crate::saved::{self, Saved, Stamp};
use crate::vars::Vars;
use crate::{fsutil, inuse};

/// What a file resource is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EntryType {
    File,
    Dir,
    /// Symlink, or a junction for a directory on Windows.
    Link,
}

/// Ledger data of a file resource: everything teardown needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// Worktree root. Nothing outside it is ever created or removed.
    pub(crate) root: PathBuf,
    /// Absolute destination.
    pub(crate) path: PathBuf,
    #[serde(rename = "type")]
    pub(crate) entry: EntryType,
    /// Directories created to hold `path`, outermost first; removed on teardown while empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) created_dirs: Vec<PathBuf>,
    /// Reflink mode only: whether every file was cloned copy-on-write rather than copied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cloned: Option<bool>,
    /// Directories only: the trash name teardown moves `path` to before deleting it. Chosen and
    /// recorded before the move, so an interruption at any point is finished from the ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) trash: Option<PathBuf>,
    /// Single files only: what tells an edited one. Without it (an interrupted copy, a record of an
    /// older `bp`) the file is removed whatever its content. Not shown by `bp status --json`, see
    /// [`crate::ledger::PRIVATE_DATA`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) fingerprint: Option<Fingerprint>,
    /// Where teardown keeps the edited file, recorded before it is moved there; an attempt that was
    /// interrupted or failed may have left a path unused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) saved: Vec<PathBuf>,
}

/// What a file's content was when it was created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Fingerprint {
    size: u64,
    #[serde(flatten)]
    content: Content,
}

/// Permissions, attributes and, with a hash, the modification time are not content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Content {
    Sha256(String),
    /// Of a copy-on-write clone, as nanoseconds since the epoch: reading all of it would cost what
    /// cloning saved.
    Modified(i64),
}

impl Fingerprint {
    fn of_bytes(bytes: &[u8]) -> Fingerprint {
        Fingerprint::hashed(bytes.len() as u64, &Sha256::digest(bytes))
    }

    fn hashed(size: u64, digest: &[u8]) -> Fingerprint {
        let hex = digest.iter().map(|b| format!("{b:02x}")).collect();
        Fingerprint {
            size,
            content: Content::Sha256(hex),
        }
    }

    /// Of a fresh clone; read after all if its modification time cannot be told.
    fn of_clone(path: &Path) -> io::Result<Fingerprint> {
        let meta = fs::symlink_metadata(path)?;
        match modified(&meta) {
            Some(modified) => Ok(Fingerprint {
                size: meta.len(),
                content: Content::Modified(modified),
            }),
            None => hash_file(path),
        }
    }

    /// Whether the file `path` still has this content.
    fn matches(&self, path: &Path, meta: &Metadata) -> io::Result<bool> {
        if meta.len() != self.size {
            return Ok(false);
        }
        match &self.content {
            Content::Modified(recorded) => Ok(modified(meta) == Some(*recorded)),
            Content::Sha256(_) => Ok(hash_file(path)? == *self),
        }
    }
}

fn modified(meta: &Metadata) -> Option<i64> {
    let since = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    since.as_nanos().try_into().ok()
}

fn hash_file(path: &Path) -> io::Result<Fingerprint> {
    copy_hashed(&mut File::open(path)?, &mut io::sink())
}

/// Copies everything and tells what it was.
fn copy_hashed(from: &mut impl Read, to: &mut impl Write) -> io::Result<Fingerprint> {
    let mut hasher = Sha256::new();
    let mut size = 0;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let n = match from.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buffer[..n]);
        to.write_all(&buffer[..n])?;
        size += n as u64;
    }
    Ok(Fingerprint::hashed(size, &hasher.finalize()))
}

/// A checked file resource, ready to be created.
#[derive(Debug)]
pub(crate) struct Plan {
    pub(crate) record: Record,
    action: Action,
}

#[derive(Debug)]
enum Action {
    Copy { source: PathBuf, reflink: bool },
    Link { target: PathBuf, dir: bool },
    Write { bytes: Vec<u8>, source: PathBuf },
}

/// Checks a rule against the disk and prepares its creation. Returns `None` when there is
/// nothing to create: `mode = "ignore"`, or an optional source that does not exist.
///
/// `main` and `root` are expected in canonical form, as [`crate::worktree::Worktree`] gives them.
pub(crate) fn plan(spec: &FileSpec, main: &Path, root: &Path, vars: &Vars) -> Result<Option<Plan>> {
    plan_inner(spec, main, root, vars).with_context(|| format!("file {:?}", spec.path))
}

fn plan_inner(spec: &FileSpec, main: &Path, root: &Path, vars: &Vars) -> Result<Option<Plan>> {
    if spec.mode == FileMode::Ignore {
        return Ok(None);
    }

    let source = join(main, spec.source());
    // Follows a link: a source that is itself a link is used for what it points to.
    let meta = match fs::metadata(&source) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound && spec.optional => return Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => bail!(
            "source {} does not exist (set `optional = true` if it may be missing)",
            source.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", source.display())),
    };
    if !meta.is_file() && !meta.is_dir() {
        bail!("source {} is not a file or a directory", source.display());
    }

    let path = join(root, &spec.name());
    // Config validation keeps both inside; this guards against a validation gap.
    if !source.starts_with(main) || !path.starts_with(root) || path == root {
        bail!("path escapes the main checkout or the worktree");
    }
    if meta.is_dir()
        && matches!(spec.mode, FileMode::Copy | FileMode::Reflink)
        && root.starts_with(&source)
    {
        bail!(
            "cannot copy {} into the worktree inside it",
            source.display()
        );
    }
    let created_dirs = missing_parents(root, &path)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => bail!(
            "{} already exists; bp does not overwrite what it did not create",
            path.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    }

    let (entry, action) = match spec.mode {
        FileMode::Copy | FileMode::Reflink => (
            if meta.is_dir() {
                EntryType::Dir
            } else {
                EntryType::File
            },
            Action::Copy {
                source,
                reflink: spec.mode == FileMode::Reflink,
            },
        ),
        FileMode::Link => (
            EntryType::Link,
            Action::Link {
                target: source,
                dir: meta.is_dir(),
            },
        ),
        FileMode::Template => {
            if !meta.is_file() {
                bail!("a template source must be a file: {}", source.display());
            }
            let bytes = render_template(spec, &source, vars)?;
            (EntryType::File, Action::Write { bytes, source })
        }
        FileMode::Ignore => unreachable!("handled above"),
    };

    let fingerprint = match &action {
        Action::Write { bytes, .. } => Some(Fingerprint::of_bytes(bytes)),
        _ => None,
    };
    Ok(Some(Plan {
        record: Record {
            root: root.to_path_buf(),
            path,
            entry,
            created_dirs,
            cloned: None,
            trash: None,
            fingerprint,
            saved: Vec::new(),
        },
        action,
    }))
}

fn render_template(spec: &FileSpec, source: &Path, vars: &Vars) -> Result<Vec<u8>> {
    let bytes = fs::read(source).with_context(|| format!("cannot read {}", source.display()))?;
    let mut text = String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("{} is not UTF-8 text", source.display()))?;
    if spec.vars {
        text = vars
            .render_text(&text)
            .with_context(|| format!("in {}", source.display()))?;
    }
    for rule in &spec.replace {
        let with = vars.render(&rule.with)?;
        if !text.contains(&rule.find) {
            bail!(
                "replace rule {:?} matches nothing in {}; has the source changed?",
                rule.find,
                source.display()
            );
        }
        text = text.replace(&rule.find, &with);
    }
    Ok(text.into_bytes())
}

/// Creates what `plan` describes and returns the final record for the ledger.
pub(crate) fn create(plan: &Plan) -> Result<Record> {
    let mut record = plan.record.clone();
    create_inner(plan, &mut record)
        .with_context(|| format!("cannot create {}", record.path.display()))?;
    Ok(record)
}

fn create_inner(plan: &Plan, record: &mut Record) -> Result<()> {
    for dir in &record.created_dirs {
        match fs::create_dir(dir) {
            Ok(()) => {}
            // Created meanwhile by someone else: fine if it is a real directory, never a link.
            Err(e)
                if e.kind() == io::ErrorKind::AlreadyExists
                    && fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) => {}
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", dir.display())),
        }
    }
    let path = &record.path;
    match &plan.action {
        Action::Copy { source, reflink } => {
            let cloned = if record.entry == EntryType::Dir {
                fs::create_dir(path)?;
                copy_tree(source, path, *reflink)?
            } else {
                let copied = copy_file(source, path, *reflink, true)?;
                record.fingerprint = copied.fingerprint;
                Some(copied.cloned)
            };
            // `None` for a reflink of a tree without files: nothing was cloned or copied.
            record.cloned = if *reflink { cloned } else { None };
        }
        Action::Link { target, dir } => make_link(target, path, *dir)?,
        Action::Write { bytes, source } => {
            let mut file = File::create_new(path)?;
            file.write_all(bytes)?;
            // Mode bits on Unix, the read-only attribute on Windows, as `copy` does.
            file.set_permissions(fs::metadata(source)?.permissions())?;
        }
    }
    Ok(())
}

struct Copied {
    /// Copy-on-write.
    cloned: bool,
    fingerprint: Option<Fingerprint>,
}

/// Copies one file into a new `dest`, with its fingerprint if asked for.
///
/// `dest` is always created exclusively, so a file or link that appeared there since `plan` is
/// never overwritten or written through (`fs::copy` would do both).
fn copy_file(source: &Path, dest: &Path, reflink: bool, fingerprint: bool) -> Result<Copied> {
    let mut from = File::open(source)?;
    if reflink {
        match reflink_copy::reflink_file_to_path(&from, dest) {
            Ok(()) => {
                let fingerprint = match fingerprint {
                    true => Some(Fingerprint::of_clone(dest)?),
                    false => None,
                };
                return Ok(Copied {
                    cloned: true,
                    fingerprint,
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(e.into()),
            // Not supported here (file system, platform): fall back to a plain copy.
            Err(_) => {}
        }
    }
    let mut to = File::create_new(dest)?;
    let fingerprint = if fingerprint {
        Some(copy_hashed(&mut from, &mut to)?)
    } else {
        io::copy(&mut from, &mut to)?;
        None
    };
    to.set_permissions(from.metadata()?.permissions())?;
    Ok(Copied {
        cloned: false,
        fingerprint,
    })
}

/// Whatever the number of CPUs: measured on 4 of them, 8 threads were faster than 4 on Windows and about
/// as fast on Linux. 16 were hardly faster than 8 anywhere.
const COPY_THREADS: usize = 8;

/// Files waiting to be copied. Kept short, so that a file is copied soon after it was listed.
const COPY_QUEUE: usize = 64;

/// Source and destination.
type Pair = (PathBuf, PathBuf);

/// Copies the contents of `source` into the existing, empty `dest`; files on threads of their own, the
/// rest on this one. Links inside the tree are recreated as links to the same target, never followed.
/// Returns whether every file was cloned, `None` without files.
///
/// The first error stops the other threads. Nothing is written any more when this returns.
fn copy_tree(source: &Path, dest: &Path, reflink: bool) -> Result<Option<bool>> {
    let stop = AtomicBool::new(false);
    let mut dirs = vec![(source.to_path_buf(), dest.to_path_buf())];
    let cloned = thread::scope(|scope| -> Result<Option<bool>> {
        let (files, queue) = sync_channel(COPY_QUEUE);
        // Owned by the threads alone: once the last one is gone, sending fails instead of waiting.
        let queue = Arc::new(Mutex::new(queue));
        let workers = (0..COPY_THREADS)
            .map(|_| {
                let (queue, stop) = (Arc::clone(&queue), &stop);
                thread::Builder::new()
                    .spawn_scoped(scope, move || copy_queued(&queue, reflink, stop))
            })
            .collect::<io::Result<Vec<_>>>()
            .context("cannot start a thread")?;
        drop(queue);

        let listed = list_tree(source, dest, &files, &mut dirs);
        if listed.is_err() {
            stop.store(true, Ordering::Relaxed);
        }
        drop(files);

        let mut cloned = None;
        // Every thread is waited for by the scope, also after an error.
        for worker in workers {
            let this = worker
                .join()
                .unwrap_or_else(|e| std::panic::resume_unwind(e))?;
            cloned = both(cloned, this);
        }
        // Last: when a thread failed, listing fails only because of that.
        listed.map(|()| cloned)
    })?;

    // Last, so that a read-only source directory does not block writing its contents; the inner
    // ones first.
    for (source, dest) in dirs.iter().rev() {
        copy_dir_mode(source, dest)
            .with_context(|| format!("cannot set the permissions of {}", dest.display()))?;
    }
    Ok(cloned)
}

/// Creates the directories and links of the tree and queues its files.
fn list_tree(
    source: &Path,
    dest: &Path,
    files: &SyncSender<Pair>,
    dirs: &mut Vec<Pair>,
) -> Result<()> {
    let entries =
        fs::read_dir(source).with_context(|| format!("cannot read {}", source.display()))?;
    for entry in entries {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            copy_link(&from, &to)?;
        } else if file_type.is_dir() {
            fs::create_dir(&to).with_context(|| format!("cannot create {}", to.display()))?;
            dirs.push((from.clone(), to.clone()));
            list_tree(&from, &to, files, dirs)?;
        } else if file_type.is_file() {
            files.send((from, to)).context("copying has stopped")?;
        } else {
            bail!("{} is not a file, directory or link", from.display());
        }
    }
    Ok(())
}

/// Copies queued files until there are no more or `stop` is set, which it does itself on an error.
fn copy_queued(
    queue: &Mutex<Receiver<Pair>>,
    reflink: bool,
    stop: &AtomicBool,
) -> Result<Option<bool>> {
    let mut cloned = None;
    loop {
        // A statement of its own: the lock must not be held while copying.
        let next = queue.lock().expect("no thread panics with the lock").recv();
        let Ok((from, to)) = next else {
            return Ok(cloned);
        };
        if stop.load(Ordering::Relaxed) {
            return Ok(cloned);
        }
        match copy_file(&from, &to, reflink, false) {
            Ok(this) => cloned = both(cloned, Some(this.cloned)),
            Err(e) => {
                stop.store(true, Ordering::Relaxed);
                return Err(e.context(format!("cannot copy {}", from.display())));
            }
        }
    }
}

/// Whether all were cloned; `None` is nothing to clone.
fn both(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a && b),
        (a, b) => a.or(b),
    }
}

/// The owner keeps write access: in a read-only directory nothing could be removed on teardown.
#[cfg(unix)]
fn copy_dir_mode(source: &Path, dest: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(source)?.permissions().mode() | 0o200;
    Ok(fs::set_permissions(dest, fs::Permissions::from_mode(mode))?)
}

#[cfg(windows)]
fn copy_dir_mode(_source: &Path, _dest: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn copy_link(from: &Path, to: &Path) -> Result<()> {
    let target = fs::read_link(from)?;
    std::os::unix::fs::symlink(&target, to)
        .with_context(|| format!("cannot recreate link {}", from.display()))
}

#[cfg(windows)]
fn copy_link(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::fs::FileTypeExt;

    let target = fs::read_link(from)?;
    let result = if junction::exists(from).unwrap_or(false) {
        junction::create(&target, to)
    } else if fs::symlink_metadata(from)?.file_type().is_symlink_dir() {
        std::os::windows::fs::symlink_dir(&target, to)
    } else {
        std::os::windows::fs::symlink_file(&target, to)
    };
    result
        .map_err(symlink_error)
        .with_context(|| format!("cannot recreate link {}", from.display()))
}

#[cfg(unix)]
fn make_link(target: &Path, link: &Path, _dir: bool) -> Result<()> {
    Ok(std::os::unix::fs::symlink(target, link)?)
}

#[cfg(windows)]
fn make_link(target: &Path, link: &Path, dir: bool) -> Result<()> {
    if dir {
        // A junction needs no privileges. `junction::create` makes an empty directory first and
        // turns it into a junction; if that step fails, the empty directory is ours to remove.
        if let Err(e) = junction::create(target, link) {
            if fs::symlink_metadata(link).is_ok_and(|m| m.is_dir()) {
                let _ = fs::remove_dir(link);
            }
            return Err(e.into());
        }
        Ok(())
    } else {
        std::os::windows::fs::symlink_file(target, link).map_err(symlink_error)
    }
}

#[cfg(windows)]
fn symlink_error(e: io::Error) -> anyhow::Error {
    const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
    if e.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) {
        anyhow::Error::new(e).context(
            "a file symlink on Windows needs Developer Mode or administrator rights; \
             enable Developer Mode or use mode = \"copy\"",
        )
    } else {
        e.into()
    }
}

/// Removes what `record` describes, then the directories created for it while they are empty.
/// Already gone is fine; a path that is no longer what was created is left in place.
///
/// A directory is removed all or nothing: it is first renamed to a trash name next to it. On
/// Windows the rename fails while anything inside is in use (an open file, a process's working
/// directory), and then nothing has been touched. The trash name is chosen before the move and
/// handed to `on_moved` with the updated record, which must make it durable (the ledger); only
/// then is the directory moved. Interrupted anywhere after that, the next call with the recorded
/// record finishes the job: it deletes a leftover trash and moves a remaining directory. Single
/// files and links are removed in one operation. Transient locks are retried; a lasting one is
/// reported with the processes holding the path, when the OS can tell.
///
/// A single file that was edited is moved to `saved`, its new path recorded through `on_moved` in the
/// same way; if it cannot be moved it stays, which is an error. `kept` becomes the paths where this or
/// an earlier attempt kept the file, also when there is an error.
pub(crate) fn remove(
    record: &Record,
    saved: &Saved,
    kept: &mut Vec<PathBuf>,
    on_moved: impl FnOnce(&Record) -> Result<()>,
) -> Result<()> {
    remove_inner(record, saved, kept, on_moved)
        .with_context(|| format!("cannot remove {}", record.path.display()))
}

/// Where earlier attempts kept the file of `record`.
pub(crate) fn kept_files(record: &Record, saved: &Saved) -> Vec<PathBuf> {
    let exists = |target: &&PathBuf| fs::symlink_metadata(target).is_ok();
    let recorded = record.saved.iter().filter(|t| saved.contains(t));
    recorded.filter(exists).cloned().collect()
}

fn remove_inner(
    record: &Record,
    saved: &Saved,
    kept: &mut Vec<PathBuf>,
    on_moved: impl FnOnce(&Record) -> Result<()>,
) -> Result<()> {
    let path = &record.path;
    let inside = |p: &Path| p.starts_with(&record.root) && p != record.root;
    let trash_inside = record
        .trash
        .as_deref()
        .is_none_or(|t| inside(t) && t.parent() == path.parent());
    if !inside(path) || !trash_inside || !record.created_dirs.iter().all(|d| inside(d)) {
        bail!("refusing to touch a path outside {}", record.root.display());
    }

    *kept = kept_files(record, saved);
    if parents_are_real_dirs(&record.root, path)? {
        // A delete interrupted after the move; the trash shares the parents of `path`.
        if let Some(trash) = &record.trash {
            remove_trash(trash)?;
        }
        match fs::symlink_metadata(path) {
            Ok(meta) => remove_entry(record, &meta, saved, kept, on_moved)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }

    for dir in record.created_dirs.iter().rev() {
        match fs::symlink_metadata(dir) {
            Ok(meta) if meta.is_dir() => {}
            // Gone, or replaced by something we did not create.
            _ => continue,
        }
        match fsutil::retry(|| fs::remove_dir(dir)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            // Holds something we did not create, and so do its parents.
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => break,
            Err(e) => {
                return Err(inuse::error(dir, e))
                    .with_context(|| format!("cannot remove {}", dir.display()));
            }
        }
    }
    Ok(())
}

fn remove_entry(
    record: &Record,
    meta: &Metadata,
    saved: &Saved,
    kept: &mut Vec<PathBuf>,
    on_moved: impl FnOnce(&Record) -> Result<()>,
) -> Result<()> {
    let (path, expected) = (&record.path, record.entry);
    let file_type = meta.file_type();
    // `is_dir` and `is_file` are false for links, junctions included.
    let matches = match expected {
        EntryType::Link => file_type.is_symlink(),
        EntryType::File => file_type.is_file(),
        EntryType::Dir => file_type.is_dir(),
    };
    // On Windows a junction starts as an empty directory; a crash before it became a junction
    // leaves that directory behind. `remove_dir` only ever removes it while empty.
    if expected == EntryType::Link && cfg!(windows) && file_type.is_dir() {
        return fsutil::retry(|| fs::remove_dir(path))
            .context("left in place: not a link and not empty");
    }
    if !matches {
        bail!(
            "left in place: it is no longer the {} bp created",
            match expected {
                EntryType::Link => "link",
                EntryType::File => "file",
                EntryType::Dir => "directory",
            }
        );
    }
    match expected {
        EntryType::Link => {
            fsutil::retry(|| remove_link(path, meta)).map_err(|e| inuse::error(path, e))
        }
        EntryType::File => remove_file(record, meta, saved, kept, on_moved),
        EntryType::Dir => remove_tree(record, on_moved),
    }
}

/// Deletes a file that still is what was created, and moves one that was edited to `saved`.
fn remove_file(
    record: &Record,
    meta: &Metadata,
    saved: &Saved,
    kept: &mut Vec<PathBuf>,
    on_moved: impl FnOnce(&Record) -> Result<()>,
) -> Result<()> {
    let path = &record.path;
    let expendable = match is_expendable(record, meta, kept) {
        Ok(expendable) => expendable,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(inuse::error(path, e))
                .context("left in place: cannot tell whether it was edited");
        }
    };
    if expendable {
        match saved::remove_unchanged(path, &Stamp::of(meta)) {
            Ok(true) => return Ok(()),
            // Written to since it was looked at.
            Ok(false) if fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) => {}
            Ok(false) => bail!("left in place: it is no longer the file bp created"),
            Err(e) => return Err(inuse::error(path, e)),
        }
    }

    // Recorded by an earlier attempt that did not get to use it.
    let absent = |t: &&PathBuf| fs::symlink_metadata(t).is_err_and(|e| saved::is_absent(&e));
    let mut recorded = record.saved.iter().filter(|t| saved.contains(t));
    let target = match recorded.find(absent) {
        Some(target) => target.clone(),
        None => {
            let target = saved.choose(path.strip_prefix(&record.root)?)?;
            let mut moved = record.clone();
            moved.saved.push(target.clone());
            on_moved(&moved)
                .context("cannot record where the edited file goes; nothing was moved")?;
            target
        }
    };
    saved
        .take(path, &target)
        .context("it was edited and cannot be kept")?;
    kept.push(target);
    Ok(())
}

/// Whether nothing is lost with the file: it is what was created, or an earlier attempt kept the
/// same content but could not remove the file.
fn is_expendable(record: &Record, meta: &Metadata, kept: &[PathBuf]) -> io::Result<bool> {
    let path = &record.path;
    let Some(fingerprint) = &record.fingerprint else {
        return Ok(true);
    };
    if fsutil::retry(|| fingerprint.matches(path, meta))? {
        return Ok(true);
    }
    for copy in kept {
        // Anything unexpected about the copy: keep the file once more.
        if fs::symlink_metadata(copy).is_ok_and(|m| m.is_file() && m.len() == meta.len())
            && let Ok(copy) = hash_file(copy)
            && fsutil::retry(|| hash_file(path))? == copy
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Infix of trash names: `<name>.bp-trash-<unique>` next to the directory being removed.
const TRASH: &str = ".bp-trash-";

/// Moves a copied directory to its trash name, recorded first, then deletes it (see [`remove`]).
fn remove_tree(record: &Record, on_moved: impl FnOnce(&Record) -> Result<()>) -> Result<()> {
    let path = &record.path;
    let trash = match &record.trash {
        // Recorded by an earlier attempt; `remove_inner` has already cleared it.
        Some(trash) => trash.clone(),
        None => {
            let trash = trash_path(path);
            on_moved(&Record {
                trash: Some(trash.clone()),
                ..record.clone()
            })
            .context("cannot record the trash name; nothing was moved")?;
            trash
        }
    };
    fsutil::retry(|| fs::rename(path, &trash)).map_err(|e| inuse::error(path, e))?;
    remove_trash(&trash).with_context(|| format!("moved to {}, but", trash.display()))
}

/// Deletes a trash directory left by `remove_tree`. Gone is fine; anything other than a directory
/// is not ours and is left in place.
fn remove_trash(trash: &Path) -> Result<()> {
    match fs::symlink_metadata(trash) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => bail!(
            "left in place: {} is not the directory bp moved there",
            trash.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    // Does not follow links inside the tree: they are removed as links.
    match fsutil::retry(|| fs::remove_dir_all(trash)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(inuse::error(trash, e)),
    }
}

/// A unique sibling of `path` (same directory, so the same volume: the rename never copies).
fn trash_path(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!("{TRASH}{}-{nanos:x}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(unix)]
fn remove_link(path: &Path, _meta: &Metadata) -> io::Result<()> {
    fs::remove_file(path)
}

#[cfg(windows)]
fn remove_link(path: &Path, meta: &Metadata) -> io::Result<()> {
    use std::os::windows::fs::FileTypeExt;
    // A directory link (junction or symlink) is removed with RemoveDirectory, which deletes
    // the link itself and never touches its target.
    if meta.file_type().is_symlink_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// `base` joined with a `/`- or `\`-separated relative path, using native separators.
fn join(base: &Path, relative: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    for segment in relative
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
    {
        path.push(segment);
    }
    path
}

/// The parent directories of `path` below `root` that do not exist yet, outermost first.
/// Fails if an existing parent is a link or not a directory: creating through a link could write
/// into the main checkout.
fn missing_parents(root: &Path, path: &Path) -> Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    for dir in parents_below(root, path) {
        if !missing.is_empty() {
            missing.push(dir);
            continue;
        }
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(meta) if meta.file_type().is_symlink() => bail!(
                "{} is a link; refusing to create files through it",
                dir.display()
            ),
            Ok(_) => bail!("{} is not a directory", dir.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push(dir),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
        }
    }
    Ok(missing)
}

/// Whether every parent of `path` below `root` is a real directory. `Ok(false)` if one is
/// missing (then `path` is gone too); an error if one is a link or a file.
fn parents_are_real_dirs(root: &Path, path: &Path) -> Result<bool> {
    for dir in parents_below(root, path) {
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!(
                "left in place: {} is no longer a directory bp can safely descend into",
                dir.display()
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

/// Directories strictly between `root` and `path`, outermost first.
fn parents_below(root: &Path, path: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path
        .ancestors()
        .skip(1)
        .take_while(|p| *p != root && p.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Replace;
    use std::collections::BTreeMap;

    struct Tree {
        _tmp: tempfile::TempDir,
        main: PathBuf,
        wt: PathBuf,
    }

    fn tree() -> Tree {
        let tmp = tempfile::tempdir().unwrap();
        // Non-ASCII on purpose: it is the normal case on many developer machines.
        let main = tmp.path().join("main-caf\u{e9}");
        let wt = tmp.path().join("wt-na\u{ef}ve");
        fs::create_dir(&main).unwrap();
        fs::create_dir(&wt).unwrap();
        Tree {
            main: dunce::canonicalize(&main).unwrap(),
            wt: dunce::canonicalize(&wt).unwrap(),
            _tmp: tmp,
        }
    }

    fn spec(path: &str, mode: FileMode) -> FileSpec {
        FileSpec {
            path: path.to_owned(),
            mode,
            source: None,
            optional: false,
            vars: true,
            replace: Vec::new(),
        }
    }

    fn vars() -> Vars {
        let mut v = Vars::new();
        v.set("port.web", "8003");
        v
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn up(t: &Tree, spec: &FileSpec) -> Record {
        let plan = plan(spec, &t.main, &t.wt, &vars())
            .unwrap()
            .expect("something to create");
        create(&plan).unwrap()
    }

    /// Removes without a ledger: the trash location is not recorded anywhere.
    fn rm(record: &Record) -> Result<()> {
        remove(record, &saved(record), &mut Vec::new(), |_| Ok(()))
    }

    /// Edited files go next to the worktree.
    fn saved(record: &Record) -> Saved {
        Saved::new(&record.root.with_file_name("home"), "wt-00000000")
    }

    /// Removes links first, then the rest, each in reverse order, as `bp down` does.
    fn rm_all(records: &[Record]) -> Vec<Result<()>> {
        let links = records.iter().rev().filter(|r| r.entry == EntryType::Link);
        let others = records.iter().rev().filter(|r| r.entry != EntryType::Link);
        links.chain(others).map(rm).collect()
    }

    /// Makes a directory link the way `bp` does on this platform (no privileges needed).
    fn dir_link(target: &Path, link: &Path) {
        make_link(target, link, true).unwrap();
    }

    /// File symlinks need Developer Mode or admin rights on Windows; CI runners have them, a
    /// developer machine may not.
    fn file_symlinks_allowed(t: &Tree) -> bool {
        let probe = t.wt.join("probe");
        let ok = make_link(&t.main, &probe, false).is_ok();
        let _ = fs::remove_file(&probe);
        ok
    }

    #[test]
    fn copies_a_file_and_removes_it() {
        let t = tree();
        write(&t.main.join(".env"), "A=1\n");
        let record = up(&t, &spec(".env", FileMode::Copy));
        assert_eq!(fs::read_to_string(t.wt.join(".env")).unwrap(), "A=1\n");
        assert_eq!(record.entry, EntryType::File);
        assert_eq!(record.cloned, None);

        rm(&record).unwrap();
        assert!(!t.wt.join(".env").exists());
        // Idempotent.
        rm(&record).unwrap();
        // The main checkout is untouched.
        assert!(t.main.join(".env").exists());
    }

    #[test]
    fn copies_a_directory_into_new_parents_and_removes_them() {
        let t = tree();
        write(&t.main.join("conf/local/a.txt"), "a");
        write(&t.main.join("conf/local/sub/b.txt"), "b");
        let mut s = spec("deep/er/local", FileMode::Copy);
        s.source = Some("conf/local".into());
        let record = up(&t, &s);
        assert_eq!(record.entry, EntryType::Dir);
        assert_eq!(
            record.created_dirs,
            [t.wt.join("deep"), t.wt.join("deep").join("er")]
        );
        assert_eq!(
            fs::read_to_string(t.wt.join("deep/er/local/sub/b.txt")).unwrap(),
            "b"
        );

        // Whatever the app wrote into the copied directory goes with it.
        write(&t.wt.join("deep/er/local/cache.bin"), "x");
        rm(&record).unwrap();
        assert!(!t.wt.join("deep").exists());
        assert!(t.wt.exists());
    }

    /// What is below `dir`, by relative path: the text of a file, where a link leads, or a directory.
    fn contents(dir: &Path) -> BTreeMap<PathBuf, String> {
        fn visit(base: &Path, dir: &Path, found: &mut BTreeMap<PathBuf, String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let file_type = fs::symlink_metadata(&path).unwrap().file_type();
                let what = if file_type.is_symlink() {
                    format!("link to {}", fs::canonicalize(&path).unwrap().display())
                } else if file_type.is_dir() {
                    visit(base, &path, found);
                    "directory".to_owned()
                } else {
                    format!("file {}", fs::read_to_string(&path).unwrap())
                };
                found.insert(path.strip_prefix(base).unwrap().to_path_buf(), what);
            }
        }
        let mut found = BTreeMap::new();
        visit(dir, dir, &mut found);
        found
    }

    #[test]
    fn a_tree_of_many_files_is_copied_whole() {
        let t = tree();
        let source = t.main.join("vendor");
        for i in 0..600 {
            let dir = format!("pkg-{}/caf\u{e9}/{}", i % 7, i % 5);
            write(&source.join(dir).join(format!("{i}.txt")), &i.to_string());
        }
        write(&source.join("pkg-0/empty.txt"), "");
        fs::create_dir(source.join("empty")).unwrap();
        write(&t.main.join("shared/sentinel.txt"), "keep me");
        dir_link(&t.main.join("shared"), &source.join("pkg-3/lib"));

        for mode in [FileMode::Copy, FileMode::Reflink] {
            let record = up(&t, &spec("vendor", mode));
            assert_eq!(record.cloned.is_some(), mode == FileMode::Reflink);
            let copied = contents(&t.wt.join("vendor"));
            assert_eq!(copied, contents(&source), "{mode:?}");
            assert_eq!(copied.values().filter(|w| w.starts_with("link")).count(), 1);
            rm(&record).unwrap();
            assert!(!t.wt.join("vendor").exists());
        }
        assert!(t.main.join("shared/sentinel.txt").exists());
    }

    /// With more files than the queue holds. Not returning is a failure, too: nobody might be left to
    /// take from a full queue.
    fn copy_tree_in_time(source: &Path, dest: &Path) -> Result<Option<bool>> {
        let (done, result) = sync_channel(1);
        let (source, dest) = (source.to_path_buf(), dest.to_path_buf());
        thread::spawn(move || done.send(copy_tree(&source, &dest, false)));
        result
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("copying does not end")
    }

    #[test]
    fn the_first_error_stops_copying_and_names_the_file() {
        const FILES: usize = 4000;
        let t = tree();
        let source = t.main.join("vendor");
        fs::create_dir(&source).unwrap();
        for i in 0..FILES {
            fs::write(source.join(format!("{i}.txt")), "from main").unwrap();
        }
        let plan = plan(&spec("vendor", FileMode::Copy), &t.main, &t.wt, &vars());
        let record = plan.unwrap().unwrap().record;

        // In the way of the file that comes first.
        let first = fs::read_dir(&source).unwrap().next().unwrap().unwrap();
        let dest = t.wt.join("vendor");
        fs::create_dir(&dest).unwrap();
        let planted = dest.join(first.file_name());
        fs::write(&planted, "user data").unwrap();

        let err = format!("{:#}", copy_tree_in_time(&source, &dest).unwrap_err());
        let expected = format!("cannot copy {}", first.path().display());
        assert!(err.contains(&expected), "{err}");
        assert_eq!(fs::read_to_string(&planted).unwrap(), "user data");
        let copied = files_in(&dest);
        assert!(copied < FILES, "the other threads went on: {copied}");

        // What was copied by then goes as after any failure.
        rm(&record).unwrap();
        assert!(!dest.exists());
        assert_eq!(files_in(&source), FILES);
    }

    /// The error is about what cannot be copied, not about the threads.
    #[cfg(unix)]
    #[test]
    fn an_entry_that_cannot_be_copied_stops_copying() {
        let t = tree();
        let source = t.main.join("vendor");
        for i in 0..200 {
            write(&source.join(format!("{i}.txt")), "x");
        }
        let pipe = source.join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&pipe).status();
        assert!(made.unwrap().success());
        let dest = t.wt.join("vendor");
        fs::create_dir(&dest).unwrap();
        let err = format!("{:#}", copy_tree_in_time(&source, &dest).unwrap_err());
        let expected = format!("{} is not a file, directory or link", pipe.display());
        assert!(err.contains(&expected), "{err}");
    }

    #[test]
    fn a_tree_is_cloned_only_if_all_of_it_is() {
        assert_eq!(both(None, None), None);
        assert_eq!(both(Some(true), None), Some(true));
        assert_eq!(both(None, Some(false)), Some(false));
        assert_eq!(both(Some(true), Some(true)), Some(true));
        assert_eq!(both(Some(false), Some(true)), Some(false));
        assert_eq!(both(Some(true), Some(false)), Some(false));
    }

    #[test]
    fn created_parents_holding_other_files_are_kept() {
        let t = tree();
        write(&t.main.join("a/b.txt"), "b");
        let record = up(&t, &spec("a/b.txt", FileMode::Copy));
        write(&t.wt.join("a/mine.txt"), "user data");
        rm(&record).unwrap();
        assert!(!t.wt.join("a/b.txt").exists());
        assert!(t.wt.join("a/mine.txt").exists());
    }

    #[test]
    fn reflink_clones_or_falls_back_to_a_copy() {
        let t = tree();
        write(&t.main.join("big.bin"), "data");
        write(&t.main.join("dir/x.bin"), "x");
        let file = up(&t, &spec("big.bin", FileMode::Reflink));
        let dir = up(&t, &spec("dir", FileMode::Reflink));
        assert_eq!(fs::read_to_string(t.wt.join("big.bin")).unwrap(), "data");
        assert_eq!(fs::read_to_string(t.wt.join("dir/x.bin")).unwrap(), "x");
        assert!(file.cloned.is_some() && dir.cloned.is_some());
        for r in rm_all(&[file, dir]) {
            r.unwrap();
        }
        assert!(!t.wt.join("big.bin").exists() && !t.wt.join("dir").exists());
    }

    #[test]
    fn missing_source_fails_unless_optional() {
        let t = tree();
        let mut s = spec("absent", FileMode::Copy);
        let err = plan(&s, &t.main, &t.wt, &vars()).unwrap_err();
        assert!(format!("{err:#}").contains("optional"), "{err:#}");
        s.optional = true;
        assert!(plan(&s, &t.main, &t.wt, &vars()).unwrap().is_none());
        assert!(
            plan(&spec("x", FileMode::Ignore), &t.main, &t.wt, &vars())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn never_overwrites_an_existing_destination() {
        let t = tree();
        write(&t.main.join(".env"), "from main");
        write(&t.wt.join(".env"), "committed");
        assert!(plan(&spec(".env", FileMode::Copy), &t.main, &t.wt, &vars()).is_err());
        assert_eq!(fs::read_to_string(t.wt.join(".env")).unwrap(), "committed");
    }

    #[test]
    fn never_creates_through_a_link() {
        let t = tree();
        fs::create_dir(t.main.join("node_modules")).unwrap();
        write(&t.main.join("cache/x"), "x");
        let link = up(&t, &spec("node_modules", FileMode::Link));
        let mut s = spec("node_modules/x", FileMode::Copy);
        s.source = Some("cache/x".into());
        let err = plan(&s, &t.main, &t.wt, &vars()).unwrap_err();
        assert!(format!("{err:#}").contains("is a link"), "{err:#}");
        assert!(!t.main.join("node_modules/x").exists());
        rm(&link).unwrap();
    }

    #[test]
    fn a_path_that_changed_type_is_left_in_place() {
        let t = tree();
        write(&t.main.join("f"), "f");
        let record = up(&t, &spec("f", FileMode::Copy));
        fs::remove_file(t.wt.join("f")).unwrap();
        write(&t.wt.join("f/user.txt"), "user data");
        let err = rm(&record).unwrap_err();
        assert!(format!("{err:#}").contains("left in place"), "{err:#}");
        assert!(t.wt.join("f/user.txt").exists());
    }

    #[test]
    fn links_a_directory_and_removes_only_the_link() {
        let t = tree();
        write(&t.main.join("node_modules/pkg/index.js"), "js");
        let record = up(&t, &spec("node_modules", FileMode::Link));
        assert_eq!(record.entry, EntryType::Link);
        let meta = fs::symlink_metadata(t.wt.join("node_modules")).unwrap();
        assert!(meta.file_type().is_symlink(), "a link, not a copy");
        // Writes through the link land in the main checkout.
        write(&t.wt.join("node_modules/new.js"), "new");
        assert!(t.main.join("node_modules/new.js").exists());

        rm(&record).unwrap();
        assert!(fs::symlink_metadata(t.wt.join("node_modules")).is_err());
        assert!(t.main.join("node_modules/pkg/index.js").exists());
        assert!(t.main.join("node_modules/new.js").exists());
    }

    #[test]
    fn links_a_file_where_allowed() {
        let t = tree();
        if !file_symlinks_allowed(&t) {
            eprintln!("skipped: file symlinks are not allowed for this user");
            return;
        }
        write(&t.main.join("s3.php"), "<?php");
        let record = up(&t, &spec("s3.php", FileMode::Link));
        assert_eq!(fs::read_to_string(t.wt.join("s3.php")).unwrap(), "<?php");
        rm(&record).unwrap();
        assert!(fs::symlink_metadata(t.wt.join("s3.php")).is_err());
        assert!(t.main.join("s3.php").exists());
    }

    /// The regression this guards against destroys the main checkout: a recursive delete of a
    /// copied directory following a link (a junction on Windows) into the target.
    #[test]
    fn teardown_never_follows_a_link_into_the_main_checkout() {
        let t = tree();
        write(&t.main.join("shared/sentinel.txt"), "keep me");
        write(&t.main.join("app/index.txt"), "app");
        // A link inside the copied tree itself, recreated as a link by the copy.
        dir_link(&t.main.join("shared"), &t.main.join("app/lib"));

        let app = up(&t, &spec("app", FileMode::Copy));
        let mut vendor = spec("app/vendor", FileMode::Link);
        vendor.source = Some("shared".into());
        let vendor = up(&t, &vendor);
        let copied_link = fs::symlink_metadata(t.wt.join("app/lib")).unwrap();
        assert!(copied_link.file_type().is_symlink(), "copied as a link");
        assert!(t.wt.join("app/vendor/sentinel.txt").exists());

        for r in rm_all(&[app.clone(), vendor]) {
            r.unwrap();
        }
        assert!(!t.wt.join("app").exists());
        assert_eq!(
            fs::read_to_string(t.main.join("shared/sentinel.txt")).unwrap(),
            "keep me"
        );

        // Even without the links-first pass, the recursive delete removes links as links.
        let app = up(&t, &spec("app", FileMode::Copy));
        dir_link(&t.main.join("shared"), &t.wt.join("app/vendor"));
        rm(&app).unwrap();
        assert!(!t.wt.join("app").exists());
        assert!(t.main.join("shared/sentinel.txt").exists());
        assert!(t.main.join("app/lib").exists());
    }

    #[test]
    fn teardown_does_not_descend_through_a_replaced_parent() {
        let t = tree();
        write(&t.main.join("x/y/z.txt"), "z");
        write(&t.main.join("other/y/z.txt"), "keep me");
        let record = up(&t, &spec("x/y", FileMode::Copy));
        // The user replaced the created parent with a link into the main checkout.
        fs::remove_dir_all(t.wt.join("x")).unwrap();
        dir_link(&t.main.join("other"), &t.wt.join("x"));

        let err = rm(&record).unwrap_err();
        assert!(format!("{err:#}").contains("left in place"), "{err:#}");
        assert!(t.main.join("other/y/z.txt").exists());
    }

    #[test]
    fn an_interrupted_creation_is_undone_from_the_plan() {
        let t = tree();
        write(&t.main.join("d/a.txt"), "a");
        write(&t.main.join("d/sub/b.txt"), "b");
        let mut s = spec("p/d", FileMode::Copy);
        s.source = Some("d".into());
        let plan = plan(&s, &t.main, &t.wt, &vars()).unwrap().unwrap();
        // Crash halfway: parents and part of the tree exist, `create` never returned. Directories
        // are ahead of their files.
        write(&t.wt.join("p/d/a.txt"), "half");
        fs::create_dir(t.wt.join("p/d/sub")).unwrap();
        rm(&plan.record).unwrap();
        assert!(!t.wt.join("p").exists());

        // Crash before anything was made.
        rm(&plan.record).unwrap();
    }

    #[test]
    fn a_directory_is_moved_to_trash_and_the_move_recorded_first() {
        let t = tree();
        write(&t.main.join("vendor/pkg/a.txt"), "a");
        let record = up(&t, &spec("vendor", FileMode::Copy));
        let mut recorded = None;
        remove(&record, &saved(&record), &mut Vec::new(), |moved| {
            // Recorded before anything is moved.
            assert!(record.path.join("pkg/a.txt").exists());
            let trash = moved.trash.as_deref().expect("trash name");
            assert!(!trash.exists());
            assert_eq!(trash.parent(), record.path.parent());
            assert_eq!(moved.path, record.path);
            recorded = Some(moved.clone());
            Ok(())
        })
        .unwrap();
        recorded.expect("move reported");
        assert_eq!(fs::read_dir(&t.wt).unwrap().count(), 0);
    }

    #[test]
    fn every_interruption_of_a_directory_delete_is_finished_from_the_record() {
        let t = tree();
        write(&t.main.join("vendor/a.txt"), "a");
        let created = up(&t, &spec("vendor", FileMode::Copy));
        let trash = t.wt.join("vendor.bp-trash-1-2");
        let recorded = Record {
            trash: Some(trash.clone()),
            ..created.clone()
        };
        let no_record = |_: &Record| -> Result<()> { panic!("already recorded") };

        // Crashed after recording, before the move: the directory is still in place.
        rm_with(&recorded, no_record);
        assert_eq!(fs::read_dir(&t.wt).unwrap().count(), 0);

        // Crashed after the move, during the delete: only a partial trash is left.
        fs::create_dir_all(trash.join("half")).unwrap();
        rm_with(&recorded, no_record);
        assert!(!trash.exists());

        // Both: a new directory at the path (a later `up`) and an old trash.
        up(&t, &spec("vendor", FileMode::Copy));
        fs::create_dir_all(trash.join("half")).unwrap();
        rm_with(&recorded, no_record);
        assert_eq!(fs::read_dir(&t.wt).unwrap().count(), 0);
    }

    fn rm_with(record: &Record, on_moved: impl FnOnce(&Record) -> Result<()>) {
        remove(record, &saved(record), &mut Vec::new(), on_moved).unwrap();
    }

    #[test]
    fn a_failed_record_of_the_trash_name_moves_nothing() {
        let t = tree();
        write(&t.main.join("vendor/a.txt"), "a");
        let record = up(&t, &spec("vendor", FileMode::Copy));
        let full = |_: &Record| bail!("disk full");
        let err = remove(&record, &saved(&record), &mut Vec::new(), full).unwrap_err();
        assert!(format!("{err:#}").contains("disk full"), "{err:#}");
        assert!(t.wt.join("vendor/a.txt").exists());
        assert_eq!(fs::read_dir(&t.wt).unwrap().count(), 1);
    }

    #[test]
    fn a_trash_outside_the_parent_is_refused() {
        let t = tree();
        write(&t.main.join("vendor/a.txt"), "a");
        let record = Record {
            trash: Some(t.wt.join("elsewhere").join("x")),
            ..up(&t, &spec("vendor", FileMode::Copy))
        };
        assert!(rm(&record).is_err());
        assert!(t.wt.join("vendor/a.txt").exists());
    }

    /// On Windows a directory with an open file inside cannot be renamed, even when the file was
    /// opened with `FILE_SHARE_DELETE` (as std opens files): nothing is deleted, and the report
    /// names the holder.
    #[cfg(windows)]
    #[test]
    fn a_directory_in_use_is_left_whole_and_its_holder_named() {
        let t = tree();
        write(&t.main.join("vendor/a.txt"), "a");
        write(&t.main.join("vendor/sub/b.txt"), "b");
        let record = up(&t, &spec("vendor", FileMode::Copy));
        let open = File::open(t.wt.join("vendor/sub/b.txt")).unwrap();

        let err = rm(&record).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("in use by"), "{message}");
        assert!(
            message.contains(&format!("pid {}", std::process::id())),
            "{message}"
        );
        assert!(t.wt.join("vendor/a.txt").exists());
        assert!(t.wt.join("vendor/sub/b.txt").exists());

        drop(open);
        rm(&record).unwrap();
        assert!(!t.wt.join("vendor").exists());
    }

    #[cfg(windows)]
    #[test]
    fn a_file_in_use_names_its_holder() {
        use std::os::windows::fs::OpenOptionsExt;
        let t = tree();
        write(&t.main.join(".env"), "A=1\n");
        let record = up(&t, &spec(".env", FileMode::Copy));
        // No FILE_SHARE_DELETE: the way many editors and tools open files.
        let open = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(t.wt.join(".env"))
            .unwrap();
        let err = rm(&record).unwrap_err();
        assert!(format!("{err:#}").contains("in use by"), "{err:#}");
        drop(open);
        rm(&record).unwrap();
        assert!(!t.wt.join(".env").exists());
    }

    /// The only file kept for the pod of `record`.
    fn kept_text(record: &Record, kept: &[PathBuf]) -> String {
        assert_eq!(kept.len(), 1, "{kept:?}");
        let home = record.root.with_file_name("home");
        assert!(kept[0].starts_with(home.join("saved").join("wt-00000000")));
        let relative = record.path.strip_prefix(&record.root).unwrap();
        assert!(kept[0].ends_with(relative), "{}", kept[0].display());
        fs::read_to_string(&kept[0]).unwrap()
    }

    fn files_in(dir: &Path) -> usize {
        fs::read_dir(dir).map_or(0, |entries| {
            entries
                .map(|e| e.unwrap().path())
                .map(|p| if p.is_dir() { files_in(&p) } else { 1 })
                .sum()
        })
    }

    /// Moves go into the record, as `bp down` does with the ledger.
    fn rm_recorded(record: &mut Record) -> Result<Vec<PathBuf>> {
        let mut recorded = None;
        let mut kept = Vec::new();
        let result = remove(record, &saved(record), &mut kept, |moved| {
            recorded = Some(moved.clone());
            Ok(())
        });
        if let Some(moved) = recorded {
            *record = moved;
        }
        result.map(|()| kept)
    }

    #[test]
    fn an_edited_file_is_kept() {
        for mode in [FileMode::Copy, FileMode::Reflink, FileMode::Template] {
            let mut edits = vec!["A=1\nB=2\n", "", "A={{port.web}}\n"];
            // The same size and, for a clone, possibly the same modification time.
            if mode != FileMode::Reflink {
                edits.push(if mode == FileMode::Copy {
                    "A=2\n"
                } else {
                    "A=9003\n"
                });
            }
            for edit in edits {
                let t = tree();
                let source = match mode {
                    FileMode::Template => "A={{port.web}}\n",
                    _ => "A=1\n",
                };
                write(&t.main.join("conf/.env"), source);
                let mut record = up(&t, &spec("conf/.env", mode));
                fs::write(t.wt.join("conf/.env"), edit).unwrap();

                let kept = rm_recorded(&mut record).unwrap();
                assert_eq!(kept_text(&record, &kept), edit, "{mode:?}");
                assert!(!t.wt.join("conf").exists());
                assert_eq!(
                    fs::read_to_string(t.main.join("conf/.env")).unwrap(),
                    source
                );
                // Again, as after an interruption before the step was recorded.
                assert_eq!(rm_recorded(&mut record).unwrap(), kept);
                assert_eq!(kept_text(&record, &kept), edit);
            }
        }
    }

    #[test]
    fn a_file_replaced_by_rename_is_kept() {
        let t = tree();
        write(&t.main.join(".env"), "A=1\n");
        let mut record = up(&t, &spec(".env", FileMode::Copy));
        write(&t.wt.join("new"), "A=2\n");
        fs::rename(t.wt.join("new"), t.wt.join(".env")).unwrap();
        let kept = rm_recorded(&mut record).unwrap();
        assert_eq!(kept_text(&record, &kept), "A=2\n");
    }

    #[test]
    fn an_unchanged_file_is_not_kept() {
        for mode in [FileMode::Copy, FileMode::Reflink, FileMode::Template] {
            let t = tree();
            write(&t.main.join(".env"), "A=1\r\nB=\u{e9}\n");
            let mut record = up(&t, &spec(".env", mode));
            let path = t.wt.join(".env");
            // Only a clone is told by its modification time.
            if record.cloned != Some(true) {
                let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
                file.set_modified(UNIX_EPOCH).unwrap();
                drop(file);
                fs::write(&path, fs::read(&path).unwrap()).unwrap();
            }
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&path, permissions).unwrap();
            // The source changing is not an edit of the copy.
            write(&t.main.join(".env"), "A=2\n");

            assert!(rm_recorded(&mut record).unwrap().is_empty(), "{mode:?}");
            assert!(!path.exists());
            assert_eq!(files_in(&t.wt.with_file_name("home")), 0);
        }
    }

    #[test]
    fn an_edited_file_that_was_deleted_is_gone() {
        let t = tree();
        write(&t.main.join(".env"), "A=1\n");
        let mut record = up(&t, &spec(".env", FileMode::Copy));
        fs::write(t.wt.join(".env"), "A=2\n").unwrap();
        fs::remove_file(t.wt.join(".env")).unwrap();
        assert!(rm_recorded(&mut record).unwrap().is_empty());
        assert_eq!(files_in(&t.wt.with_file_name("home")), 0);
    }

    #[test]
    fn a_record_without_a_fingerprint_is_removed_as_before() {
        let t = tree();
        write(&t.main.join(".env"), "A=1\n");
        let record = up(&t, &spec(".env", FileMode::Copy));
        fs::write(t.wt.join(".env"), "A=2\n").unwrap();
        let mut old = serde_json::to_value(&record).unwrap();
        assert!(old.as_object_mut().unwrap().remove("fingerprint").is_some());
        let mut old: Record = serde_json::from_value(old).unwrap();
        assert!(rm_recorded(&mut old).unwrap().is_empty());
        assert!(!t.wt.join(".env").exists());
    }

    #[test]
    fn only_single_files_have_a_fingerprint() {
        let t = tree();
        write(&t.main.join("dir/a.txt"), "a");
        for mode in [FileMode::Copy, FileMode::Reflink] {
            let record = up(&t, &spec("dir", mode));
            assert_eq!(record.fingerprint, None);
            rm(&record).unwrap();
        }
        let link = up(&t, &spec("dir", FileMode::Link));
        assert_eq!(link.fingerprint, None);
        rm(&link).unwrap();
        // A template's is in the plan already; a copy's comes with its content.
        write(&t.main.join("f"), "f");
        for (mode, planned) in [(FileMode::Copy, false), (FileMode::Template, true)] {
            let plan = plan(&spec("f", mode), &t.main, &t.wt, &vars()).unwrap();
            let plan = plan.unwrap();
            assert_eq!(plan.record.fingerprint.is_some(), planned, "{mode:?}");
        }
    }

    /// A clone is not read: it goes by its size and modification time.
    #[test]
    fn a_clone_is_told_by_its_modification_time() {
        let t = tree();
        write(&t.main.join("big.bin"), "data");
        let mut record = up(&t, &spec("big.bin", FileMode::Reflink));
        if record.cloned != Some(true) {
            eprintln!("skipped: no copy-on-write clones here");
            return;
        }
        let path = t.wt.join("big.bin");
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(UNIX_EPOCH).unwrap();
        drop(file);
        let kept = rm_recorded(&mut record).unwrap();
        assert_eq!(kept_text(&record, &kept), "data");
    }

    #[test]
    fn every_interruption_of_keeping_a_file_is_finished_from_the_record() {
        let t = tree();
        write(&t.main.join(".env"), "A=1\n");
        let edited = || {
            let mut record = up(&t, &spec(".env", FileMode::Copy));
            fs::write(t.wt.join(".env"), "A=2\n").unwrap();
            let target = saved(&record).choose(Path::new(".env")).unwrap();
            record.saved = vec![target.clone()];
            (record, target)
        };

        // Crashed after recording, before the move.
        let (mut record, _) = edited();
        let kept = rm_recorded(&mut record).unwrap();
        assert_eq!(kept_text(&record, &kept), "A=2\n");

        // Crashed after the move: the other tests call `remove` again. After a copy to another
        // volume, before the removal: the copy is enough.
        let (mut record, target) = edited();
        write(&target, "A=2\n");
        let kept = rm_recorded(&mut record).unwrap();
        assert_eq!(kept, [target]);
        assert_eq!(kept_text(&record, &kept), "A=2\n");
        assert!(!t.wt.join(".env").exists());

        // The same, but edited once more since: neither is lost.
        let (mut record, target) = edited();
        write(&target, "A=2\n");
        fs::write(t.wt.join(".env"), "A=3\n").unwrap();
        let kept = rm_recorded(&mut record).unwrap();
        let texts: Vec<_> = kept
            .iter()
            .map(|p| fs::read_to_string(p).unwrap())
            .collect();
        assert_eq!(texts, ["A=2\n", "A=3\n"]);

        // Crashed while copying, and then the user deleted the file.
        let (mut record, _) = edited();
        fs::remove_file(t.wt.join(".env")).unwrap();
        assert!(rm_recorded(&mut record).unwrap().is_empty());
    }

    /// The look right before the removal compares with the one the decision was made on.
    #[test]
    fn a_file_touched_since_the_decision_is_kept() {
        let t = tree();
        write(&t.main.join(".env"), "A=1\n");
        let record = up(&t, &spec(".env", FileMode::Copy));
        let path = t.wt.join(".env");
        let seen = fs::symlink_metadata(&path).unwrap();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(UNIX_EPOCH).unwrap();
        drop(file);
        let mut kept = Vec::new();
        remove_file(&record, &seen, &saved(&record), &mut kept, |_| Ok(())).unwrap();
        assert_eq!(kept_text(&record, &kept), "A=1\n");
    }

    /// A damaged record: nothing is taken for kept, or put, outside the place for it.
    #[test]
    fn recorded_places_of_others_are_ignored() {
        let t = tree();
        write(&t.main.join(".env"), "A=1\n");
        let path = t.wt.join(".env");
        let home = t.wt.with_file_name("home");
        let foreign = [
            path.clone(),
            t.wt.join("elsewhere"),
            home.join("saved").join("other-pod").join("1").join(".env"),
            home.join("pods").join("x"),
        ];
        for place in foreign {
            let mut record = up(&t, &spec(".env", FileMode::Copy));
            fs::write(&path, "A=2\n").unwrap();
            record.saved = vec![place.clone()];
            let kept = rm_recorded(&mut record).unwrap();
            assert_eq!(kept_text(&record, &kept), "A=2\n");
            assert!(!place.exists(), "{}", place.display());
        }
    }

    /// The error is about something else: where the file went must not get lost with it.
    #[cfg(windows)]
    #[test]
    fn a_kept_file_is_told_also_when_its_directory_cannot_be_removed() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let t = tree();
        write(&t.main.join("conf/.env"), "A=1\n");
        let record = up(&t, &spec("conf/.env", FileMode::Copy));
        fs::write(t.wt.join("conf/.env"), "A=2\n").unwrap();
        let open = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(t.wt.join("conf"))
            .unwrap();
        let mut kept = Vec::new();
        remove(&record, &saved(&record), &mut kept, |_| Ok(())).unwrap_err();
        drop(open);
        assert_eq!(kept_text(&record, &kept), "A=2\n");
    }

    #[test]
    fn an_edited_file_that_cannot_be_kept_stays() {
        let t = tree();
        write(&t.main.join(".env"), "A=1\n");
        let mut record = up(&t, &spec(".env", FileMode::Copy));
        fs::write(t.wt.join(".env"), "A=2\n").unwrap();

        let full = |_: &Record| bail!("disk full");
        let err = remove(&record, &saved(&record), &mut Vec::new(), full).unwrap_err();
        assert!(format!("{err:#}").contains("disk full"), "{err:#}");
        assert_eq!(files_in(&t.wt.with_file_name("home")), 0);

        // No directory can be made there.
        let home = t.wt.with_file_name("home");
        write(&home.join("saved"), "in the way");
        for _ in 0..2 {
            rm_recorded(&mut record).unwrap_err();
            assert_eq!(record.saved.len(), 1, "one place is enough");
            assert_eq!(fs::read_to_string(t.wt.join(".env")).unwrap(), "A=2\n");
        }
        fs::remove_file(home.join("saved")).unwrap();
        let kept = rm_recorded(&mut record).unwrap();
        assert_eq!(kept_text(&record, &kept), "A=2\n");
        assert!(!t.wt.join(".env").exists());
    }

    /// Opened without sharing deletion, as many editors do, it can be read but not moved.
    #[cfg(windows)]
    #[test]
    fn an_edited_file_in_use_stays() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        let t = tree();
        write(&t.main.join(".env"), "A=1\n");
        let mut record = up(&t, &spec(".env", FileMode::Copy));
        fs::write(t.wt.join(".env"), "A=2\n").unwrap();
        let mut options = fs::OpenOptions::new();
        for share in [FILE_SHARE_READ, 0] {
            let open = options
                .read(true)
                .share_mode(share)
                .open(t.wt.join(".env"))
                .unwrap();
            let err = rm_recorded(&mut record).unwrap_err();
            assert!(format!("{err:#}").contains("in use by"), "{err:#}");
            drop(open);
            assert_eq!(fs::read_to_string(t.wt.join(".env")).unwrap(), "A=2\n");
        }
        let kept = rm_recorded(&mut record).unwrap();
        assert_eq!(kept_text(&record, &kept), "A=2\n");
    }

    #[test]
    fn record_round_trips_through_json() {
        let t = tree();
        write(&t.main.join("a/b"), "b");
        let mut s = spec("n/b", FileMode::Reflink);
        s.source = Some("a/b".into());
        let record = up(&t, &s);
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["type"], "file");
        assert_eq!(serde_json::from_value::<Record>(value).unwrap(), record);
        rm(&record).unwrap();
    }

    fn template(path: &str) -> FileSpec {
        spec(path, FileMode::Template)
    }

    #[test]
    fn template_substitutes_and_replaces() {
        let t = tree();
        write(
            &t.main.join(".env"),
            "\u{feff}URL=http://localhost:{{port.web}}\r\nPROXY=localhost:8000\r\n",
        );
        let mut s = template(".env");
        s.replace = vec![Replace {
            find: "localhost:8000".into(),
            with: "localhost:{{port.web}}".into(),
        }];
        let record = up(&t, &s);
        assert_eq!(
            fs::read_to_string(t.wt.join(".env")).unwrap(),
            "\u{feff}URL=http://localhost:8003\r\nPROXY=localhost:8003\r\n"
        );
        rm(&record).unwrap();
    }

    #[test]
    fn template_escapes_literal_braces() {
        let t = tree();
        write(
            &t.main.join("fmt.json"),
            "{\"format\":\"{{{{.Names}} {{port.web}}\",\"x\":{\"y\":{\"z\":{}}}}\n",
        );
        let mut s = template("fmt.json");
        s.replace = vec![Replace {
            find: "\"x\"".into(),
            with: "\"{{{{x}}\"".into(),
        }];
        up(&t, &s);
        assert_eq!(
            fs::read_to_string(t.wt.join("fmt.json")).unwrap(),
            "{\"format\":\"{{.Names}} 8003\",\"{{x}}\":{\"y\":{\"z\":{}}}}\n"
        );
    }

    #[test]
    fn template_without_vars_keeps_braces() {
        let t = tree();
        write(
            &t.main.join("view.blade.php"),
            "{{ $title }} at http://localhost:8000\n",
        );
        let mut s = template("view.blade.php");
        s.vars = false;
        s.replace = vec![Replace {
            find: "localhost:8000".into(),
            with: "localhost:{{port.web}}".into(),
        }];
        up(&t, &s);
        assert_eq!(
            fs::read_to_string(t.wt.join("view.blade.php")).unwrap(),
            "{{ $title }} at http://localhost:8003\n"
        );
    }

    #[test]
    fn template_filters_apply_in_the_source_and_in_with() {
        let t = tree();
        let mut v = vars();
        v.set("q", r#"C:\x "y""#);
        let render = |s: &FileSpec| {
            let plan = plan(s, &t.main, &t.wt, &v).unwrap().unwrap();
            create(&plan).unwrap();
            fs::read_to_string(t.wt.join(&s.path)).unwrap()
        };
        let rule = |find: &str, with: &str| Replace {
            find: find.into(),
            with: with.into(),
        };

        write(
            &t.main.join("mcp.json"),
            r#"{"a":"{{q | json}}","b":"OLD"}"#,
        );
        let mut s = template("mcp.json");
        s.replace = vec![rule("OLD", "{{q | json}}/src")];
        let parsed: serde_json::Value = serde_json::from_str(&render(&s)).unwrap();
        assert_eq!(parsed["a"], r#"C:\x "y""#);
        assert_eq!(parsed["b"], r#"C:\x "y"/src"#);

        // Without `vars` the source is left alone; `with` is still rendered. `find` is literal.
        write(
            &t.main.join("view.html"),
            "{{ q | json }} OLD {{q | slash}}",
        );
        let mut s = template("view.html");
        s.vars = false;
        s.replace = vec![
            rule("OLD", "{{q | json}}/src"),
            rule("{{q | slash}}", "{{port.web | json}}"),
        ];
        assert_eq!(render(&s), r#"{{ q | json }} C:\\x \"y\"/src 8003"#);
    }

    #[test]
    fn template_errors_are_found_before_anything_is_written() {
        let t = tree();
        write(&t.main.join("a.conf"), "x={{port.vite}}\n");
        write(&t.main.join("b.conf"), "url=localhost:9000\n");
        fs::write(t.main.join("c.bin"), [0xff, 0xfe, 0x00]).unwrap();
        fs::create_dir(t.main.join("dir")).unwrap();

        let mut drift = template("b.conf");
        drift.replace = vec![Replace {
            find: "localhost:8000".into(),
            with: "x".into(),
        }];
        let cases = [
            (template("a.conf"), "unknown variable `port.vite`"),
            (drift, "matches nothing"),
            (template("c.bin"), "not UTF-8"),
            (template("dir"), "must be a file"),
        ];
        for (s, expected) in cases {
            let err = plan(&s, &t.main, &t.wt, &vars()).unwrap_err();
            assert!(format!("{err:#}").contains(expected), "{err:#}");
            assert!(fs::symlink_metadata(t.wt.join(&s.path)).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn copies_keep_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let t = tree();
        write(&t.main.join("bin/run.sh"), "#!/bin/sh\n");
        write(&t.main.join("tpl.sh"), "#!/bin/sh\necho {{port.web}}\n");
        for p in ["bin/run.sh", "bin", "tpl.sh"] {
            fs::set_permissions(t.main.join(p), fs::Permissions::from_mode(0o750)).unwrap();
        }
        up(&t, &spec("bin", FileMode::Copy));
        up(&t, &template("tpl.sh"));
        for p in ["bin/run.sh", "bin", "tpl.sh"] {
            let mode = fs::metadata(t.wt.join(p)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o750, "{p}");
        }
    }

    #[test]
    fn removes_read_only_copies() {
        let t = tree();
        write(&t.main.join("objs/pack.idx"), "idx");
        write(&t.main.join("ro.txt"), "ro");
        for p in ["objs/pack.idx", "ro.txt"] {
            let mut perms = fs::metadata(t.main.join(p)).unwrap().permissions();
            perms.set_readonly(true);
            fs::set_permissions(t.main.join(p), perms).unwrap();
        }
        let dir = up(&t, &spec("objs", FileMode::Copy));
        let file = up(&t, &spec("ro.txt", FileMode::Copy));
        for r in rm_all(&[dir, file]) {
            r.unwrap();
        }
        assert!(!t.wt.join("objs").exists() && !t.wt.join("ro.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn removes_copies_of_read_only_directories() {
        use std::os::unix::fs::PermissionsExt;
        let t = tree();
        write(&t.main.join("modcache/pkg/a.go"), "package a");
        for p in ["modcache/pkg", "modcache"] {
            fs::set_permissions(t.main.join(p), fs::Permissions::from_mode(0o555)).unwrap();
        }
        let record = up(&t, &spec("modcache", FileMode::Copy));
        // As the source, but writable by the owner.
        for p in ["modcache/pkg", "modcache"] {
            let mode = fs::metadata(t.wt.join(p)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "{p}");
        }
        let result = rm(&record);
        // Let the temporary directory clean up the source either way.
        for p in ["modcache", "modcache/pkg"] {
            fs::set_permissions(t.main.join(p), fs::Permissions::from_mode(0o755)).unwrap();
        }
        result.unwrap();
        assert!(!t.wt.join("modcache").exists());
    }

    /// Something appearing at the destination between `plan` and `create` is never overwritten,
    /// written through, or later removed as ours.
    #[test]
    fn create_never_overwrites_what_appeared_after_plan() {
        let t = tree();
        write(&t.main.join("f.txt"), "from main");
        for mode in [FileMode::Copy, FileMode::Reflink, FileMode::Template] {
            let plan = plan(&spec("f.txt", mode), &t.main, &t.wt, &vars())
                .unwrap()
                .unwrap();
            write(&t.wt.join("f.txt"), "user data");
            assert!(create(&plan).is_err(), "{mode:?}");
            assert_eq!(fs::read_to_string(t.wt.join("f.txt")).unwrap(), "user data");
            fs::remove_file(t.wt.join("f.txt")).unwrap();
        }

        // A link planted there is not written through.
        if file_symlinks_allowed(&t) {
            write(&t.main.join("victim.txt"), "keep me");
            let plan = plan(&spec("f.txt", FileMode::Copy), &t.main, &t.wt, &vars())
                .unwrap()
                .unwrap();
            make_link(&t.main.join("victim.txt"), &t.wt.join("f.txt"), false).unwrap();
            assert!(create(&plan).is_err());
            assert_eq!(
                fs::read_to_string(t.main.join("victim.txt")).unwrap(),
                "keep me"
            );
        }
    }

    #[test]
    fn create_refuses_a_parent_replaced_by_a_link() {
        let t = tree();
        write(&t.main.join("x.txt"), "x");
        fs::create_dir(t.main.join("elsewhere")).unwrap();
        let mut s = spec("p/x.txt", FileMode::Copy);
        s.source = Some("x.txt".into());
        let plan = plan(&s, &t.main, &t.wt, &vars()).unwrap().unwrap();
        dir_link(&t.main.join("elsewhere"), &t.wt.join("p"));
        assert!(create(&plan).is_err());
        assert!(!t.main.join("elsewhere/x.txt").exists());
    }

    #[test]
    fn refuses_to_copy_a_directory_into_itself() {
        let t = tree();
        let root = t.main.join("work").join("wt");
        fs::create_dir_all(&root).unwrap();
        let mut s = spec("copy", FileMode::Copy);
        s.source = Some("work".into());
        let err = plan(&s, &t.main, &root, &vars()).unwrap_err();
        assert!(format!("{err:#}").contains("inside it"), "{err:#}");
    }

    #[test]
    fn reflink_of_an_empty_tree_claims_nothing() {
        let t = tree();
        fs::create_dir(t.main.join("empty")).unwrap();
        let record = up(&t, &spec("empty", FileMode::Reflink));
        assert_eq!(record.cloned, None);
    }
}
