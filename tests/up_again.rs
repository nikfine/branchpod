//! `bp up` on a pod that is there already, as a user or a hook runs it.

use std::fs;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git").arg("-C").arg(dir).args(args).status();
    assert!(status.unwrap().success(), "git {args:?}");
}

fn repository(path: &Path) {
    fs::create_dir(path).unwrap();
    git(path, &["init", "-q"]);
    let commit = [
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.invalid",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "init",
    ];
    git(path, &commit);
}

/// Git sets `GIT_DIR` for its hooks, which need not be about the worktree that `bp` is called for.
#[test]
fn a_git_hook_finds_the_same_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let (main, other) = (base.join("main-\u{e9}"), base.join("other"));
    repository(&main);
    repository(&other);
    let worktree = base.join("wt");
    git(
        &main,
        &["worktree", "add", "-q", worktree.to_str().unwrap()],
    );
    let config = "version = 1\n[port.web]\nrange = [41000, 41999]\n";
    fs::write(worktree.join("branchpod.toml"), config).unwrap();

    let run = |command: &str, hook: bool| {
        let mut bp = Command::new(env!("CARGO_BIN_EXE_branchpod"));
        bp.arg(command).arg(&worktree);
        bp.env("BRANCHPOD_HOME", base.join("home"));
        if hook {
            bp.env("GIT_DIR", other.join(".git"));
        }
        let out = bp.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        text
    };
    for hook_first in [true, false] {
        assert!(run("up", hook_first).contains(" is up ("));
        assert!(!other.join(".git/.branchpod").exists());
        assert!(run("up", !hook_first).contains(" is already up ("));
        assert!(run("down", hook_first).contains("clean"));
        assert!(!main.join(".git/worktrees/wt/.branchpod").exists());
    }

    // One ledger for each time the pod was there, with nothing of the other.
    let history = fs::read_dir(base.join("home/history")).unwrap();
    let ledgers: Vec<String> = history
        .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    assert_eq!(ledgers.len(), 2);
    for ledger in ledgers {
        for op in ["intent", "created", "destroyed"] {
            let op = format!(r#""op":"{op}""#);
            assert_eq!(ledger.matches(&op).count(), 1, "{ledger}");
        }
    }
}
