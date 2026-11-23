//! Locating the worktree `bp` was pointed at, and its main checkout.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Top-level directory of the worktree.
    pub root: PathBuf,
    /// Top-level directory of the main checkout; `None` when the common git dir is not a `.git`
    /// directory inside one (a bare repository, a submodule, `--separate-git-dir`).
    pub main_checkout: Option<PathBuf>,
}

impl Worktree {
    /// Resolves `path` (or the current directory) to the enclosing worktree.
    pub fn resolve(path: Option<&OsStr>) -> Result<Worktree> {
        let start = match path {
            Some(p) => {
                reject_placeholder(p)?;
                PathBuf::from(p)
            }
            None => std::env::current_dir().context("cannot determine the current directory")?,
        };
        if !start.is_dir() {
            bail!("{} is not a directory", start.display());
        }

        let out = git(
            &start,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-common-dir",
            ],
        )
        .with_context(|| format!("{} is not inside a git worktree", start.display()))?;
        let mut lines = out.lines();
        let (Some(top), Some(common)) = (lines.next(), lines.next()) else {
            bail!("unexpected output from `git rev-parse`: {out:?}");
        };

        let root = canonical(Path::new(top))?;
        let common = canonical(Path::new(common))?;
        // A non-bare repository keeps its common dir at `<main checkout>/.git`.
        let main_checkout = if common.file_name() == Some(OsStr::new(".git")) {
            common.parent().map(Path::to_path_buf)
        } else {
            None
        };
        Ok(Worktree {
            root,
            main_checkout,
        })
    }

    /// The checked-out branch name (`feature/x`); `None` on a detached HEAD. A branch without
    /// commits yet counts as checked out.
    pub fn branch(&self) -> Result<Option<String>> {
        // `symbolic-ref -q` exits with 1 and prints nothing when HEAD is not a branch. Not
        // `--short`: it abbreviates to a name unambiguous among all refs, so a tag with the same
        // name as the branch turns `feature` into `heads/feature`.
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["symbolic-ref", "-q", "HEAD"])
            .output()
            .context("cannot run git (is it installed and on PATH?)")?;
        match output.status.code() {
            Some(0) => {
                let full =
                    String::from_utf8(output.stdout).context("git printed non-UTF-8 output")?;
                let full = full.trim();
                match full.strip_prefix("refs/heads/") {
                    Some(name) => Ok(Some(name.to_owned())),
                    None => bail!("HEAD points to {full}, not to a branch"),
                }
            }
            Some(1) if output.stdout.is_empty() => Ok(None),
            _ => bail!(
                "git symbolic-ref HEAD failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }
}

/// Hook runners sometimes pass `%VAR%` or `$VAR` through unexpanded. Such a string is almost
/// never a real path, and treating it as one creates directories named `%WORKTREE_PATH%`.
pub fn reject_placeholder(arg: &OsStr) -> Result<()> {
    let text = arg.to_string_lossy();
    if let Some(name) = unexpanded_variable(&text) {
        bail!(
            "{text:?} looks like an unexpanded variable ({name}); \
             the caller should expand it before invoking bp"
        );
    }
    Ok(())
}

fn unexpanded_variable(text: &str) -> Option<String> {
    let is_ident = |s: &str| {
        !s.is_empty()
            && s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };

    // %NAME%
    let mut rest = text;
    while let Some(open) = rest.find('%') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('%') else { break };
        let name = &after[..close];
        if is_ident(name) {
            return Some(format!("%{name}%"));
        }
        rest = &after[close..];
    }

    // ${NAME} anywhere, $NAME at the start
    if let Some(start) = text.find("${")
        && let Some(end) = text[start..].find('}')
    {
        let name = &text[start + 2..start + end];
        if is_ident(name) {
            return Some(format!("${{{name}}}"));
        }
    }
    if let Some(rest) = text.strip_prefix('$') {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if is_ident(&name) {
            return Some(format!("${name}"));
        }
    }
    None
}

/// Canonical path without the `\\?\` prefix on Windows, which many tools cannot handle.
pub fn canonical(path: &Path) -> Result<PathBuf> {
    dunce::canonicalize(path).with_context(|| format!("cannot resolve {}", path.display()))
}

