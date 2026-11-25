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

/// The ledgers of the pods that are gone.
fn history(home: &Path) -> Vec<String> {
    let entries = fs::read_dir(home.join("history"));
    let paths = entries.into_iter().flatten().map(|e| e.unwrap().path());
    paths.map(|p| fs::read_to_string(p).unwrap()).collect()
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
    assert!(history(&home).is_empty());

    // Once the leftover is gone (here: by hand), `down` finishes and removes the pod.
    let destroyed = entry("service", "vite", "destroyed", serde_json::Value::Null);
    let mut ledger = fs::read_to_string(dir.join("ledger.jsonl")).unwrap();
    ledger.push_str(&format!("{destroyed}\n"));
    fs::write(dir.join("ledger.jsonl"), &ledger).unwrap();
    let (code, stdout, stderr) = bp(&home, &["down".as_ref(), worktree.as_os_str()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("clean"), "{stdout}");
    assert!(!dir.exists());
    assert_eq!(history(&home), [ledger.as_str()]);

    // Nothing recorded any more: still a success.
    let (code, stdout, _) = bp(&home, &["down".as_ref(), worktree.as_os_str()]);
    assert_eq!(code, 0);
    assert!(stdout.contains("no pod"), "{stdout}");
    assert_eq!(history(&home), [ledger]);
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

/// What `down`, `status` and `logs` say about `path`.
fn assert_no_pod(home: &Path, path: &Path) {
    for command in ["down", "status"] {
        let (code, stdout, stderr) = bp(home, &[command.as_ref(), path.as_os_str()]);
        let what = format!("{command} {}: {stdout}{stderr}", path.display());
        assert_eq!((code, stderr.as_str()), (0, ""), "{what}");
        assert!(stdout.starts_with("no pod for "), "{what}");
    }
    let args = ["status".as_ref(), "--json".as_ref(), path.as_os_str()];
    let (code, stdout, stderr) = bp(home, &args);
    let status: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!((code, &status["pod"]), (0, &serde_json::Value::Null));
    assert_eq!(stderr, "");
    // There is nothing to print.
    let (code, _, stderr) = bp(home, &["logs".as_ref(), "web".as_ref(), path.as_os_str()]);
    assert_eq!(code, 1);
    assert!(stderr.starts_with("bp: no pod for "), "{stderr}");
}

/// `git worktree remove --force` may leave the directory behind, with or without its `.git`.
#[test]
fn what_is_left_of_a_worktree_has_no_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let left = tmp.path().join("left-\u{e9}");
    fs::create_dir(&left).unwrap();
    assert_no_pod(&home, &left);
    let nowhere = tmp.path().join("no such directory");
    let gitdir = format!("gitdir: {}\n", nowhere.display());
    fs::write(left.join(".git"), gitdir).unwrap();
    assert_no_pod(&home, &left);
    // These need a worktree.
    for command in ["up", "config"] {
        let (code, _, stderr) = bp(&home, &[command.as_ref(), left.as_os_str()]);
        assert_eq!(code, 1, "{command}");
        assert!(stderr.contains("is not inside a git worktree"), "{stderr}");
    }

    // Inside a repository that has no pod.
    let main = tmp.path().join("main");
    let inside = main.join(".worktrees").join("x");
    fs::create_dir_all(&inside).unwrap();
    git(&main, &["init", "-q"]);
    assert_no_pod(&home, &inside);
}

#[test]
fn the_pod_around_a_path_is_not_its_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let main = dunce::canonicalize(tmp.path()).unwrap().join("main");
    let below = main.join("sub");
    let deep = main.join(".worktrees").join("x").join("node_modules");
    let own = main.join(".worktrees").join("own");
    let dangling = main.join(".worktrees").join("dangling");
    for dir in [&below, &deep, &own, &dangling] {
        fs::create_dir_all(dir).unwrap();
    }
    git(&main, &["init", "-q"]);
    let intent = entry("service", "vite", "intent", serde_json::Value::Null);
    let dir = pod(&home, &main, &[intent]);
    let state =
        || ["pod.json", "ledger.jsonl"].map(|name| fs::read_to_string(dir.join(name)).unwrap());
    let before = state();

    for path in [&below, &deep] {
        for args in [&["down"][..], &["status"], &["logs", "vite"]] {
            let mut args: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_ref()).collect();
            args.push(path.as_os_str());
            let (code, stdout, stderr) = bp(&home, &args);
            assert_eq!((code, stdout.as_str()), (1, ""), "{args:?}: {stderr}");
            assert!(stderr.contains("is not a worktree root"), "{stderr}");
        }
    }
    // These are roots, or were: a worktree without a pod, and what is left of one.
    git(&own, &["init", "-q"]);
    let nowhere = format!("gitdir: {}\n", main.join("nowhere").display());
    fs::write(dangling.join(".git"), nowhere).unwrap();
    assert_no_pod(&home, &own);
    assert_no_pod(&home, &dangling);
    // As is a worktree that is gone completely.
    assert_no_pod(&home, &main.join(".worktrees").join("gone"));
    assert_eq!(state(), before);

    // The current directory stands for the worktree it is in.
    for (command, code) in [("status", 4), ("down", 3)] {
        let out = Command::new(env!("CARGO_BIN_EXE_branchpod"))
            .arg(command)
            .current_dir(&below)
            .env("BRANCHPOD_HOME", &home)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(code), "{command}: {stderr}");
    }
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
