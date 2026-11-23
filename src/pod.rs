//! Pods: the per-worktree unit `bp` manages, and where their state lives.
//!
//! State is kept in the user data directory, not inside the worktree, so it survives the worktree
//! being deleted behind our back, the case `gc` exists for.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ledger;

pub const META_VERSION: u32 = 1;
const HOME_ENV: &str = "BRANCHPOD_HOME";

/// Stable identifier: a readable slug of the directory name plus a hash of the full path.
/// Used in labels (`bp.pod=<id>`), so it is limited to `[a-z0-9-]`.
pub fn pod_id(worktree_root: &Path) -> String {
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

pub fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodMeta {
    pub v: u32,
    pub id: String,
    pub worktree: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_checkout: Option<PathBuf>,
}

/// The `bp` home: `$BRANCHPOD_HOME`, or the platform's local data directory.
pub struct Store {
    home: PathBuf,
}

impl Store {
    pub fn open() -> Result<Store> {
        let home = match std::env::var_os(HOME_ENV) {
            Some(home) if !home.is_empty() => PathBuf::from(home),
            _ => directories::ProjectDirs::from("", "", "branchpod")
                .context("cannot determine the user data directory")?
                .data_local_dir()
                .to_path_buf(),
        };
        Ok(Store { home })
    }

    pub fn at(home: PathBuf) -> Store {
        Store { home }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn pod_dir(&self, id: &str) -> PathBuf {
        self.home.join("pods").join(id)
    }

    pub fn ledger_path(&self, id: &str) -> PathBuf {
        self.pod_dir(id).join("ledger.jsonl")
    }

    pub fn write_meta(&self, meta: &PodMeta) -> Result<()> {
        let dir = self.pod_dir(&meta.id);
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join("pod.json");
        crate::fsutil::write_atomic(&path, &serde_json::to_vec_pretty(meta)?)
            .with_context(|| format!("cannot write {}", path.display()))
    }

    pub fn list(&self) -> Result<Vec<PodMeta>> {
        let dir = self.home.join("pods");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
        };
        let mut pods = Vec::new();
        for entry in entries {
            let path = entry?.path().join("pod.json");
            if let Ok(bytes) = fs::read(&path) {
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
    pub fn find_by_worktree(&self, path: &Path) -> Result<Option<PodMeta>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|p| same_path(&p.worktree, path)))
    }

    pub fn resources(&self, id: &str) -> Result<Vec<ledger::ResourceState>> {
        Ok(ledger::state(&ledger::read(&self.ledger_path(id))?))
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
    fn finds_pod_by_recorded_path_even_if_gone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().to_path_buf());
        let worktree = dir.path().join("gone-worktree");
        let meta = PodMeta {
            v: META_VERSION,
            id: pod_id(&worktree),
            worktree: worktree.clone(),
            main_checkout: None,
        };
        store.write_meta(&meta).unwrap();

        assert_eq!(store.find_by_worktree(&worktree).unwrap(), Some(meta));
        assert_eq!(
            store.find_by_worktree(&dir.path().join("other")).unwrap(),
            None
        );
    }
}
