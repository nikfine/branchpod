//! `bp down` as a hook runs it: exit codes and a worktree that is already gone.

use std::fs;
use std::path::Path;
use std::process::Command;

fn bp(home: &Path, args: &[&std::ffi::OsStr]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_branchpod"))
        .args(args)
        .env("BRANCHPOD_HOME", home)
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Writes a pod for `worktree` with the given ledger lines.
fn pod(home: &Path, worktree: &Path, ledger: &[serde_json::Value]) -> std::path::PathBuf {
    let dir = home.join("pods").join("wt-00000000");
    fs::create_dir_all(&dir).unwrap();
    let meta = serde_json::json!({ "v": 1, "id": "wt-00000000", "worktree": worktree });
    fs::write(dir.join("pod.json"), meta.to_string()).unwrap();
    let lines: Vec<String> = ledger.iter().map(|l| format!("{l}\n")).collect();
    fs::write(dir.join("ledger.jsonl"), lines.concat()).unwrap();
    dir
}

fn entry(kind: &str, name: &str, op: &str, data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "v": 1, "ts_ms": 1, "kind": kind, "name": name, "op": op, "data": data })
}

#[test]
fn exit_code_tells_clean_from_leftovers() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    // Never created: the worktree is gone, the pod is found by its recorded path.
    let worktree = tmp.path().join("gone");
    let file =
        serde_json::json!({ "root": worktree, "path": worktree.join(".env"), "type": "file" });
    let dir = pod(
        &home,
        &worktree,
        &[
            entry("file", ".env", "intent", file),
            entry("service", "vite", "intent", serde_json::Value::Null),
        ],
    );

    let (code, stdout, stderr) = bp(&home, &["down".as_ref(), worktree.as_os_str()]);
    assert_eq!(code, 3, "{stdout}{stderr}");
    assert!(stderr.contains("service vite"), "{stderr}");
    assert!(dir.join("pod.json").exists());

    // Once the leftover is gone (here: by hand), `down` finishes and removes the pod.
    let destroyed = entry("service", "vite", "destroyed", serde_json::Value::Null);
    let mut ledger = fs::read_to_string(dir.join("ledger.jsonl")).unwrap();
    ledger.push_str(&format!("{destroyed}\n"));
    fs::write(dir.join("ledger.jsonl"), ledger).unwrap();
    let (code, stdout, stderr) = bp(&home, &["down".as_ref(), worktree.as_os_str()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("clean"), "{stdout}");
    assert!(!dir.exists());

    // Nothing recorded any more: still a success.
    let (code, stdout, _) = bp(&home, &["down".as_ref(), worktree.as_os_str()]);
    assert_eq!(code, 0);
    assert!(stdout.contains("no pod"), "{stdout}");
}

/// The pod records the canonical path; a hook may spell it with `..`, an 8.3 name (Windows
/// runners' temp directory) or `/var` for `/private/var` (macOS).
#[test]
fn a_gone_worktree_is_found_however_its_path_is_spelled() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir(tmp.path().join("real")).unwrap();
    let canonical = dunce::canonicalize(tmp.path()).unwrap().join("gone");
    let dir = pod(
        &home,
        &canonical,
        &[entry("service", "vite", "intent", serde_json::Value::Null)],
    );
    for spelled in [
        tmp.path().join("gone"),
        tmp.path().join("real").join("..").join("gone"),
    ] {
        let (code, stdout, stderr) = bp(&home, &["down".as_ref(), spelled.as_os_str()]);
        assert_eq!(code, 3, "{}: {stdout}{stderr}", spelled.display());
    }
    assert!(dir.join("pod.json").exists());
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git").arg("-C").arg(dir).args(args).status();
    assert!(status.unwrap().success(), "git {args:?}");
}

/// `git worktree remove --force` may leave the directory behind.
#[test]
fn a_directory_outside_any_repository_has_no_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let left = tmp.path().join("left-\u{e9}");
    fs::create_dir(&left).unwrap();
    for args in [&["down"][..], &["status"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_branchpod"))
            .args(args)
            .arg(&left)
            .env("BRANCHPOD_HOME", &home)
            // Git translates what it says.
            .env("LC_ALL", "C.UTF-8")
            .env("LANGUAGE", "ru")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {stdout}{stderr}");
        assert!(stdout.starts_with("no pod for "), "{args:?}: {stdout}");
        assert_eq!(stderr, "", "{args:?}");
    }
    let args = ["status".as_ref(), "--json".as_ref(), left.as_os_str()];
    let (code, stdout, stderr) = bp(&home, &args);
    let status: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!((code, &status["pod"]), (0, &serde_json::Value::Null));
    assert_eq!(stderr, "");
    // There is nothing to print.
    let (code, _, stderr) = bp(&home, &["logs".as_ref(), "web".as_ref(), left.as_os_str()]);
    assert_eq!(code, 1);
    assert!(stderr.starts_with("bp: no pod for "), "{stderr}");
    // These need a worktree.
    for command in ["up", "config"] {
        let (code, _, stderr) = bp(&home, &[command.as_ref(), left.as_os_str()]);
        assert_eq!(code, 1, "{command}");
        assert!(stderr.contains("is not inside a git worktree"), "{stderr}");
    }
}

