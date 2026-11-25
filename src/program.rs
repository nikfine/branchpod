//! Programs named without their extension, `npx` for `npx.cmd`.
//!
//! Only Windows has such names. There std finds `npx.exe` for `npx`, but no script, so the same
//! config would not work everywhere: node tools are installed as `.cmd` scripts. What std finds is
//! left to it, so a script is only looked for where there is no `.exe`.

use std::collections::BTreeMap;
use std::path::PathBuf;
#[cfg(any(windows, test))]
use std::{fs, path::Path};

/// What `PATHEXT` may name and `CreateProcess` runs (a script through `cmd`, which std starts for
/// it). `.exe` is not here: std looks for it.
#[cfg(any(windows, test))]
const EXTENSIONS: [&str; 3] = ["com", "bat", "cmd"];

/// `program` with the extension it has on disk if it is a script named without one, otherwise as
/// it is. A bare name is searched for in the directories that std searches; `env` is what is added
/// to the environment of the process, where `PATH` may be another one.
#[cfg(windows)]
pub(crate) fn complete(program: PathBuf, env: &BTreeMap<String, String>) -> PathBuf {
    let dirs = search_dirs(env);
    let pathext = std::env::var("PATHEXT").unwrap_or_default();
    find(&program, &dirs, &extensions(&pathext)).unwrap_or(program)
}

#[cfg(not(windows))]
pub(crate) fn complete(program: PathBuf, _env: &BTreeMap<String, String>) -> PathBuf {
    program
}

/// The directories where std's `Command` looks for a bare name, in its order.
#[cfg(windows)]
fn search_dirs(env: &BTreeMap<String, String>) -> Vec<PathBuf> {
    use std::env::{current_exe, split_paths, var_os};

    let mut dirs = Vec::new();
    // The last one wins, as in the environment of the process.
    let path = env
        .iter()
        .rfind(|(key, _)| key.eq_ignore_ascii_case("PATH"));
    if let Some((_, path)) = path {
        dirs.extend(split_paths(path));
    }
    if let Ok(exe) = current_exe() {
        dirs.extend(exe.parent().map(Path::to_path_buf));
    }
    if let Some(windows) = var_os("SystemRoot").map(PathBuf::from) {
        dirs.push(windows.join("System32"));
        dirs.push(windows);
    }
    if let Some(path) = var_os("PATH") {
        dirs.extend(split_paths(&path));
    }
    dirs.retain(|dir| !dir.as_os_str().is_empty());
    dirs
}

/// The [`EXTENSIONS`] in the order of `PATHEXT`, which may name others (`.JS`).
#[cfg(any(windows, test))]
fn extensions(pathext: &str) -> Vec<&'static str> {
    let pathext = if pathext.is_empty() {
        ".COM;.EXE;.BAT;.CMD"
    } else {
        pathext
    };
    pathext
        .split(';')
        .filter_map(|known| {
            let known = known.trim().strip_prefix('.')?;
            EXTENSIONS
                .into_iter()
                .find(|ext| known.eq_ignore_ascii_case(ext))
        })
        .collect()
}

