//! Port allocation and the machine-wide port registry.
//!
//! Ports are allocated from a configured range and recorded in `ports.json` in the `bp` home,
//! guarded by `ports.lock`, so two worktrees running `bp up` at the same time never get the same
//! port. A candidate is accepted only if it can actually be bound on the host right now.
//!
//! The search starts at an offset derived from the pod id and port name, so a worktree tends to get
//! the same port on every `up` (stable URLs in browser tabs and bookmarks) without storing anything
//! after teardown.
//!
//! Recording the allocation in the pod's ledger is the caller's job (`bp up`); `bp down` releases it.

use std::collections::BTreeMap;
use std::fs::TryLockError;
use std::fs::{self, File, OpenOptions};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fsutil;

pub const REGISTRY_VERSION: u32 = 1;
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub pod: String,
    pub name: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    v: u32,
    #[serde(default)]
    ports: BTreeMap<u16, Owner>,
}

/// The registry stored under `home`. Every operation takes the lock for its whole duration.
pub struct Registry {
    file: PathBuf,
    lock: PathBuf,
    lock_timeout: Duration,
}

impl Registry {
    pub fn new(home: &Path) -> Registry {
        Registry {
            file: home.join("ports.json"),
            lock: home.join("ports.lock"),
            lock_timeout: LOCK_TIMEOUT,
        }
    }

    /// Allocates a port for `(pod, name)` from the inclusive `range`.
    ///
    /// An existing registration of the same owner inside the range is returned as is, without a
    /// bind check: the owner's own service may be listening on it (a repeated `bp up`), and
    /// moving it would point templates at a port nothing serves. Whether such a port was taken by
    /// a foreign process while the pod was down is for the caller to decide from the pod state;
    /// it can `release` and allocate again. Only new candidates are bind-checked.
    pub fn allocate(&self, pod: &str, name: &str, range: [u16; 2]) -> Result<u16> {
        self.allocate_with(pod, name, range, is_free)
    }

    fn allocate_with(
        &self,
        pod: &str,
        name: &str,
        [first, last]: [u16; 2],
        is_free: impl Fn(u16) -> bool,
    ) -> Result<u16> {
        if first == 0 || first > last {
            bail!("invalid port range [{first}, {last}]");
        }
        let _lock = self.lock()?;
        let mut reg = self.load()?;
        let owner = Owner {
            pod: pod.to_owned(),
            name: name.to_owned(),
        };

        let previous = reg
            .ports
            .iter()
            .find(|(_, o)| **o == owner)
            .map(|(p, _)| *p);
        if let Some(port) = previous {
            if (first..=last).contains(&port) {
                return Ok(port);
            }
            // The range changed in the config: the old registration no longer applies.
            reg.ports.remove(&port);
        }

        let len = u32::from(last - first) + 1;
        let start = preferred_offset(pod, name) % len;
        for i in 0..len {
            let port = first + ((start + i) % len) as u16;
            if reg.ports.contains_key(&port) || !is_free(port) {
                continue;
            }
            reg.ports.insert(port, owner);
            self.save(&reg)?;
            return Ok(port);
        }

        // Keep a registration outside the range dropped even when allocation fails.
        if previous.is_some() {
            self.save(&reg)?;
        }
        bail!("no free port for {pod}/{name} in [{first}, {last}]")
    }

    /// Releases the port of `(pod, name)`. Releasing something not registered is not an error, so
    /// teardown stays idempotent.
    pub fn release(&self, pod: &str, name: &str) -> Result<Option<u16>> {
        let _lock = self.lock()?;
        let mut reg = self.load()?;
        let port = reg
            .ports
            .iter()
            .find(|(_, o)| o.pod == pod && o.name == name)
            .map(|(p, _)| *p);
        if let Some(port) = port {
            reg.ports.remove(&port);
            self.save(&reg)?;
        }
        Ok(port)
    }

    /// Releases every port of a pod (used by `gc`).
    pub fn release_pod(&self, pod: &str) -> Result<Vec<u16>> {
        let _lock = self.lock()?;
        let mut reg = self.load()?;
        let ports: Vec<u16> = reg
            .ports
            .iter()
            .filter(|(_, o)| o.pod == pod)
            .map(|(p, _)| *p)
            .collect();
        if !ports.is_empty() {
            reg.ports.retain(|_, o| o.pod != pod);
            self.save(&reg)?;
        }
        Ok(ports)
    }

    /// The port currently registered to `(pod, name)`, if any.
    pub fn registered(&self, pod: &str, name: &str) -> Result<Option<u16>> {
        let _lock = self.lock()?;
        Ok(self
            .load()?
            .ports
            .iter()
            .find(|(_, o)| o.pod == pod && o.name == name)
            .map(|(p, _)| *p))
    }

    pub fn list(&self) -> Result<BTreeMap<u16, Owner>> {
        let _lock = self.lock()?;
        Ok(self.load()?.ports)
    }

