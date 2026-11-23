//! File resources: git-ignored files carried over from the main checkout into a worktree.
//!
//! Creation is split in two so that the write-ahead ledger can record what is about to be
//! created: [`plan`] checks everything that can fail without touching the disk (the source
//! exists, the destination is free, a template renders) and yields a [`Record`]; [`create`] then
//! makes it. The record alone is enough to undo a creation, even an interrupted one.
//!
//! Teardown ([`remove`], [`remove_all`]) deletes exactly what a record describes, and only while
//! it still is what was created: a link is removed as a link and never followed, a path that has
//! changed type is left in place and reported, and nothing is reached through a link.

use std::fs::{self, File, Metadata};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{FileMode, FileSpec};
use crate::vars::Vars;

/// What a file resource is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryType {
    File,
    Dir,
    /// Symlink, or a junction for a directory on Windows.
    Link,
}

/// Ledger data of a file resource: everything teardown needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Worktree root. Nothing outside it is ever created or removed.
    pub root: PathBuf,
    /// Absolute destination.
    pub path: PathBuf,
    #[serde(rename = "type")]
    pub entry: EntryType,
    /// Directories created to hold `path`, outermost first; removed on teardown while empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_dirs: Vec<PathBuf>,
    /// Reflink mode only: whether every file was cloned copy-on-write rather than copied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloned: Option<bool>,
}

