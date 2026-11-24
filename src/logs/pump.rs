//! The writing side of a log: a thread that moves what a service writes from a pipe to its log file
//! and rotates the file.
//!
//! The service's stdout and stderr are one pipe, so their order is kept. A file handle the service
//! writes to directly could not be rotated on Windows: an append-only handle is not usable by MSYS
//! programs, and after truncating a file behind any other handle the service goes on writing at its
//! old offset.
//!
//! The service must never wait for its log: whatever goes wrong with the file, the pipe is read on,
//! and output that cannot be written is dropped.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, PipeReader, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use super::{segment, segments};
use crate::fsutil;

const CHUNK: usize = 64 * 1024;

/// How often at most a problem with the log is reported.
const REPORT_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
struct Limits {
    /// The live file is rotated before it grows beyond this.
    rotate_at: u64,
    /// Segments kept besides the live file.
    keep: usize,
    /// After a failed rotation, how much is written before the next attempt.
    retry_after: u64,
    /// While it cannot be rotated the live file grows up to this; then it is emptied, as the
    /// newest output is worth more than the oldest.
    hard_limit: u64,
}

const LIMITS: Limits = Limits {
    rotate_at: 10 << 20,
    keep: 2,
    retry_after: 1 << 20,
    hard_limit: 50 << 20,
};

/// A running pump.
pub(crate) struct Pump {
    /// Never receives: it is disconnected when the pump ends.
    done: Receiver<()>,
}

impl Pump {
    /// Opens `log` for appending and starts a pump for it. The returned end of the pipe is for the
    /// service alone: the pump ends once every copy of it is closed.
    pub(crate) fn start(log: &Path) -> io::Result<(Pump, PipeWriter)> {
        let mut out = Output::open(log, LIMITS)?;
        let (reader, writer) = io::pipe()?;
        let (alive, done) = channel::<()>();
        thread::Builder::new()
            .name("log pump".into())
            .spawn(move || {
                let _alive = alive;
                run(reader, &mut out);
            })?;
        Ok((Pump { done }, writer))
    }

    /// Waits until everything the service wrote is in the log, up to `deadline`; whether it is.
    /// A process the service left behind may hold the pipe open for as long as it lives, so this
    /// must not wait without a limit.
    pub(crate) fn wait(&self, deadline: Instant) -> bool {
        let limit = deadline.saturating_duration_since(Instant::now());
        self.done.recv_timeout(limit) == Err(RecvTimeoutError::Disconnected)
    }
}

fn run(mut reader: PipeReader, out: &mut Output) {
    let mut buf = vec![0; CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => out.write(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                out.report(&format!("cannot read the service's output: {e}"));
                return;
            }
        }
    }
}

struct Output {
    log: PathBuf,
    limits: Limits,
    file: File,
    size: u64,
    /// Size at which to rotate next; more than `rotate_at` after a failure.
    rotate_at: u64,
    reported: Option<Instant>,
    /// Bytes dropped since the last successful write.
    dropped: u64,
}

impl Output {
    fn open(log: &Path, limits: Limits) -> io::Result<Output> {
        if let Some(dir) = log.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = open_append(log)?;
        let size = file.metadata()?.len();
        Ok(Output {
            log: log.to_path_buf(),
            limits,
            file,
            size,
            rotate_at: limits.rotate_at,
            reported: None,
            dropped: 0,
        })
    }

    /// Writes `bytes`, or drops them if that is not possible.
    fn write(&mut self, bytes: &[u8]) {
        let len = bytes.len() as u64;
        if self.size > 0 && self.size + len > self.rotate_at {
            match self.rotate() {
                Ok(()) => self.rotate_at = self.limits.rotate_at,
                Err(e) => {
                    self.report(&format!("cannot rotate the log: {e}"));
                    self.rotate_at = self.size + len + self.limits.retry_after;
                }
            }
        }
        if self.size + len > self.limits.hard_limit
            && let Err(e) = self.empty()
        {
            self.report(&format!("the log is full, dropping output: {e}"));
            self.dropped += len;
            return;
        }
        match self.file.write_all(bytes) {
            Ok(()) => {
                self.size += len;
                if self.dropped > 0 {
                    // Even if the failure was reported: on a full disk that report is lost too.
                    self.say(&format!("{} bytes of output were dropped", self.dropped));
                    self.dropped = 0;
                }
            }
            Err(e) => {
                self.report(&format!("cannot write, dropping output: {e}"));
                // Part of it may have been written.
                let before = self.size;
                if let Ok(meta) = self.file.metadata() {
                    self.size = meta.len();
                }
                self.dropped += len.saturating_sub(self.size.saturating_sub(before));
            }
        }
    }

