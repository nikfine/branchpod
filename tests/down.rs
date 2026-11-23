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