    /// Takes the registry lock, waiting up to `lock_timeout` so that one hung `bp` cannot block
    /// every other invocation forever.
    fn lock(&self) -> Result<File> {
        if let Some(dir) = self.lock.parent() {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock)
            .with_context(|| format!("cannot open {}", self.lock.display()))?;
        let deadline = Instant::now() + self.lock_timeout;
        loop {
            match file.try_lock() {
                // Unlocked when the handle is dropped.
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(TryLockError::WouldBlock) => bail!(
                    "the port registry is locked by another bp process ({} for {}s); \
                     if no bp is running, the lock is held by a hung process",
                    self.lock.display(),
                    self.lock_timeout.as_secs_f32()
                ),
                Err(TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("cannot lock {}", self.lock.display()));
                }
            }
        }
    }

    fn load(&self) -> Result<RegistryFile> {
        let bytes = match fs::read(&self.file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RegistryFile {
                    v: REGISTRY_VERSION,
                    ports: BTreeMap::new(),
                });
            }
            Err(e) => {
                return Err(e).with_context(|| format!("cannot read {}", self.file.display()));
            }
        };
        let mut reg: RegistryFile = match serde_json::from_slice(&bytes) {
            Ok(reg) => reg,
            Err(e) => return self.recover(&e),
        };
        if reg.v > REGISTRY_VERSION {
            bail!(
                "{} was written by a newer branchpod (v{})",
                self.file.display(),
                reg.v
            );
        }
        // Older formats are upgraded on the next save.
        reg.v = REGISTRY_VERSION;
        Ok(reg)
    }

    /// A damaged registry must not block `up`, `down` and `gc` until someone deletes it by hand.
    /// It is set aside for inspection and replaced by an empty one: running services stay safe
    /// because every new candidate is bind-checked, and pods re-register on their next `up`.
    fn recover(&self, error: &serde_json::Error) -> Result<RegistryFile> {
        let mut aside = self.file.as_os_str().to_owned();
        aside.push(".corrupt");
        let aside = PathBuf::from(aside);
        fs::rename(&self.file, &aside)
            .with_context(|| format!("cannot move damaged {} aside", self.file.display()))?;
        eprintln!(
            "bp: warning: {} was damaged ({error}); moved to {} and starting with an empty registry",
            self.file.display(),
            aside.display()
        );
        Ok(RegistryFile {
            v: REGISTRY_VERSION,
            ports: BTreeMap::new(),
        })
    }

    fn save(&self, reg: &RegistryFile) -> Result<()> {
        fsutil::write_atomic(&self.file, &serde_json::to_vec_pretty(reg)?)
            .with_context(|| format!("cannot write {}", self.file.display()))
    }
}

fn preferred_offset(pod: &str, name: &str) -> u32 {
    let digest = Sha256::new()
        .chain_update(pod)
        .chain_update([0])
        .chain_update(name)
        .finalize();
    u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]])
}