/// The canonical form of `path`, which may not exist (a worktree deleted behind our back): its
/// deepest existing ancestor is canonicalized and the rest appended, with `.` and `..` resolved
/// lexically. So a short 8.3 name on Windows, `..`, or `/var` for `/private/var` on macOS still
/// matches the canonical path recorded for a pod.
pub fn normalize(path: &Path) -> Result<PathBuf> {
    let abs =
        std::path::absolute(path).with_context(|| format!("cannot resolve {}", path.display()))?;
    for ancestor in abs.ancestors() {
        let Ok(mut out) = dunce::canonicalize(ancestor) else {
            continue;
        };
        let rest = abs.strip_prefix(ancestor).unwrap_or(Path::new(""));
        for component in rest.components() {
            match component {
                Component::ParentDir => {
                    out.pop();
                }
                Component::CurDir => {}
                other => out.push(other),
            }
        }
        return Ok(out);
    }
    Ok(abs)
}

/// Runs git without a shell and returns stdout. Git prints paths as UTF-8 on every platform, so
/// the output is never decoded with the console code page.
pub fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .context("cannot run git (is it installed and on PATH?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    String::from_utf8(output.stdout).context("git printed non-UTF-8 output")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_unexpanded_variables() {
        for text in [
            "%WORKTREE_PATH%",
            "C:\\x\\%WT%\\y",
            "$WORKTREE",
            "${WORKTREE}",
            "/a/${B}/c",
        ] {
            assert!(unexpanded_variable(text).is_some(), "{text}");
        }
    }

    #[test]
    fn accepts_real_paths() {
        for text in [
            "C:\\projects\\caf\u{e9}",
            "/srv/projects/example",
            "100% done",
            "a%b",
            "50%-60%",
            "/srv/a$b",
            "C:\\x\\% y %",
        ] {
            assert!(unexpanded_variable(text).is_none(), "{text}");
        }
    }

    #[test]
    fn normalizes_a_path_that_does_not_exist() {
        // The temporary directory is often spelled non-canonically: an 8.3 name on Windows
        // runners (`RUNNER~1`), `/var` for `/private/var` on macOS.
        let tmp = tempfile::tempdir().unwrap();
        let base = canonical(tmp.path()).unwrap();
        std::fs::create_dir(tmp.path().join("real")).unwrap();
        let expected = base.join("gone").join("wt");
        for spelled in [
            tmp.path().join("gone").join("wt"),
            tmp.path().join("real").join("..").join("gone").join("wt"),
            tmp.path()
                .join("nope")
                .join("..")
                .join("gone")
                .join(".")
                .join("wt"),
        ] {
            assert_eq!(
                normalize(&spelled).unwrap(),
                expected,
                "{}",
                spelled.display()
            );
        }
        assert_eq!(
            normalize(&tmp.path().join("real")).unwrap(),
            base.join("real")
        );
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn resolves_main_checkout_and_linked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        // Non-ASCII on purpose: it is the normal case on many developer machines.
        let main = tmp.path().join("main-caf\u{e9}");
        std::fs::create_dir(&main).unwrap();
        git_ok(&main, &["init", "-q", "-b", "main"]);
        git_ok(
            &main,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let linked = tmp.path().join("feature-na\u{ef}ve");
        git_ok(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().unwrap(),
            ],
        );

        let sub = main.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let from_main = Worktree::resolve(Some(sub.as_os_str())).unwrap();
        assert_eq!(from_main.root, canonical(&main).unwrap());
        assert_eq!(from_main.main_checkout, Some(canonical(&main).unwrap()));

        let from_linked = Worktree::resolve(Some(linked.as_os_str())).unwrap();
        assert_eq!(from_linked.root, canonical(&linked).unwrap());
        assert_eq!(from_linked.main_checkout, Some(canonical(&main).unwrap()));

        assert_eq!(from_linked.branch().unwrap().as_deref(), Some("feature"));
        // A tag with the same name does not change how the branch is named.
        git_ok(&linked, &["tag", "feature"]);
        assert_eq!(from_linked.branch().unwrap().as_deref(), Some("feature"));
        git_ok(&linked, &["checkout", "-q", "--detach"]);
        assert_eq!(from_linked.branch().unwrap(), None);
    }
}