/// A checked file resource, ready to be created.
#[derive(Debug)]
pub struct Plan {
    pub record: Record,
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
pub fn plan(spec: &FileSpec, main: &Path, root: &Path, vars: &Vars) -> Result<Option<Plan>> {
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

    Ok(Some(Plan {
        record: Record {
            root: root.to_path_buf(),
            path,
            entry,
            created_dirs,
            cloned: None,
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
pub fn create(plan: &Plan) -> Result<Record> {
    let mut record = plan.record.clone();
    create_inner(plan, &mut record)
        .with_context(|| format!("cannot create {}", record.path.display()))?;
    Ok(record)
}

fn create_inner(plan: &Plan, record: &mut Record) -> Result<()> {
    for dir in &record.created_dirs {
        match fs::create_dir(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", dir.display())),
        }
    }
    let path = &record.path;
    match &plan.action {
        Action::Copy { source, reflink } => {
            let mut cloned = true;
            if record.entry == EntryType::Dir {
                fs::create_dir(path)?;
                copy_tree(source, path, *reflink, &mut cloned)?;
            } else {
                cloned = copy_file(source, path, *reflink)?;
            }
            record.cloned = reflink.then_some(cloned);
        }
        Action::Link { target, dir } => make_link(target, path, *dir)?,
        Action::Write { bytes, source } => {
            let mut file = File::create_new(path)?;
            file.write_all(bytes)?;
            #[cfg(unix)]
            file.set_permissions(fs::metadata(source)?.permissions())?;
            #[cfg(not(unix))]
            let _ = source;
        }
    }
    Ok(())
}

/// Copies one file; returns whether it was cloned copy-on-write.
fn copy_file(source: &Path, dest: &Path, reflink: bool) -> Result<bool> {
    let cloned = if reflink {
        reflink_copy::reflink_or_copy(source, dest)?.is_none()
    } else {
        fs::copy(source, dest)?;
        false
    };
    Ok(cloned)
}

/// Copies the contents of `source` into the existing, empty `dest`. Links inside the tree are
/// recreated as links to the same target, never followed.
fn copy_tree(source: &Path, dest: &Path, reflink: bool, cloned: &mut bool) -> Result<()> {
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
            fs::create_dir(&to)?;
            copy_tree(&from, &to, reflink, cloned)?;
        } else if file_type.is_file() {
            let this = copy_file(&from, &to, reflink)
                .with_context(|| format!("cannot copy {}", from.display()))?;
            *cloned &= this;
        } else {
            bail!("{} is not a file, directory or link", from.display());
        }
    }
    // Last, so that a read-only source directory does not block writing its contents.
    #[cfg(unix)]
    fs::set_permissions(dest, fs::metadata(source)?.permissions())?;
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
pub fn remove(record: &Record) -> Result<()> {
    remove_inner(record).with_context(|| format!("cannot remove {}", record.path.display()))
}

fn remove_inner(record: &Record) -> Result<()> {
    let path = &record.path;
    let inside = |p: &Path| p.starts_with(&record.root) && p != record.root;
    if !inside(path) || !record.created_dirs.iter().all(|d| inside(d)) {
        bail!("refusing to touch a path outside {}", record.root.display());
    }

    if parents_are_real_dirs(&record.root, path)? {
        match fs::symlink_metadata(path) {
            Ok(meta) => remove_entry(path, record.entry, &meta)?,
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
        match fs::remove_dir(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            // Holds something we did not create, and so do its parents.
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => break,
            Err(e) => return Err(e).with_context(|| format!("cannot remove {}", dir.display())),
        }
    }
    Ok(())
}

fn remove_entry(path: &Path, expected: EntryType, meta: &Metadata) -> Result<()> {
    let file_type = meta.file_type();
    // `is_dir` and `is_file` are false for links, junctions included.
    let matches = match expected {
        EntryType::Link => file_type.is_symlink(),
        EntryType::File => file_type.is_file(),
        EntryType::Dir => file_type.is_dir(),
    };
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
        EntryType::Link => remove_link(path, meta)?,
        EntryType::File => fs::remove_file(path)?,
        // Does not follow links inside the tree: they are removed as links.
        EntryType::Dir => fs::remove_dir_all(path)?,
    }
    Ok(())
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

/// Removes several resources: every link first, then the rest, each group in reverse order of
/// creation. Removing links first means no recursive delete can ever reach into the main
/// checkout through one. Results are returned in the order of `records`; one failure does not
/// stop the others.
pub fn remove_all(records: &[Record]) -> Vec<Result<()>> {
    let mut results: Vec<Option<Result<()>>> = records.iter().map(|_| None).collect();
    let links = records
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, r)| r.entry == EntryType::Link);
    let others = records
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, r)| r.entry != EntryType::Link);
    for (i, record) in links.chain(others) {
        results[i] = Some(remove(record));
    }
    results
        .into_iter()
        .map(|r| r.expect("every record visited"))
        .collect()
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

        remove(&record).unwrap();
        assert!(!t.wt.join(".env").exists());
        // Idempotent.
        remove(&record).unwrap();
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
        remove(&record).unwrap();
        assert!(!t.wt.join("deep").exists());
        assert!(t.wt.exists());
    }

    #[test]
    fn created_parents_holding_other_files_are_kept() {
        let t = tree();
        write(&t.main.join("a/b.txt"), "b");
        let record = up(&t, &spec("a/b.txt", FileMode::Copy));
        write(&t.wt.join("a/mine.txt"), "user data");
        remove(&record).unwrap();
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
        for r in remove_all(&[file, dir]) {
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
        remove(&link).unwrap();
    }

    #[test]
    fn a_path_that_changed_type_is_left_in_place() {
        let t = tree();
        write(&t.main.join("f"), "f");
        let record = up(&t, &spec("f", FileMode::Copy));
        fs::remove_file(t.wt.join("f")).unwrap();
        write(&t.wt.join("f/user.txt"), "user data");
        let err = remove(&record).unwrap_err();
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

        remove(&record).unwrap();
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
        remove(&record).unwrap();
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

        for r in remove_all(&[app.clone(), vendor]) {
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
        remove(&app).unwrap();
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

        let err = remove(&record).unwrap_err();
        assert!(format!("{err:#}").contains("left in place"), "{err:#}");
        assert!(t.main.join("other/y/z.txt").exists());
    }

    #[test]
    fn an_interrupted_creation_is_undone_from_the_plan() {
        let t = tree();
        write(&t.main.join("d/a.txt"), "a");
        let mut s = spec("p/d", FileMode::Copy);
        s.source = Some("d".into());
        let plan = plan(&s, &t.main, &t.wt, &vars()).unwrap().unwrap();
        // Crash halfway: parents and part of the tree exist, `create` never returned.
        write(&t.wt.join("p/d/a.txt"), "half");
        remove(&plan.record).unwrap();
        assert!(!t.wt.join("p").exists());

        // Crash before anything was made.
        remove(&plan.record).unwrap();
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
        remove(&record).unwrap();
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
        remove(&record).unwrap();
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
        for p in ["bin/run.sh", "tpl.sh"] {
            fs::set_permissions(t.main.join(p), fs::Permissions::from_mode(0o750)).unwrap();
        }
        up(&t, &spec("bin", FileMode::Copy));
        up(&t, &template("tpl.sh"));
        for p in ["bin/run.sh", "tpl.sh"] {
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
        for r in remove_all(&[dir, file]) {
            r.unwrap();
        }
        assert!(!t.wt.join("objs").exists() && !t.wt.join("ro.txt").exists());
    }
}
