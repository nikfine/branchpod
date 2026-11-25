//! `bp -V` and `--version`, and `build.rs`: what it finds out about the commit and when it runs
//! again. The latter is tested on a tiny package without dependencies that has a copy of it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[test]
fn short_version_is_bare_and_long_one_may_have_the_commit() {
    let bp = |flag: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_branchpod"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let bare = concat!("bp ", env!("CARGO_PKG_VERSION"));
    assert_eq!(bp("-V"), format!("{bare}\n"));

    let long = bp("--version");
    assert_eq!(long, concat!("bp ", env!("BRANCHPOD_LONG_VERSION"), "\n"));
    // Whether these sources are in a repository is not known here.
    let rest = long.strip_prefix(bare).unwrap().trim_end();
    if !rest.is_empty() {
        let commit = rest.strip_prefix(" (").unwrap().strip_suffix(')').unwrap();
        let hash = commit.strip_suffix("-dirty").unwrap_or(commit);
        assert_eq!(hash.len(), 9, "{long}");
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()), "{long}");
    }
}

#[test]
fn without_a_repository_of_its_own_the_version_is_bare() {
    let sandbox = Sandbox::new();
    let package = sandbox.package("outer/package");
    let first = package.build();
    assert_eq!(first.version, "1.2.3");
    assert!(first.compiled);
    assert!(!package.build().compiled, "built again for nothing");

    // A repository around the sources is not theirs.
    let outer = sandbox.dir("outer");
    sandbox.git(&outer, &["init", "-q", "--template="]);
    sandbox.git(&outer, &["add", "."]);
    sandbox.git(&outer, &["commit", "-q", "-m", "outer"]);
    package.edit();
    assert_eq!(package.version(), "1.2.3");
}

#[test]
fn sources_outside_the_work_tree_are_not_of_that_repository() {
    let sandbox = Sandbox::new();
    let outer = sandbox.repository("outer");
    let package = sandbox.package("outer/.git/package");
    assert_eq!(package.version(), "1.2.3");

    let elsewhere = sandbox.dir("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    outer.git(&["config", "core.worktree", elsewhere.to_str().unwrap()]);
    assert_eq!(outer.version(), "1.2.3");
}

#[test]
fn a_repository_of_another_owner_takes_safe_directory() {
    let sandbox = Sandbox::new();
    let package = sandbox.repository("package");
    let cargo = || {
        let mut cargo = package.cargo();
        // What the tests of git itself use.
        cargo.env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1");
        cargo
    };
    assert_eq!(read_build(cargo()).version, "1.2.3");

    package.edit();
    package.git(&["commit", "-q", "-a", "-m", "edit"]);
    let mut trusting = cargo();
    trusting
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "safe.directory")
        .env("GIT_CONFIG_VALUE_0", "*");
    let expected = format!("1.2.3 ({})", &package.head()[..9]);
    assert_eq!(read_build(trusting).version, expected);
}

#[test]
fn a_repository_without_commits_has_no_commit_to_show() {
    let sandbox = Sandbox::new();
    let package = sandbox.package("package");
    sandbox.git(&package.dir, &["init", "-q", "--template="]);
    assert_eq!(package.version(), "1.2.3");
}

#[test]
fn the_commit_follows_head() {
    let sandbox = Sandbox::new();
    let package = sandbox.repository("package");
    package.assert_commit("");
    assert!(!package.build().compiled, "built again for nothing");

    package.commit();
    package.assert_commit("");

    // Both the packed ref and the first commit after it, which makes a loose one again.
    package.git(&["pack-refs", "--all"]);
    package.assert_commit("");
    assert!(!package.build().compiled, "built again for nothing");
    package.commit();
    package.assert_commit("");

    // Committed and packed since the last build: the loose ref is gone again.
    package.git(&["pack-refs", "--all"]);
    package.assert_commit("");
    package.commit();
    package.git(&["pack-refs", "--all"]);
    package.assert_commit("");

    // A branch in a directory that packing the refs removes.
    package.git(&["checkout", "-q", "-b", "feature/x"]);
    package.commit();
    package.assert_commit("");
    package.git(&["pack-refs", "--all"]);
    package.assert_commit("");
    package.commit();
    package.assert_commit("");

    package.git(&["checkout", "-q", "--detach", "main"]);
    package.assert_commit("");
    package.commit();
    package.assert_commit("");
}

