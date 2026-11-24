//! Service logs: one live file per service plus the segments rotated away from it.
//!
//! The live file is renamed to `<log>.<seq>` when it has grown enough, with `seq` growing by one
//! each time, and a new live file takes its place. Only the newest segments are kept. The bytes are
//! what the service wrote; nothing is decoded on the way to the file.
//!
//! [`read`] knows nothing about who writes the files: it opens them for a moment, takes no lock and
//! so cannot disturb the service.

use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

pub(crate) mod read;

/// Path of segment `seq` of `log`.
fn segment(log: &Path, seq: u64) -> PathBuf {
    let mut path = OsString::from(log);
    path.push(format!(".{seq}"));
    path.into()
}

/// Sequence numbers of the segments of `log` that exist, ascending. Empty also when the directory
/// is gone.
fn segments(log: &Path) -> io::Result<Vec<u64>> {
    let (Some(dir), Some(name)) = (log.parent(), log.file_name()) else {
        return Ok(Vec::new());
    };
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut found = Vec::new();
    for entry in entries {
        let file = entry?.file_name();
        let Some(rest) = file
            .as_encoded_bytes()
            .strip_prefix(name.as_encoded_bytes())
            .and_then(|rest| rest.strip_prefix(b"."))
        else {
            continue;
        };
        // Only what `segment` produces: no sign, no leading zeros.
        if let Some(seq) = std::str::from_utf8(rest)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            && seq.to_string().as_bytes() == rest
        {
            found.push(seq);
        }
    }
    found.sort_unstable();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_only_the_segments_of_this_log_in_numeric_order() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "web.log",
            "web.log.2",
            "web.log.10",
            "web.log.9",
            "web.log.01",
            "web.log.+3",
            "web.log.x",
            "web.log.4.part",
            "web2.log.5",
            "web.log2.6",
        ] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        let log = dir.path().join("web.log");
        assert_eq!(segments(&log).unwrap(), [2, 9, 10]);
        assert_eq!(segment(&log, 2), dir.path().join("web.log.2"));
        assert!(segments(&dir.path().join("no/web.log")).unwrap().is_empty());
    }
}