/// A hook would take "no pod" for "nothing is left".
#[test]
fn what_keeps_git_from_telling_stays_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let dir = |name: &str| {
        let dir = base.join(name);
        fs::create_dir(&dir).unwrap();
        dir
    };
    let dangling = dir("dangling");
    let target = base.join("no such directory");
    fs::write(
        dangling.join(".git"),
        format!("gitdir: {}\n", target.display()),
    )
    .unwrap();
    let garbage = dir("garbage");
    fs::write(garbage.join(".git"), "junk\n").unwrap();
    // To git the same as none.
    let empty = dir("empty");
    fs::create_dir(empty.join(".git")).unwrap();
    let below = empty.join("sub");
    fs::create_dir(&below).unwrap();
    let bare = dir("bare");
    git(&bare, &["init", "-q", "--bare"]);
    for broken in [&dangling, &garbage, &empty, &below, &bare] {
        for command in ["down", "status"] {
            let (code, stdout, stderr) = bp(&home, &[command.as_ref(), broken.as_os_str()]);
            assert_eq!(code, 1, "{command} {}: {stdout}", broken.display());
            assert!(stderr.contains("is not inside a git worktree"), "{stderr}");
        }
    }

    let plain = dir("plain");
    let run = |name: &str, value: &Path| {
        let out = Command::new(env!("CARGO_BIN_EXE_branchpod"))
            .arg("down")
            .arg(&plain)
            .env("BRANCHPOD_HOME", &home)
            .env(name, value)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        (out.status.code(), stderr)
    };
    // No git there.
    let (code, stderr) = run("PATH", &plain);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("cannot run git"), "{stderr}");
    let (code, stderr) = run("GIT_DIR", &target);
    assert_eq!(code, Some(1), "{stderr}");
}

/// What is left of a worktree inside the main checkout: the pod of the main checkout is another one.
#[test]
fn a_path_inside_a_worktree_does_not_stand_for_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let main = dunce::canonicalize(tmp.path()).unwrap().join("main");
    let left = main.join(".worktrees").join("x");
    fs::create_dir_all(&left).unwrap();
    git(&main, &["init", "-q"]);
    let intent = entry("service", "vite", "intent", serde_json::Value::Null);
    let dir = pod(&home, &main, &[intent]);
    let before = fs::read_to_string(dir.join("ledger.jsonl")).unwrap();

    for command in ["down", "status"] {
        let (code, stdout, stderr) = bp(&home, &[command.as_ref(), left.as_os_str()]);
        assert_eq!(code, 0, "{command}: {stdout}{stderr}");
        assert!(stdout.starts_with("no pod for "), "{command}: {stdout}");
        assert!(stderr.contains("is not a worktree itself"), "{stderr}");
    }
    let (code, _, stderr) = bp(&home, &["logs".as_ref(), "vite".as_ref(), left.as_os_str()]);
    assert_eq!(code, 1);
    assert!(stderr.contains("bp: no pod for "), "{stderr}");
    let after = fs::read_to_string(dir.join("ledger.jsonl")).unwrap();
    assert_eq!(after, before);

    // Its own path does, and so does the current directory.
    let (code, stdout, _) = bp(&home, &["status".as_ref(), main.as_os_str()]);
    assert_eq!(code, 4, "{stdout}");
    let out = Command::new(env!("CARGO_BIN_EXE_branchpod"))
        .arg("status")
        .current_dir(&left)
        .env("BRANCHPOD_HOME", &home)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
}

#[test]
fn a_damaged_ledger_means_teardown_could_not_run() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let worktree = tmp.path().join("gone");
    let dir = pod(&home, &worktree, &[]);
    fs::write(
        dir.join("ledger.jsonl"),
        "{\"v\":1,\"ts_ms\":1,\"kind\":\"bogus\",\"name\":\"a\",\"op\":\"intent\"}\n{}\n",
    )
    .unwrap();
    let (code, _, stderr) = bp(&home, &["down".as_ref(), worktree.as_os_str()]);
    assert_eq!(code, 1, "{stderr}");
    assert!(dir.join("pod.json").exists());
}
