//! A copy edited in the worktree, from `bp up` to `bp down`: it is kept, and nothing about it leaks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Short enough to be guessed from its hash.
const SECRET: &str = "PIN=0000\n";
const EDITED: &str = "PIN=1234\n";
const CONFIG: &str = "version = 1\n\
    [[file]]\npath = \"conf/.env\"\nmode = \"copy\"\n\
    [[file]]\npath = \"notes.txt\"\nmode = \"copy\"\n\
    [[file]]\npath = \"url.txt\"\nmode = \"template\"\n";

struct Setup {
    home: PathBuf,
    worktree: PathBuf,
}

/// A main checkout and a linked worktree in `base`, with the files of [`CONFIG`]. Non-ASCII names.
fn setup(base: &Path, home: &Path) -> Setup {
    let base = dunce::canonicalize(base).unwrap();
    let main = base.join("main-caf\u{e9}");
    let worktree = base.join("feature-na\u{ef}ve");
    fs::create_dir_all(main.join("conf")).unwrap();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    let path = worktree.to_str().unwrap();
    git(&["worktree", "add", "-q", "-b", "feature", path]);
    fs::write(main.join("conf/.env"), SECRET).unwrap();
    fs::write(main.join("notes.txt"), "notes").unwrap();
    fs::write(main.join("url.txt"), "pod {{pod.id}}").unwrap();
    fs::write(main.join("branchpod.local.toml"), CONFIG).unwrap();
    Setup {
        // As `bp` spells it: the build directory may be given with `/` on Windows.
        home: dunce::canonicalize(home).unwrap(),
        worktree,
    }
}

impl Setup {
    /// Exit code, stdout, stderr.
    fn bp(&self, args: &[&str]) -> (i32, String, String) {
        self.run(Command::new(env!("CARGO_BIN_EXE_branchpod")).args(args))
    }

    fn run(&self, command: &mut Command) -> (i32, String, String) {
        let out = command
            .arg(&self.worktree)
            .env("BRANCHPOD_HOME", &self.home)
            .output()
            .unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn up(&self) {
        let (code, stdout, stderr) = self.bp(&["up"]);
        assert_eq!(code, 0, "{stdout}{stderr}");
    }

    fn edit(&self) {
        fs::write(self.worktree.join("conf/.env"), EDITED).unwrap();
    }

    /// Every file that was kept.
    fn kept(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).into_iter().flatten() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, found);
                } else {
                    found.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(&self.home.join("saved"), &mut found);
        found
    }

    /// `bp down` ends with 0, keeps the edited file and says where it is.
    fn assert_down_keeps_the_edit(&self) -> String {
        let (code, stdout, stderr) = self.bp(&["down"]);
        assert_eq!(code, 0, "{stdout}{stderr}");
        assert!(stdout.ends_with("clean\n"), "{stdout}");
        assert!(
            stdout.contains("saved     file       conf/.env"),
            "{stdout}"
        );
        let kept = self.kept();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(fs::read_to_string(&kept[0]).unwrap(), EDITED);
        assert!(
            stderr.contains(&format!("kept as {}", kept[0].display())),
            "{stderr}"
        );
        assert!(!self.worktree.join("conf").exists());
        assert!(!stdout.contains("1234") && !stderr.contains("1234"));
        stdout
    }
}

#[test]
fn an_edited_copy_is_kept_and_the_pod_torn_down() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let s = setup(tmp.path(), home.path());
    s.up();
    s.edit();

    let (_, shown, _) = s.bp(&["status", "--json"]);
    assert!(shown.contains(".env"), "{shown}");
    for leak in ["sha256", "fingerprint", "1234", "0000\n"] {
        assert!(!shown.contains(leak), "{shown}");
    }
    let stdout = s.assert_down_keeps_the_edit();
    for unchanged in ["notes.txt", "url.txt"] {
        let line = format!("destroyed file       {unchanged}");
        assert!(stdout.contains(&line), "{stdout}");
    }

    // Nothing recorded any more, and the next pod starts afresh.
    let (code, stdout, _) = s.bp(&["down"]);
    assert!(code == 0 && stdout.contains("no pod"), "{stdout}");
    s.up();
    let (code, stdout, stderr) = s.bp(&["down"]);
    assert_eq!((code, stderr.as_str()), (0, ""), "{stdout}");
    assert_eq!(s.kept().len(), 1, "the first edit is still there");
}

