//! Pods: the per-worktree unit `bp` manages, and where their state lives.
//!
//! State is kept in the user data directory, not inside the worktree, so it survives the worktree
//! being deleted behind our back, the case `gc` exists for.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ledger;
use crate::mark::Mark;

pub(crate) const META_VERSION: u32 = 1;
const HOME_ENV: &str = "BRANCHPOD_HOME";

/// Stable identifier: a readable slug of the directory name plus a hash of the full path.
/// Used in labels (`bp.pod=<id>`), so it is limited to `[a-z0-9-]`.
pub(crate) fn pod_id(worktree_root: &Path) -> String {
    let slug: String = worktree_root
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug: String = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = slug
        .chars()
        .take(24)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned();

    let digest = Sha256::digest(path_key(worktree_root).as_bytes());
    let hash: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();

    if slug.is_empty() {
        format!("pod-{hash}")
    } else {
        format!("{slug}-{hash}")
    }
}

/// Path form used for identity. Windows and macOS file systems are case-insensitive by default,
/// so case differences in how a caller spells the path must not create a second pod.
fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(any(windows, target_os = "macos")) {
        s.to_lowercase()
    } else {
        s.to_owned()
    }
}

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PodMeta {
    pub(crate) v: u32,
    pub(crate) id: String,
    pub(crate) worktree: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) main_checkout: Option<PathBuf>,
    /// Set while `bp up` or `bp down` changes the pod; still set afterwards if it failed or was
    /// interrupted. The ledger does not tell: what `up` rejects while planning is not recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) unfinished: Option<Unfinished>,
    /// What tells the worktree from another one at the same path. Versions before it did not mark
    /// worktrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mark: Option<Mark>,
    /// Of the config it was created from ([`crate::config::Config::hash`]). Recorded since `mark` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config_hash: Option<String>,
}

/// What a pod is in the middle of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Unfinished {
    Up,
    Down,
    /// Written by a newer version. Without it such a pod, and with it every list of pods, could not
    /// be read.
    #[serde(other)]
    Other,
}

impl PodMeta {
    pub(crate) fn with(&self, unfinished: Option<Unfinished>) -> PodMeta {
        PodMeta {
            unfinished,
            ..self.clone()
        }
    }
}

/// The `bp` home: `$BRANCHPOD_HOME`, or the platform's local data directory. [`Store::open`] makes
/// it absolute, so every path derived from it means the same in any working directory.
pub(crate) struct Store {
    home: PathBuf,
}

/// The home for this value of the variable (an empty one means "not set"), as an absolute path:
/// the supervisor and the commands run in other directories. The default is not always absolute
/// either (a relative `$HOME`). Not canonicalized: the home need not exist yet, and links in it
/// stay as the user spelled them.
fn resolve_home(
    var: Option<OsString>,
    default: impl FnOnce() -> Result<PathBuf>,
) -> Result<PathBuf> {
    let home = match var {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => default()?,
    };
    std::path::absolute(&home)
        .with_context(|| format!("cannot resolve the bp home {}", home.display()))
}

impl Store {
    pub(crate) fn open() -> Result<Store> {
        let home = resolve_home(std::env::var_os(HOME_ENV), || {
            let dirs = directories::ProjectDirs::from("", "", "branchpod")
                .context("cannot determine the user data directory")?;
            Ok(dirs.data_local_dir().to_path_buf())
        })?;
        Ok(Store { home })
    }

    #[cfg(test)]
    pub(crate) fn at(home: PathBuf) -> Store {
        Store { home }
    }

    pub(crate) fn home(&self) -> &Path {
        &self.home
    }

    pub(crate) fn pod_dir(&self, id: &str) -> PathBuf {
        self.home.join("pods").join(id)
    }

    pub(crate) fn ledger_path(&self, id: &str) -> PathBuf {
        self.pod_dir(id).join("ledger.jsonl")
    }

    pub(crate) fn write_meta(&self, meta: &PodMeta) -> Result<()> {
        let dir = self.pod_dir(&meta.id);
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join("pod.json");
        crate::fsutil::write_atomic(&path, &serde_json::to_vec_pretty(meta)?)
            .with_context(|| format!("cannot write {}", path.display()))
    }

    /// `None` when there is no pod `id` (any more).
    pub(crate) fn meta(&self, id: &str) -> Result<Option<PodMeta>> {
        let path = self.pod_dir(id).join("pod.json");
        let bytes = match crate::fsutil::retry(|| fs::read(&path)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("cannot parse {}", path.display()))
    }

