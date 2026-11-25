//! Write-ahead ledger of the resources created for one pod.
//!
//! An append-only JSON Lines file. The intent to create a resource is written (and flushed to
//! disk) before the resource is created, its identifying data after. A crash between the two
//! leaves a `pending` resource that teardown still visits, so partial creation is never invisible.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::fsutil;

pub(crate) const LEDGER_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Git,
    File,
    Port,
    Command,
    Service,
    /// The pod's supervisor process, which runs its services.
    Supervisor,
}

impl Kind {
    /// The name used in the ledger and in output.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::Git => "git",
            Kind::File => "file",
            Kind::Port => "port",
            Kind::Command => "command",
            Kind::Service => "service",
            Kind::Supervisor => "supervisor",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // pad, not write_str: honours width and alignment ({:<8}) in tables.
        f.pad(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub(crate) enum Event {
    /// About to create the resource. `data` holds what teardown needs to undo a creation that
    /// was interrupted (the planned path, for example).
    Intent {
        #[serde(default, skip_serializing_if = "Value::is_null")]
        data: Value,
    },
    /// Created; `data` holds whatever teardown needs (path, port, captured id, pid).
    Created {
        #[serde(default)]
        data: Value,
    },
    /// Teardown is about to move the resource (a directory renamed to a trash name before it is
    /// deleted, an edited file to where it is kept): `data` replaces the recorded data and names the
    /// new location, so an interruption at any point is finished from the ledger. Status and order
    /// are unchanged.
    Moved { data: Value },
    /// Released by teardown or gc.
    Destroyed,
    /// Creation or teardown failed; the resource may exist partially.
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) v: u32,
    pub(crate) ts_ms: u64,
    pub(crate) kind: Kind,
    pub(crate) name: String,
    #[serde(flatten)]
    pub(crate) event: Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    /// Intent recorded, creation not confirmed: may or may not exist.
    Pending,
    Live,
    /// A step failed; may exist partially.
    Failed,
    Destroyed,
}

impl Status {
    /// The name used in output.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Live => "live",
            Status::Failed => "failed",
            Status::Destroyed => "destroyed",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // pad, not write_str: honours width and alignment ({:<8}) in tables.
        f.pad(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ResourceState {
    pub(crate) kind: Kind,
    pub(crate) name: String,
    pub(crate) status: Status,
    #[serde(
        skip_serializing_if = "Value::is_null",
        serialize_with = "without_private_data"
    )]
    pub(crate) data: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

/// Fields of a resource's data that stay in the ledger. The state is printed by `bp status --json`,
/// and the hash of a short secret (`fingerprint`) gives it away; `boot` tells nobody else anything.
pub(crate) const PRIVATE_DATA: [&str; 2] = ["fingerprint", "boot"];

fn without_private_data<S: Serializer>(data: &Value, serializer: S) -> Result<S::Ok, S::Error> {
    match data {
        Value::Object(fields) => serializer.collect_map(
            fields
                .iter()
                .filter(|(name, _)| !PRIVATE_DATA.contains(&name.as_str())),
        ),
        _ => data.serialize(serializer),
    }
}

pub(crate) struct Ledger {
    path: PathBuf,
    file: File,
}

#[cfg(test)]
impl Event {
    /// An intent without data.
    pub(crate) fn intent() -> Event {
        Event::Intent { data: Value::Null }
    }
}

impl Ledger {
    pub(crate) fn open(path: &Path) -> Result<Ledger> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        // It holds the output of commands and the fingerprints of files that may be secrets.
        fsutil::owner_only(&mut options);
        let file = options
            .open(path)
            .with_context(|| format!("cannot open ledger {}", path.display()))?;
        // Also one that an older `bp` created. A file system without permissions may refuse.
        #[cfg(unix)]
        let _ = file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600));
        let ledger = Ledger {
            path: path.to_path_buf(),
            file,
        };
        ledger.repair_torn_tail()?;
        Ok(ledger)
    }

    pub(crate) fn record(&mut self, kind: Kind, name: &str, event: Event) -> Result<()> {
        let entry = Entry {
            v: LEDGER_VERSION,
            ts_ms: now_ms(),
            kind,
            name: name.to_owned(),
            event,
        };
        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');
        self.file
            .write_all(&line)
            .and_then(|()| self.file.sync_data())
            .with_context(|| format!("cannot write ledger {}", self.path.display()))
    }

    /// A crash mid-write can leave a last line without its newline. Terminate it so the next
    /// record starts on a fresh line; `read` skips the torn fragment.
    fn repair_torn_tail(&self) -> Result<()> {
        let bytes = fs::read(&self.path)?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            (&self.file).write_all(b"\n")?;
            self.file.sync_data()?;
        }
        Ok(())
    }
}

