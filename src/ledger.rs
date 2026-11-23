//! Write-ahead ledger of the resources created for one pod.
//!
//! An append-only JSON Lines file. The intent to create a resource is written (and flushed to
//! disk) before the resource is created, its identifying data after. A crash between the two
//! leaves a `pending` resource that teardown still visits, so partial creation is never invisible.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LEDGER_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Git,
    File,
    Port,
    Command,
    Service,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Event {
    /// About to create the resource.
    Intent,
    /// Created; `data` holds whatever teardown needs (path, port, captured id, pid).
    Created {
        #[serde(default)]
        data: Value,
    },
    /// Released by teardown or gc.
    Destroyed,
    /// Creation or teardown failed; the resource may exist partially.
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub v: u32,
    pub ts_ms: u64,
    pub kind: Kind,
    pub name: String,
    #[serde(flatten)]
    pub event: Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Intent recorded, creation not confirmed: may or may not exist.
    Pending,
    Live,
    /// A step failed; may exist partially.
    Failed,
    Destroyed,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResourceState {
    pub kind: Kind,
    pub name: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub data: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct Ledger {
    path: PathBuf,
    file: File,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Ledger> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open ledger {}", path.display()))?;
        let ledger = Ledger {
            path: path.to_path_buf(),
            file,
        };
        ledger.repair_torn_tail()?;
        Ok(ledger)
    }

    pub fn record(&mut self, kind: Kind, name: &str, event: Event) -> Result<()> {
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
pub fn read(path: &Path) -> Result<Vec<Entry>> {
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
pub fn state(entries: &[Entry]) -> Vec<ResourceState> {
    let mut resources: Vec<ResourceState> = Vec::new();
    for entry in entries {
        let pos = resources
            .iter()
            .position(|r| r.kind == entry.kind && r.name == entry.name);
        match (&entry.event, pos) {
            (Event::Intent, pos) => {
                if let Some(pos) = pos {
                    resources.remove(pos);
                }
                resources.push(ResourceState {
                    kind: entry.kind,
                    name: entry.name.clone(),
                    status: Status::Pending,
                    data: Value::Null,
                    error: None,
                });
            }
            (Event::Created { data }, Some(pos)) => {
                let r = &mut resources[pos];
                r.status = Status::Live;
                r.data = data.clone();
                r.error = None;
            }
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
                    Event::Created { data } => data.clone(),
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
        ledger.record(Kind::Port, "web", Event::Intent).unwrap();
        ledger
            .record(
                Kind::Port,
                "web",
                Event::Created {
                    data: json!({"port": 8003}),
                },
            )
            .unwrap();
        ledger.record(Kind::Command, "app", Event::Intent).unwrap();
        ledger.record(Kind::File, ".env", Event::Intent).unwrap();
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
    fn recreation_moves_resource_to_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap();
        for (kind, name) in [(Kind::Port, "web"), (Kind::Service, "vite")] {
            ledger.record(kind, name, Event::Intent).unwrap();
            ledger
                .record(kind, name, Event::Created { data: Value::Null })
                .unwrap();
        }
        ledger.record(Kind::Port, "web", Event::Destroyed).unwrap();
        ledger.record(Kind::Port, "web", Event::Intent).unwrap();
        drop(ledger);

        let s = state(&read(&path).unwrap());
        assert_eq!(
            s.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["vite", "web"]
        );
        assert_eq!(s[1].status, Status::Pending);
    }

    #[test]
    fn torn_last_line_is_skipped_and_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.record(Kind::Port, "web", Event::Intent).unwrap();
        drop(ledger);
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"v":1,"ts_ms":1,"kind":"po"#).unwrap();
        drop(f);

        assert_eq!(read(&path).unwrap().len(), 1);

        let mut ledger = Ledger::open(&path).unwrap();
        ledger.record(Kind::Service, "vite", Event::Intent).unwrap();
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