    pub(crate) fn list(&self) -> Result<Vec<PodMeta>> {
        let dir = self.home.join("pods");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
        };
        let mut pods = Vec::new();
        for entry in entries {
            let path = entry?.path().join("pod.json");
            // Being replaced, it may not open for a moment on Windows.
            if let Ok(bytes) = crate::fsutil::retry(|| fs::read(&path)) {
                let meta: PodMeta = serde_json::from_slice(&bytes)
                    .with_context(|| format!("cannot parse {}", path.display()))?;
                pods.push(meta);
            }
        }
        pods.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(pods)
    }

    /// Finds a known pod by its worktree path. Works when the directory no longer exists, which
    /// is why it compares recorded paths instead of recomputing an id from a canonical path.
    pub(crate) fn find_by_worktree(&self, path: &Path) -> Result<Option<PodMeta>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|p| same_path(&p.worktree, path)))
    }

    pub(crate) fn resources(&self, id: &str) -> Result<Vec<ledger::ResourceState>> {
        Ok(ledger::state(&ledger::read(&self.ledger_path(id))?))
    }

    /// Removes the state of pod `id` once nothing recorded in it is left. Everything else goes
    /// before `pod.json`: interrupted or failing before that, the pod is still listed with nothing
    /// to tear down, and the next `bp down` finishes the removal. The lock file is kept (see
    /// `podlock`).
    pub(crate) fn remove_pod(&self, id: &str) -> Result<()> {
        use crate::fsutil::retry;

        let checked = |path: &Path, result: std::io::Result<()>| match result {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("cannot remove {}", path.display()))
            }
            _ => Ok(()),
        };
        let dir = self.pod_dir(id);
        let meta = dir.join("pod.json");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path == meta {
                continue;
            }
            let result = if entry.file_type()?.is_dir() {
                retry(|| fs::remove_dir_all(&path))
            } else {
                retry(|| fs::remove_file(&path))
            };
            checked(&path, result)?;
        }
        checked(&meta, retry(|| fs::remove_file(&meta)))?;
        checked(&dir, retry(|| fs::remove_dir(&dir)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_stable_readable_and_label_safe() {
        let a = pod_id(Path::new("/work/My Feature_branch"));
        assert_eq!(a, pod_id(Path::new("/work/My Feature_branch/")));
        assert!(a.starts_with("my-feature-branch-"), "{a}");
        assert!(
            a.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        assert_ne!(a, pod_id(Path::new("/other/My Feature_branch")));
        assert!(pod_id(Path::new("/work/\u{e9}")).starts_with("pod-"));
    }

    #[test]
    fn home_is_the_variable_or_the_default_and_absolute() {
        let cwd = std::env::current_dir().unwrap();
        let default = || Ok(PathBuf::from("default"));
        let home = |var: Option<&str>| resolve_home(var.map(OsString::from), default).unwrap();
        assert_eq!(home(Some("rel-\u{fc}")), cwd.join("rel-\u{fc}"));
        // Also when the default is relative.
        assert_eq!(home(None), cwd.join("default"));
        assert_eq!(home(Some("")), cwd.join("default"));
        let absolute = cwd.join("no such directory");
        assert_eq!(home(absolute.to_str()), absolute);
        // The default is not needed, so it may fail.
        assert!(resolve_home(Some("x".into()), || anyhow::bail!("no default")).is_ok());
        assert!(resolve_home(None, || anyhow::bail!("no default")).is_err());
    }

    #[test]
    fn finds_pod_by_recorded_path_even_if_gone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().to_path_buf());
        let worktree = dir.path().join("gone-worktree");
        let meta = PodMeta {
            v: META_VERSION,
            id: pod_id(&worktree),
            worktree: worktree.clone(),
            main_checkout: None,
            unfinished: None,
            mark: None,
            config_hash: None,
        };
        store.write_meta(&meta).unwrap();

        assert_eq!(store.find_by_worktree(&worktree).unwrap(), Some(meta));
        assert_eq!(
            store.find_by_worktree(&dir.path().join("other")).unwrap(),
            None
        );
    }

    /// Written by older versions, and read by them: no field is required or refused.
    #[test]
    fn only_a_pod_that_is_unfinished_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().to_path_buf());
        assert_eq!(store.meta("p").unwrap(), None);
        let meta: PodMeta =
            serde_json::from_str(r#"{ "v": 1, "id": "p", "worktree": "/wt", "later": 1 }"#)
                .unwrap();
        assert_eq!(meta.unfinished, None);
        let newer = r#"{ "v": 1, "id": "p", "worktree": "/wt", "unfinished": "gc" }"#;
        let newer: PodMeta = serde_json::from_str(newer).unwrap();
        assert_eq!(newer.unfinished, Some(Unfinished::Other));
        store
            .write_meta(&meta.with(Some(Unfinished::Down)))
            .unwrap();
        let text = fs::read_to_string(store.pod_dir("p").join("pod.json")).unwrap();
        assert!(text.contains(r#""unfinished": "down""#), "{text}");
        let read = store.meta("p").unwrap().unwrap();
        assert_eq!(read.unfinished, Some(Unfinished::Down));
        store.write_meta(&read.with(None)).unwrap();
        let text = fs::read_to_string(store.pod_dir("p").join("pod.json")).unwrap();
        assert!(!text.contains("unfinished"), "{text}");
        assert_eq!(store.meta("p").unwrap(), Some(meta));
    }
}