/// Reads all entries. A malformed last line is a torn write and is skipped; a malformed line in
/// the middle means the file was damaged and is reported.
pub(crate) fn read(path: &Path) -> Result<Vec<Entry>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot open ledger {}", path.display())),
    };
    let lines: Vec<String> = BufReader::new(file).lines().collect::<Result<_, _>>()?;
    let last_content = lines.iter().rposition(|l| !l.trim().is_empty());

    let mut entries = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(entry) if entry.v > LEDGER_VERSION => bail!(
                "{}:{}: written by a newer branchpod (ledger v{})",
                path.display(),
                i + 1,
                entry.v
            ),
            Ok(entry) => entries.push(entry),
            // Torn: either the final line, or a fragment that the next `open` terminated.
            Err(_) if Some(i) == last_content || !line.trim_end().ends_with('}') => continue,
            Err(e) => bail!("{}:{}: corrupt ledger entry: {e}", path.display(), i + 1),
        }
    }
    Ok(entries)
}

/// Folds entries into the current state of each resource, ordered by its latest intent.
/// Teardown visits every non-destroyed resource in reverse of this order.
pub(crate) fn state(entries: &[Entry]) -> Vec<ResourceState> {
    let mut resources: Vec<ResourceState> = Vec::new();
    for entry in entries {
        let pos = resources
            .iter()
            .position(|r| r.kind == entry.kind && r.name == entry.name);
        match (&entry.event, pos) {
            (Event::Intent { data }, pos) => {
                if let Some(pos) = pos {
                    resources.remove(pos);
                }
                resources.push(ResourceState {
                    kind: entry.kind,
                    name: entry.name.clone(),
                    status: Status::Pending,
                    data: data.clone(),
                    error: None,
                });
            }
            (Event::Created { data }, Some(pos)) => {
                let r = &mut resources[pos];
                r.status = Status::Live;
                r.data = data.clone();
                r.error = None;
            }
            (Event::Moved { data }, Some(pos)) => resources[pos].data = data.clone(),
            (Event::Destroyed, Some(pos)) => {
                resources[pos].status = Status::Destroyed;
                resources[pos].error = None;
            }
            (Event::Failed { error }, Some(pos)) => {
                resources[pos].status = Status::Failed;
                resources[pos].error = Some(error.clone());
            }
            // An event without a preceding intent can only come from a damaged file; keep it
            // visible rather than drop it.
            (event, None) => resources.push(ResourceState {
                kind: entry.kind,
                name: entry.name.clone(),
                status: match event {
                    Event::Destroyed => Status::Destroyed,
                    Event::Created { .. } => Status::Live,
                    _ => Status::Failed,
                },
                data: match event {
                    Event::Created { data } | Event::Moved { data } => data.clone(),
                    _ => Value::Null,
                },
                error: match event {
                    Event::Failed { error } => Some(error.clone()),
                    _ => None,
                },
            }),
        }
    }
    resources
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_and_folds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pod").join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.record(Kind::Port, "web", Event::intent()).unwrap();
        ledger
            .record(
                Kind::Port,
                "web",
                Event::Created {
                    data: json!({"port": 8003}),
                },
            )
            .unwrap();
        ledger
            .record(Kind::Command, "app", Event::intent())
            .unwrap();
        ledger.record(Kind::File, ".env", Event::intent()).unwrap();
        ledger
            .record(
                Kind::File,
                ".env",
                Event::Failed {
                    error: "denied".into(),
                },
            )
            .unwrap();
        drop(ledger);

        let s = state(&read(&path).unwrap());
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].kind, s[0].status), (Kind::Port, Status::Live));
        assert_eq!(s[0].data, json!({"port": 8003}));
        assert_eq!((s[1].kind, s[1].status), (Kind::Command, Status::Pending));
        assert_eq!(s[2].status, Status::Failed);
        assert_eq!(s[2].error.as_deref(), Some("denied"));
    }

    #[test]
    fn intent_data_survives_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap();
        let planned = json!({"path": "/wt/.env"});
        ledger
            .record(
                Kind::File,
                ".env",
                Event::Intent {
                    data: planned.clone(),
                },
            )
            .unwrap();
        ledger
            .record(
                Kind::File,
                ".env",
                Event::Failed {
                    error: "disk full".into(),
                },
            )
            .unwrap();
        drop(ledger);

        let s = state(&read(&path).unwrap());
        assert_eq!(s[0].status, Status::Failed);
        assert_eq!(s[0].data, planned);
        // An intent without data is written exactly as before.
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("\"data\":null"), "{text}");
    }

    #[test]
    fn recreation_moves_resource_to_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap();
        for (kind, name) in [(Kind::Port, "web"), (Kind::Service, "vite")] {
            ledger.record(kind, name, Event::intent()).unwrap();
            ledger
                .record(kind, name, Event::Created { data: Value::Null })
                .unwrap();
        }
        ledger.record(Kind::Port, "web", Event::Destroyed).unwrap();
        ledger.record(Kind::Port, "web", Event::intent()).unwrap();
        drop(ledger);

        let s = state(&read(&path).unwrap());
        assert_eq!(
            s.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["vite", "web"]
        );
        assert_eq!(s[1].status, Status::Pending);
    }

    #[test]
    fn moved_replaces_data_and_keeps_status_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap();
        for name in ["vendor", "web"] {
            ledger.record(Kind::File, name, Event::intent()).unwrap();
            ledger
                .record(Kind::File, name, Event::Created { data: json!({}) })
                .unwrap();
        }
        let trash = json!({"path": "/wt/vendor.bp-trash-1"});
        ledger
            .record(
                Kind::File,
                "vendor",
                Event::Moved {
                    data: trash.clone(),
                },
            )
            .unwrap();
        drop(ledger);

        let s = state(&read(&path).unwrap());
        assert_eq!((s[0].name.as_str(), s[0].status), ("vendor", Status::Live));
        assert_eq!(s[0].data, trash);
    }

    #[test]
    fn torn_last_line_is_skipped_and_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.record(Kind::Port, "web", Event::intent()).unwrap();
        drop(ledger);
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"v":1,"ts_ms":1,"kind":"po"#).unwrap();
        drop(f);

        assert_eq!(read(&path).unwrap().len(), 1);

        let mut ledger = Ledger::open(&path).unwrap();
        ledger
            .record(Kind::Service, "vite", Event::intent())
            .unwrap();
        drop(ledger);
        let s = state(&read(&path).unwrap());
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].name, "vite");
    }

    #[test]
    fn corruption_in_the_middle_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        fs::write(
            &path,
            "{\"v\":1,\"ts_ms\":1,\"kind\":\"port\",\"name\":\"a\",\"op\":\"intent\"}\n\
             {\"v\":1,\"ts_ms\":1,\"kind\":\"bogus\",\"name\":\"a\",\"op\":\"intent\"}\n\
             {\"v\":1,\"ts_ms\":1,\"kind\":\"port\",\"name\":\"b\",\"op\":\"intent\"}\n",
        )
        .unwrap();
        assert!(read(&path).is_err());
    }

    #[test]
    fn a_fingerprint_is_kept_but_not_shown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.record(Kind::File, ".env", Event::intent()).unwrap();
        let data = json!({"path": "/wt/.env", "boot": "b", "fingerprint": {"size": 3}});
        let created = Event::Created { data: data.clone() };
        ledger.record(Kind::File, ".env", created).unwrap();
        drop(ledger);

        let s = state(&read(&path).unwrap());
        assert_eq!(s[0].data, data);
        let shown = serde_json::to_value(&s[0]).unwrap();
        assert_eq!(shown["data"], json!({"path": "/wt/.env"}));
    }

    #[cfg(unix)]
    #[test]
    fn only_the_user_can_read_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mode = || fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        drop(Ledger::open(&path).unwrap());
        assert_eq!(mode(), 0o600);
        // From an older `bp`.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        drop(Ledger::open(&path).unwrap());
        assert_eq!(mode(), 0o600);
    }

    #[test]
    fn names_line_up_in_tables() {
        assert_eq!(format!("{:<8}|", Kind::Port), "port    |");
        assert_eq!(format!("{:>9}|", Status::Live), "     live|");
    }

    #[test]
    fn newer_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        fs::write(
            &path,
            "{\"v\":2,\"ts_ms\":1,\"kind\":\"port\",\"name\":\"a\",\"op\":\"intent\"}\n",
        )
        .unwrap();
        assert!(read(&path).is_err());
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(&dir.path().join("none.jsonl")).unwrap().is_empty());
    }
}