/// The script that `program` names without its extension, if std would find no `.exe` for it. A
/// file without an extension, such as the `sh` script that npm puts next to `npx.cmd`, is nothing
/// that Windows runs.
#[cfg(any(windows, test))]
fn find(program: &Path, dirs: &[PathBuf], extensions: &[&str]) -> Option<PathBuf> {
    let name = program.file_name()?.to_str()?;
    if name.contains('.') {
        return None;
    }
    let own_dir;
    let dirs = if program.is_absolute() {
        own_dir = [program.parent()?.to_path_buf()];
        &own_dir
    } else if program.as_os_str() == name {
        dirs
    } else {
        return None;
    };
    // Not `exists`, as in std: it would follow the link that an app from the Store is.
    let exe = format!("{name}.exe");
    if dirs
        .iter()
        .any(|dir| fs::symlink_metadata(dir.join(&exe)).is_ok())
    {
        return None;
    }
    dirs.iter()
        // A relative one would be the current directory or depend on it, and `cmd` would look for
        // the script from another one.
        .filter(|dir| dir.is_absolute())
        .flat_map(|dir| {
            extensions
                .iter()
                .map(move |ext| dir.join(format!("{name}.{ext}")))
        })
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dirs {
        tmp: tempfile::TempDir,
        dirs: Vec<PathBuf>,
    }

    /// Directories with empty files, in the order of the search.
    fn dirs(files: &[&[&str]]) -> Dirs {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = files
            .iter()
            .enumerate()
            .map(|(i, files)| {
                let dir = tmp.path().join(i.to_string());
                fs::create_dir(&dir).unwrap();
                for file in *files {
                    fs::write(dir.join(file), "").unwrap();
                }
                dir
            })
            .collect();
        Dirs { tmp, dirs }
    }

    impl Dirs {
        /// Where `program` is found, relative to the parent of the directories.
        fn find(&self, program: &str) -> Option<PathBuf> {
            let found = find(Path::new(program), &self.dirs, &extensions(""))?;
            Some(found.strip_prefix(self.tmp.path()).unwrap().to_path_buf())
        }
    }

    fn found(dir: &str, file: &str) -> Option<PathBuf> {
        Some(Path::new(dir).join(file))
    }

    #[test]
    fn pathext_gives_the_order_and_only_what_runs() {
        assert_eq!(extensions(""), ["com", "bat", "cmd"]);
        assert_eq!(
            extensions(".CMD; .Com;.EXE;.VBS;.JS;;bat;.bat"),
            ["cmd", "com", "bat"]
        );
        assert!(extensions(".EXE").is_empty());

        let d = dirs(&[&["x.js", "x.bat", "x.cmd"]]);
        for (pathext, file) in [(".JS;.BAT;.CMD", "x.bat"), (".JS;.CMD;.BAT", "x.cmd")] {
            let path = find(Path::new("x"), &d.dirs, &extensions(pathext));
            assert_eq!(path, Some(d.dirs[0].join(file)));
        }
    }

    #[test]
    fn a_bare_name_is_the_first_script_in_the_directories() {
        let d = dirs(&[
            &["npx", "npx.ps1", "vite"],
            &["npx", "npx.cmd"],
            &["npx.bat", "npx.com"],
        ]);
        assert_eq!(d.find("npx"), found("1", "npx.cmd"));
        assert_eq!(d.find("vite"), None);
        assert_eq!(d.find("nothing"), None);

        fs::create_dir(d.dirs[0].join("npx.cmd")).unwrap();
        assert_eq!(d.find("npx"), found("1", "npx.cmd"));
    }

    #[test]
    fn what_std_finds_is_left_to_it() {
        // Wherever it is: it was found before scripts were.
        let d = dirs(&[&["x.com", "x.cmd", "y.cmd"], &["x.exe", "y.cmd.exe"]]);
        assert_eq!(d.find("x"), None);
        assert_eq!(d.find("y"), found("0", "y.cmd"));
        // With an extension, or what looks like one.
        assert_eq!(d.find("y.cmd"), None);
        let d = dirs(&[&["a.b.cmd"]]);
        assert_eq!(d.find("a.b"), None);
    }

    #[test]
    fn a_path_is_completed_where_it_is() {
        let d = dirs(&[&["vite", "vite.cmd", "both.cmd", "both.exe"], &["else.cmd"]]);
        let find = |program: &Path| find(program, &d.dirs, &extensions(""));
        let vite = d.dirs[0].join("vite");
        assert_eq!(find(&vite), Some(d.dirs[0].join("vite.cmd")));
        assert_eq!(find(&d.dirs[0].join("both")), None);
        assert_eq!(find(&d.dirs[0].join("else")), None);
        // Relative to a directory that is not known here.
        assert_eq!(find(&Path::new("0").join("vite")), None);
        assert_eq!(find(Path::new("./vite")), None);
        assert_eq!(find(Path::new("vite/")), None);
        assert_eq!(find(Path::new("..")), None);
    }

    /// An app from the Store is a link that cannot be followed.
    #[cfg(unix)]
    #[test]
    fn an_exe_that_is_a_link_is_left_to_std() {
        let d = dirs(&[&["x.cmd"], &[]]);
        std::os::unix::fs::symlink("gone", d.dirs[1].join("x.exe")).unwrap();
        assert_eq!(d.find("x"), None);
    }

    #[cfg(unix)] // Windows may have the temporary directory on another drive.
    #[test]
    fn scripts_are_not_searched_for_in_relative_directories() {
        let mut d = dirs(&[&["x.cmd"], &["x.cmd"]]);
        let up: PathBuf = std::env::current_dir()
            .unwrap()
            .components()
            .skip(1)
            .map(|_| "..")
            .collect();
        d.dirs[0] = up.join(d.dirs[0].strip_prefix("/").unwrap());
        assert!(d.dirs[0].join("x.cmd").is_file());
        assert_eq!(d.find("x"), found("1", "x.cmd"));
    }

    #[cfg(windows)]
    #[test]
    fn directories_are_searched_in_the_order_of_std() {
        let env = |key: &str| BTreeMap::from([(key.to_owned(), r#"C:\a;;"C:\b;c""#.to_owned())]);
        let without = search_dirs(&BTreeMap::new());
        let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32");
        assert_eq!(
            without[0],
            std::env::current_exe().unwrap().parent().unwrap()
        );
        assert_eq!(without[1], system);
        for key in ["PATH", "Path"] {
            let with = search_dirs(&env(key));
            assert_eq!(with[..2], [r"C:\a", r"C:\b;c"].map(PathBuf::from));
            assert_eq!(with[2..], without);
        }
    }
}
