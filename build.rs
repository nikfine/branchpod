//! Puts the commit into `bp --version`: `0.1.0 (abc123456)`, with `-dirty` after the commit when the
//! sources differ from it. Without git, or when anything about it fails, it is the bare version: this
//! script never fails because of git.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What the binary is built from, and so what can make it differ from the commit. Also what reruns
/// this script: other files neither mark the version nor cause a rebuild. A `.cargo/config.toml`
/// belongs here once the repository has one.
const SOURCES: [&str; 4] = ["src", "Cargo.toml", "Cargo.lock", "build.rs"];

/// Cut here rather than by `git rev-parse --short`, whose length depends on the repository and its
/// configuration.
const COMMIT_LENGTH: usize = 9;

fn main() {
    for source in SOURCES {
        // Cargo takes a missing path for a changed one and would rerun the script on every build.
        if Path::new(source).exists() {
            println!("cargo:rerun-if-changed={source}");
        }
    }
    let version = env::var("CARGO_PKG_VERSION").expect("cargo sets it");
    let version = match commit() {
        Some(commit) => format!("{version} ({commit})"),
        None => version,
    };
    // Always set, so that a variable of the same name in the caller's environment is never used.
    println!("cargo:rustc-env=BRANCHPOD_LONG_VERSION={version}");
}

/// The commit for the version, `None` when it is not known or its changes could not be noticed.
fn commit() -> Option<String> {
    let git = Git::new()?;
    let found = git.run(&[
        "rev-parse",
        "--is-inside-work-tree",
        "--show-prefix",
        "--git-dir",
        "--git-common-dir",
        "HEAD",
    ])?;
    let [inside, prefix, git_dir, common_dir, head] = found.split('\n').collect::<Vec<_>>()[..]
    else {
        return None;
    };
    // Not the top of a work tree: sources unpacked inside an unrelated one, or in its `.git`, where
    // the prefix is empty, too.
    if inside != "true" || !prefix.is_empty() {
        return None;
    }
    let watched = watched(&git, Path::new(git_dir), Path::new(common_dir))?;
    for path in watched {
        // Text, as it comes from UTF-8 output.
        println!("cargo:rerun-if-changed={}", path.to_str()?);
    }

    let mut status = vec!["status", "--porcelain", "--untracked-files=all", "--"];
    status.extend(SOURCES);
    let clean = git.run(&status).is_some_and(|changes| changes.is_empty());
    let commit = head.get(..COMMIT_LENGTH)?;
    Some(format!("{commit}{}", if clean { "" } else { "-dirty" }))
}

/// The existing paths that change whenever HEAD gets another commit.
fn watched(git: &Git, git_dir: &Path, common_dir: &Path) -> Option<Vec<PathBuf>> {
    let mut paths = vec![git_dir.join("HEAD")];
    // Refs kept in the reftable format; a linked worktree has a HEAD of its own.
    let tables = [git_dir.join("reftable"), common_dir.join("reftable")];
    if tables[1].exists() {
        paths.extend(tables.into_iter().filter(|path| path.exists()));
    } else if let Some(branch) = git.run(&["symbolic-ref", "-q", "HEAD"]) {
        let loose = common_dir.join(branch);
        if loose.exists() {
            paths.push(loose);
        } else {
            // A packed ref. The next commit creates the loose one, in the deepest directory there
            // is by now. Cargo looks at everything inside and at the directories themselves, so it
            // notices a loose ref that is packed again before the next build, too.
            paths.push(loose.ancestors().find(|path| path.exists())?.to_owned());
        }
    }
    // Such as a path of another system, printed by a git that is not native.
    paths.iter().all(|path| path.exists()).then_some(paths)
}

struct Git {
    /// Variables that make git look at a certain repository: set when cargo runs in a git hook.
    local_env: Vec<String>,
}

impl Git {
    fn new() -> Option<Git> {
        let mut git = Git { local_env: vec![] };
        let names = git.run(&["rev-parse", "--local-env-vars"])?;
        git.local_env = names
            .lines()
            // Configuration may be what makes the repository readable at all (`safe.directory`).
            .filter(|name| !name.starts_with("GIT_CONFIG"))
            .map(str::to_owned)
            .collect();
        Some(git)
    }

    /// The output without its last line break; `None` when git fails or prints something that is
    /// not UTF-8, which cargo could not be given either.
    fn run(&self, args: &[&str]) -> Option<String> {
        let mut command = Command::new("git");
        for name in &self.local_env {
            command.env_remove(name);
        }
        // Neither the index is written nor a daemon started.
        let output = command
            .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        Some(text.strip_suffix('\n').unwrap_or(&text).to_owned())
    }
}