#[test]
fn unchanged_copies_are_deleted_without_a_word() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let s = setup(tmp.path(), home.path());
    s.up();
    let (code, stdout, stderr) = s.bp(&["down"]);
    assert_eq!((code, stderr.as_str()), (0, ""), "{stdout}");
    assert!(!home.path().join("saved").exists());
    for file in ["conf", "notes.txt", "url.txt"] {
        assert!(!s.worktree.join(file).exists(), "{file}");
    }
}

#[test]
fn a_copy_that_cannot_be_kept_means_leftovers() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let s = setup(tmp.path(), home.path());
    s.up();
    s.edit();
    let saved = home.path().join("saved");
    fs::write(&saved, "in the way").unwrap();
    for _ in 0..2 {
        let (code, stdout, stderr) = s.bp(&["down"]);
        assert_eq!(code, 3, "{stdout}{stderr}");
        let text = fs::read_to_string(s.worktree.join("conf/.env")).unwrap();
        assert_eq!(text, EDITED);
    }
    fs::remove_file(&saved).unwrap();
    s.assert_down_keeps_the_edit();
}

/// A directory on another volume than the temporary one, where the machine has one: shared memory
/// on Linux, the build directory of a runner that keeps it on another drive.
fn elsewhere() -> tempfile::TempDir {
    if cfg!(target_os = "linux")
        && let Ok(dir) = tempfile::tempdir_in("/dev/shm")
    {
        return dir;
    }
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

#[test]
fn an_edited_copy_is_kept_on_another_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let home = elsewhere();
    let s = setup(tmp.path(), home.path());
    s.up();
    s.edit();
    s.assert_down_keeps_the_edit();
}

/// Writing the copy fails halfway, as on a full disk: here, at the limit for the size of a file.
#[cfg(target_os = "linux")]
#[test]
fn a_copy_that_does_not_fit_means_leftovers() {
    let tmp = tempfile::tempdir().unwrap();
    let home = elsewhere();
    let device = |p: &Path| std::os::unix::fs::MetadataExt::dev(&fs::metadata(p).unwrap());
    if device(tmp.path()) == device(home.path()) {
        eprintln!("skipped: there is no other volume");
        return;
    }
    let s = setup(tmp.path(), home.path());
    s.up();
    let big = "x".repeat(1 << 20);
    fs::write(s.worktree.join("conf/.env"), &big).unwrap();

    // 64 to 128 KiB, depending on the shell's unit: more than the ledger, less than the file.
    let mut limited = Command::new("sh");
    let script = "trap '' XFSZ; ulimit -f 128; exec \"$0\" \"$@\"";
    limited.args(["-c", script, env!("CARGO_BIN_EXE_branchpod"), "down"]);
    let (code, stdout, stderr) = s.run(&mut limited);
    assert_eq!(code, 3, "{stdout}{stderr}");
    assert!(stderr.contains("cannot copy it to"), "{stderr}");
    let text = fs::read_to_string(s.worktree.join("conf/.env")).unwrap();
    assert!(text == big, "the edited file is whole");
    assert_eq!(
        s.kept(),
        Vec::<PathBuf>::new(),
        "nothing unfinished is left"
    );

    let (code, stdout, stderr) = s.bp(&["down"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    let kept = s.kept();
    assert_eq!(kept.len(), 1);
    assert!(fs::read_to_string(&kept[0]).unwrap() == big);
}