#[test]
fn changed_sources_are_marked() {
    let sandbox = Sandbox::new();
    let package = sandbox.repository("package");
    package.assert_commit("");

    // Not something the binary is built from.
    fs::write(package.dir.join("README.md"), "changed").unwrap();
    fs::write(package.dir.join("new.txt"), "new").unwrap();
    assert!(!package.build().compiled, "built again for nothing");
    package.assert_commit("");

    package.edit();
    package.assert_commit("-dirty");
    package.git(&["checkout", "-q", "--", "src"]);
    package.assert_commit("");

    // Not tracked yet, but a part of the build, whatever `git status` shows by default.
    package.git(&["config", "status.showUntrackedFiles", "no"]);
    let module = package.dir.join("src").join("new.rs");
    fs::write(&module, "").unwrap();
    package.assert_commit("-dirty");
    fs::remove_file(module).unwrap();
    package.assert_commit("");

    package.edit();
    package.git(&["commit", "-q", "-a", "-m", "edit"]);
    package.assert_commit("");

    // `git status` fails, so whether there are changes is not known.
    package.edit();
    package.git(&["commit", "-q", "-a", "-m", "edit"]);
    package.git(&["config", "status.showUntrackedFiles", "bogus"]);
    package.assert_commit("-dirty");
}

#[test]
fn a_linked_worktree_shows_its_own_commit() {
    let sandbox = Sandbox::new();
    let main = sandbox.repository("package");
    let linked = Package {
        sandbox: &sandbox,
        dir: sandbox.root.join("linked w\u{f6}rktree"),
    };
    let path = linked.dir.to_str().unwrap();
    main.git(&["worktree", "add", "-q", "-b", "linked", path]);
    linked.assert_commit("");
    linked.commit();
    assert_ne!(linked.head(), main.head());
    linked.assert_commit("");
    assert!(!linked.build().compiled, "built again for nothing");

    // Both have loose refs, so commits in one do not concern the other.
    main.commit();
    assert!(!linked.build().compiled, "built again for nothing");

    main.git(&["pack-refs", "--all"]);
    linked.assert_commit("");
    linked.commit();
    linked.assert_commit("");

    // Only its own HEAD changes.
    linked.git(&["checkout", "-q", "--detach", "HEAD~1"]);
    linked.assert_commit("");
}

#[test]
fn variables_of_a_git_hook_do_not_lead_elsewhere() {
    let sandbox = Sandbox::new();
    let other = sandbox.repository("other");
    other.commit();
    let package = sandbox.repository("package");
    package.git(&[
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "not in the other one",
    ]);
    let git_dir = other.dir.join(".git");
    let mut cargo = package.cargo();
    cargo
        .env("GIT_DIR", &git_dir)
        .env("GIT_WORK_TREE", &other.dir)
        .env("GIT_INDEX_FILE", git_dir.join("index"))
        .env("GIT_PREFIX", "src/");
    let expected = format!("1.2.3 ({})", &package.head()[..9]);
    assert_eq!(read_build(cargo).version, expected);
    assert_ne!(package.head(), other.head());
}

#[test]
fn refs_in_the_reftable_format() {
    let sandbox = Sandbox::new();
    let package = sandbox.package("package");
    // Needs git 2.45.
    let init = ["init", "-q", "-b", "main", "--ref-format=reftable"];
    if !sandbox
        .command("git", &package.dir)
        .args(init)
        .output()
        .unwrap()
        .status
        .success()
    {
        eprintln!("skipped: this git has no reftable");
        return;
    }
    package.git(&["add", "."]);
    package.commit();
    package.assert_commit("");
    assert!(!package.build().compiled, "built again for nothing");
    package.commit();
    package.assert_commit("");

    let linked = Package {
        sandbox: &sandbox,
        dir: sandbox.root.join("linked"),
    };
    let path = linked.dir.to_str().unwrap();
    package.git(&["worktree", "add", "-q", "-b", "linked", path]);
    linked.assert_commit("");
    linked.commit();
    linked.assert_commit("");
    linked.git(&["checkout", "-q", "--detach", "main"]);
    linked.assert_commit("");
}

/// A temporary directory, its name with a space and beyond ASCII, where neither git nor cargo sees
/// the git configuration of the machine or a repository above.
struct Sandbox {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    /// What makes git look at a certain repository, in case the tests run in a git hook.
    local_env: Vec<String>,
}