    /// Our own handle stays open over the rename (it shares deletion, which renaming needs on
    /// Windows), so there is no moment at which nothing could be written.
    fn rotate(&mut self) -> io::Result<()> {
        let seqs = segments(&self.log)?;
        let next = segment(&self.log, seqs.last().map_or(1, |last| last + 1));
        match fs::rename(&self.log, &next) {
            Ok(()) => {}
            // Somebody removed the log: start a new one.
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        match open_append(&self.log) {
            Ok(file) => self.file = file,
            Err(e) => {
                // We go on writing to the old file: readers must find it under its old name. A
                // follower that took it for a finished segment in between reads it once more.
                let _ = fs::rename(&next, &self.log);
                return Err(e);
            }
        }
        self.size = self.file.metadata()?.len();
        // Including those that could not be removed the last time.
        let excess = (seqs.len() + 1).saturating_sub(self.limits.keep);
        for &seq in seqs.iter().take(excess) {
            let _ = fs::remove_file(segment(&self.log, seq));
        }
        Ok(())
    }

    /// Our handle only appends, which is not enough to truncate on Windows.
    fn empty(&mut self) -> io::Result<()> {
        OpenOptions::new().write(true).open(&self.log)?.set_len(0)?;
        self.size = self.file.metadata()?.len();
        self.rotate_at = self.limits.rotate_at;
        Ok(())
    }

    /// Says it in the supervisor's own log, but not too often.
    fn report(&mut self, problem: &str) {
        if self.reported.is_some_and(|at| at.elapsed() < REPORT_EVERY) {
            return;
        }
        self.reported = Some(Instant::now());
        self.say(problem);
    }

    fn say(&self, text: &str) {
        // Not `eprintln!`, which panics if stderr cannot be written to.
        let _ = writeln!(io::stderr(), "bp supervise: {}: {text}", self.log.display());
    }
}

/// A log may hold secrets a service printed: on Unix only its owner may read it.
fn open_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    fsutil::owner_only(&mut options);
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::read::{Chunk, Cursor, Lines, Snapshot};

    const SMALL: Limits = Limits {
        rotate_at: 100,
        keep: 2,
        retry_after: 20,
        hard_limit: 300,
    };