/// True if nothing listens on `port`, checked by binding it on the loopback and wildcard addresses.
///
/// Both are needed: a dev server usually binds loopback while a container runtime publishes on the
/// wildcard address, and on some systems binding one succeeds while the other is taken. Ports
/// reserved by the OS (e.g. Windows excluded port ranges) fail the IPv4 bind and count as busy.
///
/// Not detected: a listener on one specific non-loopback address (e.g. `192.168.1.10:<port>`) on
/// Windows, where binding the wildcard address still succeeds next to it.
pub fn is_free(port: u16) -> bool {
    let v4 = [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED];
    if !v4
        .iter()
        .all(|ip| TcpListener::bind(SocketAddr::from((*ip, port))).is_ok())
    {
        return false;
    }
    // IPv6 may be disabled or unsupported, which surfaces as various errors depending on the
    // platform (address not available, address family not supported). Only "in use" proves busy.
    [Ipv6Addr::LOCALHOST, Ipv6Addr::UNSPECIFIED]
        .iter()
        .all(|ip| {
            !matches!(
                TcpListener::bind(SocketAddr::from((*ip, port))),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    fn registry() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        (dir, reg)
    }

    #[test]
    fn allocation_is_stable_and_released() {
        let (_dir, reg) = registry();
        let a = reg
            .allocate_with("pod-a", "web", [20000, 20099], |_| true)
            .unwrap();
        // Same owner gets the same port back, also after release (derived, not stored).
        assert_eq!(
            reg.allocate_with("pod-a", "web", [20000, 20099], |_| true)
                .unwrap(),
            a
        );
        assert_eq!(reg.release("pod-a", "web").unwrap(), Some(a));
        assert_eq!(reg.release("pod-a", "web").unwrap(), None);
        assert_eq!(
            reg.allocate_with("pod-a", "web", [20000, 20099], |_| true)
                .unwrap(),
            a
        );
    }

    #[test]
    fn owners_never_share_a_port() {
        let (_dir, reg) = registry();
        let mut seen = BTreeSet::new();
        for pod in ["a", "b", "c", "d"] {
            for name in ["web", "vite"] {
                let port = reg
                    .allocate_with(pod, name, [20000, 20007], |_| true)
                    .unwrap();
                assert!(seen.insert(port), "{port} handed out twice");
            }
        }
        assert!(
            reg.allocate_with("e", "web", [20000, 20007], |_| true)
                .is_err()
        );
        assert_eq!(reg.release_pod("a").unwrap().len(), 2);
        assert!(
            reg.allocate_with("e", "web", [20000, 20007], |_| true)
                .is_ok()
        );
    }

    #[test]
    fn owner_keeps_its_port_while_its_service_listens() {
        let (_dir, reg) = registry();
        let first = reg
            .allocate_with("p", "web", [20000, 20003], |_| true)
            .unwrap();
        // A repeated `up`: the port is busy because our own service listens on it.
        let again = reg
            .allocate_with("p", "web", [20000, 20003], |p| p != first)
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(reg.registered("p", "web").unwrap(), Some(first));
        assert_eq!(reg.registered("p", "vite").unwrap(), None);
    }

    #[test]
    fn busy_ports_are_skipped_and_out_of_range_registrations_replaced() {
        let (_dir, reg) = registry();
        let first = reg
            .allocate_with("q", "web", [20000, 20003], |_| true)
            .unwrap();
        // A new owner never gets a busy or registered port.
        let other = reg
            .allocate_with("p", "web", [20000, 20003], |port| port != 20001)
            .unwrap();
        assert!(other != first && other != 20001);
        reg.release("q", "web").unwrap();
        // A changed range drops a registration outside it.
        let third = reg
            .allocate_with("p", "web", [20010, 20010], |_| true)
            .unwrap();
        assert_eq!(third, 20010);
        assert_eq!(
            reg.list().unwrap().keys().copied().collect::<Vec<_>>(),
            [20010]
        );
    }

    #[test]
    fn concurrent_allocations_get_distinct_ports() {
        let dir = tempfile::tempdir().unwrap();
        let home = Arc::new(dir.path().to_path_buf());
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let home = Arc::clone(&home);
                std::thread::spawn(move || {
                    // A separate registry handle per thread, as separate processes would have.
                    Registry::new(&home).allocate_with(
                        &format!("pod-{i}"),
                        "web",
                        [20000, 20007],
                        |_| true,
                    )
                })
            })
            .collect();
        let ports: BTreeSet<u16> = handles
            .into_iter()
            .map(|h| h.join().unwrap().unwrap())
            .collect();
        assert_eq!(ports.len(), 8);
        assert_eq!(Registry::new(&home).list().unwrap().len(), 8);
    }

    #[test]
    fn bind_check_detects_a_listener() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!is_free(port));
        drop(listener);
    }

    #[test]
    fn real_allocation_skips_a_bound_port() {
        let (_dir, reg) = registry();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let busy = listener.local_addr().unwrap().port();
        if busy == u16::MAX {
            return;
        }
        // The range holds the busy port and one more; the busy one must never be returned,
        // whether or not the neighbour happens to be free on this machine.
        let got = reg.allocate("p", "web", [busy, busy + 1]);
        assert!(!matches!(got, Ok(port) if port == busy), "got {got:?}");
    }

    #[test]
    fn damaged_registry_is_set_aside() {
        let (dir, reg) = registry();
        fs::write(dir.path().join("ports.json"), b"").unwrap();
        assert_eq!(reg.release("p", "web").unwrap(), None);
        assert!(dir.path().join("ports.json.corrupt").exists());
        assert!(
            reg.allocate_with("p", "web", [20000, 20001], |_| true)
                .is_ok()
        );
    }

    #[test]
    fn lock_times_out_instead_of_hanging() {
        let (dir, reg) = registry();
        let holder = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.path().join("ports.lock"))
            .unwrap();
        holder.lock().unwrap();
        let reg = Registry {
            lock_timeout: Duration::from_millis(100),
            ..reg
        };
        let started = Instant::now();
        let err = reg.list().unwrap_err();
        assert!(format!("{err:#}").contains("locked by another bp"));
        assert!(started.elapsed() >= Duration::from_millis(100));
        drop(holder);
        assert!(reg.list().is_ok());
    }

    #[test]
    fn rejects_bad_range_and_newer_registry() {
        let (dir, reg) = registry();
        assert!(reg.allocate("p", "web", [10, 9]).is_err());
        fs::write(dir.path().join("ports.json"), r#"{"v":2,"ports":{}}"#).unwrap();
        assert!(
            reg.allocate_with("p", "web", [20000, 20001], |_| true)
                .is_err()
        );
    }
}