impl Sandbox {
    fn new() -> Sandbox {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path())
            .unwrap()
            .join("caf\u{e9} \u{436}");
        fs::create_dir_all(root.join("config")).unwrap();
        fs::write(root.join("gitconfig"), "").unwrap();
        let names = Command::new("git")
            .args(["rev-parse", "--local-env-vars"])
            .output()
            .unwrap();
        let local_env = stdout(names).lines().map(str::to_owned).collect();
        Sandbox {
            _tmp: tmp,
            root,
            local_env,
        }
    }

    fn dir(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    fn command(&self, program: &str, dir: &Path) -> Command {
        let mut command = Command::new(program);
        for name in &self.local_env {
            command.env_remove(name);
        }
        command
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.root.join("gitconfig"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("GIT_CEILING_DIRECTORIES", &self.root);
        command
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self
            .command("git", dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .output()
            .unwrap();
        stdout(out)
    }

    /// A package that prints the version `build.rs` gave it.
    fn package(&self, path: &str) -> Package<'_> {
        let dir = self.dir(path);
        fs::create_dir_all(dir.join("src")).unwrap();
        let build_script = Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs");
        fs::copy(build_script, dir.join("build.rs")).unwrap();
        let manifest = "[package]\nname = \"probe\"\nversion = \"1.2.3\"\nedition = \"2024\"\n\n\
            [workspace]\n";
        let lock = "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"1.2.3\"\n";
        let main = "fn main() {\n    println!(\"{}\", env!(\"BRANCHPOD_LONG_VERSION\"));\n}\n";
        fs::write(dir.join("Cargo.toml"), manifest).unwrap();
        fs::write(dir.join("Cargo.lock"), lock).unwrap();
        fs::write(dir.join("src").join("main.rs"), main).unwrap();
        fs::write(dir.join("README.md"), "probe\n").unwrap();
        fs::write(dir.join(".gitignore"), "/target\n").unwrap();
        fs::write(dir.join(".gitattributes"), "* text=auto eol=lf\n").unwrap();
        Package { sandbox: self, dir }
    }

    /// A package that is committed to a repository of its own.
    fn repository(&self, path: &str) -> Package<'_> {
        let package = self.package(path);
        package.git(&["init", "-q", "-b", "main", "--template="]);
        package.git(&["add", "."]);
        package.commit();
        package
    }
}

struct Package<'a> {
    sandbox: &'a Sandbox,
    dir: PathBuf,
}

struct Build {
    version: String,
    /// Whether anything was compiled; it is when the build script has run again.
    compiled: bool,
}

impl Package<'_> {
    fn git(&self, args: &[&str]) -> String {
        self.sandbox.git(&self.dir, args)
    }

    fn commit(&self) {
        self.git(&["commit", "-q", "--allow-empty", "-m", "commit"]);
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// Appends a comment to a source file.
    fn edit(&self) {
        let path = self.dir.join("src").join("main.rs");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("{text}// edited\n")).unwrap();
    }

    fn cargo(&self) -> Command {
        let cargo = std::env::var_os("CARGO").expect("set by `cargo test`");
        let mut command = self.sandbox.command(cargo.to_str().unwrap(), &self.dir);
        command
            .args(["run", "--offline", "--locked", "--color", "never"])
            .arg("--target-dir")
            .arg(self.dir.join("target"))
            .env_remove("CARGO_BUILD_TARGET")
            // `compiled` is read from what cargo says.
            .env_remove("CARGO_TERM_QUIET");
        command
    }

    fn build(&self) -> Build {
        read_build(self.cargo())
    }

    fn version(&self) -> String {
        self.build().version
    }

    /// The version has the commit of HEAD, followed by `mark`.
    #[track_caller]
    fn assert_commit(&self, mark: &str) {
        let expected = format!("1.2.3 ({}{mark})", &self.head()[..9]);
        assert_eq!(self.version(), expected);
    }
}

fn read_build(mut cargo: Command) -> Build {
    let out = cargo.output().unwrap();
    let compiled = String::from_utf8_lossy(&out.stderr).contains("Compiling");
    Build {
        version: stdout(out),
        compiled,
    }
}

#[track_caller]
fn stdout(out: Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap().trim_end().to_owned()
}