    fn all(log: &Path) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        Snapshot::open(log)
            .unwrap()
            .unwrap()
            .tail(Lines::All, |b| {
                out.extend(b);
                Ok(())
            })
            .unwrap();
        out
    }

    /// Not on macOS: there std creates a pipe and marks it close-on-exec in two steps, so a process
    /// that another test starts in between would hold it.
    fn wait_for_end(pump: &Pump) {
        if cfg!(not(target_os = "macos")) {
            assert!(pump.wait(Instant::now() + Duration::from_secs(10)));
        }
    }

    fn wait_for(log: &Path, expected: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while fs::read(log).unwrap() != expected {
            assert!(Instant::now() < deadline, "{:?}", fs::read(log));
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn moves_everything_until_the_last_writer_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        // A device name on some versions of Windows without the prefix.
        let log = dir.path().join("logs").join("service-con.log");
        let (pump, mut writer) = Pump::start(&log).unwrap();
        let mut second = writer.try_clone().unwrap();
        writer.write_all(b"out\n").unwrap();
        drop(writer);
        second.write_all(b"err \xff\n").unwrap();
        assert!(!pump.wait(Instant::now() + Duration::from_millis(50)));
        drop(second);
        wait_for(&log, b"out\nerr \xff\n");
        wait_for_end(&pump);

        // Appended to by the next run.
        let (pump, mut writer) = Pump::start(&log).unwrap();
        writer.write_all(b"again\n").unwrap();
        drop(writer);
        wait_for(&log, b"out\nerr \xff\nagain\n");
        wait_for_end(&pump);
    }

    #[cfg(unix)]
    #[test]
    fn logs_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("web.log");
        Output::open(&log, SMALL).unwrap();
        let mode = fs::metadata(&log).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn rotates_and_keeps_the_newest_segments() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("web.log");
        let mut out = Output::open(&log, SMALL).unwrap();
        for i in 0..50 {
            out.write(format!("line {i:04}\n").as_bytes()); // 10 bytes
        }
        assert_eq!(segments(&log).unwrap(), [3, 4]);
        let text = String::from_utf8(all(&log)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.first(), Some(&"line 0020"));
        assert_eq!(lines.last(), Some(&"line 0049"));
        assert_eq!(lines.len(), 30);

        // The numbering goes on after a restart.
        let mut out = Output::open(&log, SMALL).unwrap();
        out.write(&[b'x'; 90]);
        assert_eq!(segments(&log).unwrap(), [4, 5]);
    }

    #[test]
    fn a_chunk_larger_than_the_limit_is_written_whole() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("web.log");
        let mut out = Output::open(&log, SMALL).unwrap();
        out.write(&[b'x'; 150]);
        assert_eq!(fs::metadata(&log).unwrap().len(), 150);
        assert!(segments(&log).unwrap().is_empty());
        out.write(b"y");
        assert_eq!(fs::read(&log).unwrap(), b"y");
    }

    #[test]
    fn a_follower_sees_every_line_once_while_the_log_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("web.log");
        // Enough segments that a slow reader does not lose any.
        let limits = Limits {
            rotate_at: 2000,
            keep: 1000,
            ..SMALL
        };
        let limits = Limits {
            hard_limit: 1 << 30,
            ..limits
        };
        let mut out = Output::open(&log, limits).unwrap();
        let writer = thread::spawn(move || {
            for i in 0..5000 {
                out.write(format!("line {i:06}\n").as_bytes());
            }
        });
        let mut cursor = Cursor::start(&log);
        let mut seen = Vec::new();
        let mut buf = [0; 777];
        let mut finished = false;
        loop {
            match cursor.read(&mut buf).unwrap() {
                Chunk::Data(n) => seen.extend(&buf[..n]),
                Chunk::Skipped(n) => panic!("{n} segments skipped"),
                Chunk::End if finished => break,
                Chunk::End => finished = writer.is_finished(),
            }
        }
        writer.join().unwrap();
        let text = String::from_utf8(seen).unwrap();
        let expected: String = (0..5000).map(|i| format!("line {i:06}\n")).collect();
        assert!(text == expected, "lines lost or repeated");
        assert!(segments(&log).unwrap().len() > 10);
    }

    #[test]
    fn a_removed_log_is_started_anew() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("web.log");
        let mut out = Output::open(&log, SMALL).unwrap();
        out.write(&[b'x'; 90]);
        fs::remove_file(&log).unwrap();
        out.write(&[b'y'; 90]);
        assert_eq!(fs::read(&log).unwrap(), [b'y'; 90]);
    }

    /// Opened without sharing deletion, as many programs do: it cannot be renamed.
    #[cfg(windows)]
    #[test]
    fn a_log_that_cannot_be_renamed_grows_to_the_hard_limit_and_is_emptied() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ_WRITE: u32 = 3;

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("web.log");
        let mut out = Output::open(&log, SMALL).unwrap();
        out.write(b"first");
        let blocker = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ_WRITE)
            .open(&log)
            .unwrap();
        for _ in 0..29 {
            out.write(&[b'x'; 10]);
        }
        assert!(segments(&log).unwrap().is_empty());
        assert_eq!(fs::metadata(&log).unwrap().len(), 295);
        out.write(b"newest output");
        assert_eq!(fs::read(&log).unwrap(), b"newest output");

        drop(blocker);
        out.write(&[b'y'; 95]);
        assert_eq!(segments(&log).unwrap(), [1]);
        assert_eq!(fs::read(&log).unwrap(), [b'y'; 95]);
    }

    /// Writing fails on a handle that cannot write, the stand-in for a full disk.
    #[test]
    fn output_that_cannot_be_written_is_dropped_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("web.log");
        let mut out = Output::open(&log, SMALL).unwrap();
        out.write(b"before ");
        let file = std::mem::replace(&mut out.file, File::open(&log).unwrap());
        out.write(b"lost");
        out.write(b" too");
        assert_eq!(out.dropped, 8);
        out.file = file;
        out.write(b"after");
        assert_eq!(out.dropped, 0);
        assert_eq!(fs::read(&log).unwrap(), b"before after");
    }

    #[test]
    fn the_pipe_is_read_on_when_output_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("web.log");
        let mut out = Output::open(&log, SMALL).unwrap();
        out.file = File::open(&log).unwrap();
        let (reader, mut writer) = io::pipe().unwrap();
        thread::spawn(move || run(reader, &mut out));
        // More than any pipe holds: this would block if nobody read.
        writer.write_all(&vec![b'x'; 1 << 20]).unwrap();
    }
}
