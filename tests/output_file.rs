//! `output = "file"` from `bp up` to `bp down`: the output is in its file, whole, and nowhere else.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// BOM, CRLF, a trailing space and bytes that are not UTF-8.
const RAW: &[u8] = b"\xef\xbb\xbfTOKEN=s3cr3t \r\nX=\xa8\xa4\r\n\n";
const SECRET: &str = "s3cr3t";

struct Setup {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    worktree: PathBuf,
}

/// A main checkout whose local config is `config`, a linked worktree, and a file holding [`RAW`].
/// Non-ASCII names throughout.
fn setup(config: impl FnOnce(&Path) -> String) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let main = base.join("main-caf\u{e9}");
    let worktree = base.join("feature-na\u{ef}ve");
    fs::create_dir(&main).unwrap();
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
    fs::write(base.join("raw.txt"), RAW).unwrap();
    let text = format!("version = 1\n{}", config(&base));
    fs::write(main.join("branchpod.local.toml"), text).unwrap();
    Setup {
        _tmp: tmp,
        base,
        worktree,
    }
}

impl Setup {
    /// Runs `bp` with a home given as a relative path: exit code and all it printed.
    fn bp(&self, args: &[&str]) -> (i32, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_branchpod"))
            .args(args)
            .arg(&self.worktree)
            .current_dir(&self.base)
            .env("BRANCHPOD_HOME", "home-\u{fc}")
            .output()
            .unwrap();
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.code().unwrap(), text)
    }

    /// The only pod's state directory.
    fn pod_dir(&self) -> PathBuf {
        let pods: Vec<PathBuf> = fs::read_dir(self.home().join("pods"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(pods.len(), 1, "{pods:?}");
        pods[0].clone()
    }

    fn home(&self) -> PathBuf {
        self.base.join("home-\u{fc}")
    }

    /// Files of the `bp` home that hold the secret.
    fn leaks(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, found);
                } else if String::from_utf8_lossy(&fs::read(&path).unwrap()).contains(SECRET) {
                    found.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(&self.home(), &mut found);
        found.sort();
        found
    }
}

/// One `[[command]]`; `rest` is appended to it.
fn command(name: &str, create: &[String], rest: &str) -> String {
    let create = serde_json::to_string(create).unwrap();
    format!("[[command]]\nname = '{name}'\ncreate = {create}\n{rest}\n")
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// Prints `file` byte for byte, using only what every CI runner has.
fn cat(file: &str) -> Vec<String> {
    if cfg!(windows) {
        strings(&["cmd", "/d", "/c", "type", file])
    } else {
        strings(&["cat", file])
    }
}

const FILE: &str = "output = 'file'";

#[test]
fn the_output_is_in_its_file_and_nowhere_else() {
    let s = setup(|base| {
        let echo: &[&str] = if cfg!(windows) {
            &["cmd", "/d", "/c", "echo"]
        } else {
            &["echo"]
        };
        let mut spelled = strings(echo);
        spelled.extend(strings(&[
            "{{command.env.file | slash}}",
            "{{command.env.file | json}}",
        ]));
        [
            command("env", &cat(base.join("raw.txt").to_str().unwrap()), FILE),
            // A later command reads the file.
            command("copy", &cat("{{command.env.file}}"), FILE),
            command("spelled", &spelled, FILE),
        ]
        .concat()
    });
    let (code, text) = s.bp(&["up"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("output in ") && !text.contains(SECRET),
        "{text}"
    );

    let outputs = s.pod_dir().join("outputs");
    let (env, copy) = (outputs.join("env"), outputs.join("copy"));
    assert_eq!(fs::read(&env).unwrap(), RAW);
    assert_eq!(fs::read(&copy).unwrap(), RAW);
    assert_eq!(s.leaks(), [copy, env.clone()]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&outputs), mode(&env)), (0o700, 0o600));
    }

    let (code, text) = s.bp(&["status", "--json"]);
    assert_eq!(code, 0, "{text}");
    assert!(!text.contains(SECRET), "{text}");
    let status: serde_json::Value = serde_json::from_str(&text).unwrap();
    let resources = status["resources"].as_array().unwrap();
    let env = env.to_str().unwrap();
    assert_eq!(resources[0]["data"]["file"], env);
    assert!(resources[0]["data"].get("stdout").is_none(), "{text}");
    let create = resources[2]["data"]["create"].as_array().unwrap();
    let [.., slash, json] = create.as_slice() else {
        panic!("{create:?}")
    };
    if cfg!(windows) {
        assert_eq!(slash, &env.replace('\\', "/"));
    } else {
        assert_eq!(slash, env);
    }
    let quoted = format!("\"{}\"", json.as_str().unwrap());
    assert_eq!(serde_json::from_str::<String>(&quoted).unwrap(), env);

    let (code, text) = s.bp(&["down"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("clean") && !text.contains(SECRET), "{text}");
    assert!(s.leaks().is_empty());
    assert_eq!(fs::read_dir(s.home().join("pods")).unwrap().count(), 0);
}

#[test]
fn the_file_stays_until_nothing_is_left_to_destroy() {
    let s = setup(|base| {
        let raw = base.join("raw.txt");
        let fail = if cfg!(windows) {
            let script = format!("type {} & echo boom 1>&2 & exit 3", raw.display());
            strings(&["cmd", "/d", "/c", &script])
        } else {
            let script = format!("cat '{}'; echo boom >&2; exit 3", raw.display());
            strings(&["sh", "-c", &script])
        };
        // Fails until the file appears.
        let destroy = serde_json::to_string(&cat(base.join("unblock").to_str().unwrap())).unwrap();
        [
            command(
                "env",
                &cat(raw.to_str().unwrap()),
                &format!("{FILE}\ndestroy = {destroy}"),
            ),
            command("broken", &fail, FILE),
        ]
        .concat()
    });
    let (code, text) = s.bp(&["up"]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("boom") && !text.contains(SECRET), "{text}");
    let env = s.pod_dir().join("outputs").join("env");
    assert_eq!(s.leaks(), std::slice::from_ref(&env));

    let (code, text) = s.bp(&["down"]);
    assert_eq!(code, 3, "{text}");
    assert!(!text.contains(SECRET), "{text}");
    assert_eq!(fs::read(&env).unwrap(), RAW);

    fs::write(s.base.join("unblock"), "x").unwrap();
    let (code, text) = s.bp(&["down"]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(fs::read_dir(s.home().join("pods")).unwrap().count(), 0);
}
