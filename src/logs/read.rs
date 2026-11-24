//! Reading a log: its last lines, and following it across rotations.
//!
//! A position is a generation and an offset. The live file is generation `n` until it is renamed to
//! segment `n`; the next live file is then generation `n + 1`. A reader at generation `n` therefore
//! finishes segment `n` once that exists and goes on with `n + 1`, without missing or repeating a
//! byte, however the rotation and its polling interleave.

use std::fs::File;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::{segment, segments};
use crate::fsutil::retry;

/// How often a follower looks for new output.
const POLL: Duration = Duration::from_millis(200);

/// Once the service is gone, how long the log must stay unchanged to count as read to the end. The
/// writer may be a moment behind the process that produced the output.
const QUIET: Duration = Duration::from_millis(500);

/// Once the service is gone, how long its log is read at most: a process it left behind may go on
/// writing for ever.
const DRAIN: Duration = Duration::from_secs(2);

const CHUNK: usize = 64 * 1024;

/// How many of the last lines to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lines {
    All,
    Last(u64),
}

/// The files of a log as they were at one moment. They are held open, so reading them is not
/// affected by a rotation in between.
pub(crate) struct Snapshot {
    log: PathBuf,
    /// Oldest first; the live file is the last one. Never empty.
    parts: Vec<Part>,
}

struct Part {
    generation: u64,
    file: File,
    len: u64,
}

impl Snapshot {
    /// `None` when there is no log.
    pub(crate) fn open(log: &Path) -> io::Result<Option<Snapshot>> {
        'again: loop {
            let seqs = retry(|| segments(log))?;
            let mut parts: Vec<Part> = Vec::new();
            for &seq in &seqs {
                match open_file(&segment(log, seq))? {
                    Some((file, len)) => parts.push(Part {
                        generation: seq,
                        file,
                        len,
                    }),
                    // Removed as too old since it was listed: nothing before it counts either.
                    None => parts.clear(),
                }
            }
            let generation = seqs.last().map_or(1, |last| last + 1);
            let live = open_file(log)?;
            // Opened after a rotation that the list does not know.
            if retry(|| segments(log))?.last() != seqs.last() {
                continue 'again;
            }
            match live {
                Some((file, len)) => parts.push(Part {
                    generation,
                    file,
                    len,
                }),
                // No live file: between a rotation and the next write, or no log at all.
                None if parts.is_empty() => return Ok(None),
                None => {}
            }
            return Ok(Some(Snapshot {
                log: log.to_path_buf(),
                parts,
            }));
        }
    }

    /// Passes the last `lines` to `sink`; whether there was anything before them.
    pub(crate) fn tail(
        &mut self,
        lines: Lines,
        mut sink: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<bool> {
        let (first, offset) = match lines {
            Lines::All => (0, 0),
            Lines::Last(n) => self.start_of_last(n)?,
        };
        let mut buf = vec![0; CHUNK];
        for (i, part) in self.parts.iter_mut().enumerate().skip(first) {
            let from = if i == first { offset } else { 0 };
            part.file.seek(SeekFrom::Start(from))?;
            let mut left = part.len.saturating_sub(from);
            while left > 0 {
                let want = left.min(CHUNK as u64) as usize;
                let n = part.file.read(&mut buf[..want])?;
                if n == 0 {
                    break; // truncated behind our back
                }
                left -= n as u64;
                sink(&buf[..n])?;
            }
        }
        Ok((first, offset) != (0, 0))
    }

    /// Where following goes on after this snapshot.
    pub(crate) fn end(&self) -> Cursor {
        let last = self.parts.last().expect("never empty");
        Cursor {
            log: self.log.clone(),
            generation: last.generation,
            offset: last.len,
        }
    }

    /// Part and offset at which the last `n` lines start. The files are one stream: a line may
    /// cross from one into the next. A newline that ends the stream starts no further line.
    fn start_of_last(&mut self, n: u64) -> io::Result<(usize, u64)> {
        if n == 0 {
            let last = self.parts.len() - 1;
            return Ok((last, self.parts[last].len));
        }
        let mut newlines = 0;
        let mut at_end = true;
        let mut buf = vec![0; CHUNK];
        for (i, part) in self.parts.iter_mut().enumerate().rev() {
            let mut end = part.len;
            while end > 0 {
                let start = end.saturating_sub(CHUNK as u64);
                let block = &mut buf[..(end - start) as usize];
                part.file.seek(SeekFrom::Start(start))?;
                part.file.read_exact(block)?;
                for (k, &byte) in block.iter().enumerate().rev() {
                    if byte == b'\n' && !at_end {
                        newlines += 1;
                        if newlines == n {
                            return Ok((i, start + k as u64 + 1));
                        }
                    }
                    at_end = false;
                }
                end = start;
            }
        }
        Ok((0, 0))
    }
}

/// A position in a log that is being followed. Files are opened only for the moment of a read, so a
/// follower that is idle holds nothing that `bp down` has to remove.
#[derive(Debug)]
pub(crate) struct Cursor {
    log: PathBuf,
    generation: u64,
    offset: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Chunk {
    Data(usize),
    /// Nothing new.
    End,
    /// So many whole segments were removed before they could be read.
    Skipped(u64),
}

impl Cursor {
    /// The beginning of a log that does not exist yet.
    pub(crate) fn start(log: &Path) -> Cursor {
        Cursor {
            log: log.to_path_buf(),
            generation: 1,
            offset: 0,
        }
    }

    pub(crate) fn read(&mut self, buf: &mut [u8]) -> io::Result<Chunk> {
        loop {
            let own = segment(&self.log, self.generation);
            if let Some((file, len)) = open_file(&own)? {
                match self.read_from(file, len, buf)? {
                    0 => {
                        self.generation += 1;
                        self.offset = 0;
                        continue;
                    }
                    n => return Ok(Chunk::Data(n)),
                }
            }
            let live = open_file(&self.log)?;
            // Whatever was opened is our generation only if it has not been rotated by now.
            let seqs = retry(|| segments(&self.log))?;
            if seqs.contains(&self.generation) {
                continue;
            }
            if let Some(&next) = seqs.iter().find(|&&seq| seq > self.generation) {
                let skipped = next - self.generation;
                self.generation = next;
                self.offset = 0;
                return Ok(Chunk::Skipped(skipped));
            }
            let Some((file, len)) = live else {
                return Ok(Chunk::End);
            };
            return Ok(match self.read_from(file, len, buf)? {
                0 => Chunk::End,
                n => Chunk::Data(n),
            });
        }
    }

    fn read_from(&mut self, mut file: File, len: u64, buf: &mut [u8]) -> io::Result<usize> {
        if len < self.offset {
            self.offset = 0; // truncated: read what is there now
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let n = file.read(buf)?;
        self.offset += n as u64;
        Ok(n)
    }
}

/// `None` when the file does not exist. The length comes from the handle: on Windows a directory
/// entry lags behind a file that is being written.
fn open_file(path: &Path) -> io::Result<Option<(File, u64)>> {
    // On Windows a file that is being removed fails to open with "access denied" for a moment.
    match retry(|| File::open(path)) {
        Ok(file) => {
            let len = file.metadata()?.len();
            Ok(Some((file, len)))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// What a followed service is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    /// Running, or about to be started.
    Active,
    /// It will not write any more.
    Exited,
    /// Its pod was torn down: there is nothing left to read.
    Gone,
}

/// Why following ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    Exited,
    /// It exited, but something it started still writes to the log.
    StillWritten,
    Gone,
}

/// What a follower passes on.
pub(crate) enum Item<'a> {
    Data(&'a [u8]),
    Skipped(u64),
}

/// Passes on what is written to the log from `cursor` on, until the service is gone and its log is
/// read to the end. `state` is asked before each look at the log.
pub(crate) fn follow(
    mut cursor: Cursor,
    mut state: impl FnMut() -> Result<State>,
    mut sink: impl FnMut(Item) -> io::Result<()>,
) -> Result<End> {
    let mut buf = vec![0; CHUNK];
    let mut exited: Option<Instant> = None;
    let mut last_data = Instant::now();
    loop {
        if exited.is_none() {
            match state()? {
                State::Active => {}
                State::Exited => {
                    exited = Some(Instant::now());
                    last_data = Instant::now();
                }
                State::Gone => return Ok(End::Gone),
            }
        }
        match cursor.read(&mut buf)? {
            Chunk::Data(n) => {
                sink(Item::Data(&buf[..n]))?;
                last_data = Instant::now();
            }
            Chunk::Skipped(n) => {
                sink(Item::Skipped(n))?;
                continue;
            }
            Chunk::End => {
                if exited.is_some() && last_data.elapsed() >= QUIET {
                    return Ok(End::Exited);
                }
                thread::sleep(POLL);
            }
        }
        // A backlog is read to its end however long that takes; only output that keeps coming
        // after the exit is cut off.
        if exited.is_some_and(|at| at.elapsed() >= DRAIN) && cursor.caught_up()? {
            return Ok(End::StillWritten);
        }
    }
}

impl Cursor {
    /// Whether less than a chunk of the live file is left to read.
    fn caught_up(&self) -> io::Result<bool> {
        if segments(&self.log)?
            .iter()
            .any(|&seq| seq >= self.generation)
        {
            return Ok(false);
        }
        Ok(open_file(&self.log)?
            .is_none_or(|(_, len)| len.saturating_sub(self.offset) < CHUNK as u64))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;

    use super::*;

    struct Log {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    /// A log with the given segments (from 1 on) and live file.
    fn log(segs: &[&[u8]], live: Option<&[u8]>) -> Log {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w\u{e9}b.log");
        for (i, bytes) in segs.iter().enumerate() {
            fs::write(segment(&path, i as u64 + 1), bytes).unwrap();
        }
        if let Some(bytes) = live {
            fs::write(&path, bytes).unwrap();
        }
        Log { _dir: dir, path }
    }

    fn tail(log: &Log, lines: Lines) -> (Vec<u8>, bool) {
        let mut out: Vec<u8> = Vec::new();
        let more = Snapshot::open(&log.path)
            .unwrap()
            .unwrap()
            .tail(lines, |b| {
                out.extend(b);
                Ok(())
            })
            .unwrap();
        (out, more)
    }

    fn last(log: &Log, n: u64) -> String {
        String::from_utf8(tail(log, Lines::Last(n)).0).unwrap()
    }

    fn drain(cursor: &mut Cursor) -> (Vec<u8>, u64) {
        let mut buf = [0; 7]; // odd and small: chunks end anywhere
        let (mut out, mut skipped) = (Vec::new(), 0);
        loop {
            match cursor.read(&mut buf).unwrap() {
                Chunk::Data(n) => out.extend(&buf[..n]),
                Chunk::Skipped(n) => skipped += n,
                Chunk::End => return (out, skipped),
            }
        }
    }

    #[test]
    fn no_log_is_none() {
        assert!(Snapshot::open(&log(&[], None).path).unwrap().is_none());
        assert!(
            Snapshot::open(Path::new("no/such/dir/x.log"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn last_lines() {
        let l = log(&[], Some(b"a\nb\nc\n"));
        assert_eq!(last(&l, 0), "");
        assert_eq!(last(&l, 1), "c\n");
        assert_eq!(last(&l, 2), "b\nc\n");
        assert_eq!(last(&l, 3), "a\nb\nc\n");
        assert_eq!(last(&l, 99), "a\nb\nc\n");
        assert!(tail(&l, Lines::Last(2)).1);
        assert!(!tail(&l, Lines::Last(3)).1);
        assert!(!tail(&l, Lines::All).1);

        let unfinished = log(&[], Some(b"a\nb"));
        assert_eq!(last(&unfinished, 1), "b");
        assert_eq!(last(&unfinished, 2), "a\nb");
        assert_eq!(last(&log(&[], Some(b"a\n\n")), 1), "\n");
        assert_eq!(last(&log(&[], Some(b"")), 5), "");
    }

    #[test]
    fn last_lines_cross_segments() {
        let l = log(&[b"a\nb", b"b\nc\n"], Some(b"d\n"));
        assert_eq!(last(&l, 1), "d\n");
        assert_eq!(last(&l, 2), "c\nd\n");
        assert_eq!(last(&l, 3), "bb\nc\nd\n");
        assert_eq!(last(&l, 4), "a\nbb\nc\nd\n");
        // Just rotated: the last line ends in the segment, the live file is empty or missing.
        assert_eq!(last(&log(&[b"a\nb\n"], Some(b"")), 1), "b\n");
        assert_eq!(last(&log(&[b"a\nb\n"], None), 1), "b\n");
    }

    #[test]
    fn last_lines_cross_blocks() {
        let long = "x".repeat(CHUNK + 10);
        let text = format!("first\n{long}\nlast\n");
        let l = log(&[], Some(text.as_bytes()));
        assert_eq!(last(&l, 2), format!("{long}\nlast\n"));
        assert_eq!(last(&l, 3), text);
        // A newline on the very border of a block.
        let text = format!("first\n{}\n", "x".repeat(CHUNK - 1));
        assert_eq!(last(&log(&[], Some(text.as_bytes())), 1), text[6..]);
    }

    #[test]
    fn bytes_are_passed_as_they_are() {
        let bytes = b"\xff\xfe\x00\r\n\x80abc";
        assert_eq!(tail(&log(&[], Some(bytes)), Lines::All).0, bytes);
    }

    #[test]
    fn a_snapshot_is_not_affected_by_what_follows() {
        let l = log(&[b"1\n"], Some(b"2\n"));
        let mut snapshot = Snapshot::open(&l.path).unwrap().unwrap();
        let mut cursor = snapshot.end();
        fs::OpenOptions::new()
            .append(true)
            .open(&l.path)
            .and_then(|mut f| io::Write::write_all(&mut f, b"3\n"))
            .unwrap();
        fs::rename(&l.path, segment(&l.path, 2)).unwrap();
        fs::write(&l.path, "4\n").unwrap();
        let mut out: Vec<u8> = Vec::new();
        snapshot
            .tail(Lines::All, |b| {
                out.extend(b);
                Ok(())
            })
            .unwrap();
        assert_eq!(out, b"1\n2\n");
        assert_eq!(drain(&mut cursor), (b"3\n4\n".to_vec(), 0));
    }

    #[test]
    fn follows_rotations_without_loss_or_repetition() {
        let l = log(&[], None);
        let mut cursor = Cursor::start(&l.path);
        assert_eq!(drain(&mut cursor), (vec![], 0));
        fs::write(&l.path, "one, ").unwrap();
        assert_eq!(drain(&mut cursor).0, b"one, ");
        // Two rotations between two reads.
        fs::write(&l.path, "one, two, ").unwrap();
        fs::rename(&l.path, segment(&l.path, 1)).unwrap();
        fs::write(&l.path, "three, ").unwrap();
        fs::rename(&l.path, segment(&l.path, 2)).unwrap();
        assert_eq!(drain(&mut cursor), (b"two, three, ".to_vec(), 0));
        fs::write(&l.path, "four").unwrap();
        assert_eq!(drain(&mut cursor), (b"four".to_vec(), 0));
    }

    #[test]
    fn a_follower_that_is_too_slow_says_so() {
        let l = log(&[b"1", b"2", b"3"], Some(b"4"));
        let mut cursor = Cursor::start(&l.path);
        fs::remove_file(segment(&l.path, 1)).unwrap();
        fs::remove_file(segment(&l.path, 2)).unwrap();
        assert_eq!(drain(&mut cursor), (b"34".to_vec(), 2));
    }

    #[test]
    fn a_truncated_log_is_read_from_its_start() {
        let l = log(&[], Some(b"long enough"));
        let mut cursor = Cursor::start(&l.path);
        drain(&mut cursor);
        fs::write(&l.path, "new").unwrap();
        assert_eq!(drain(&mut cursor).0, b"new");
    }

    #[test]
    fn following_ends_after_the_last_output_of_an_exited_service() {
        let l = log(&[], Some(b"start\n"));
        let asked = Cell::new(0);
        let mut out: Vec<u8> = Vec::new();
        let end = follow(
            Cursor::start(&l.path),
            || {
                asked.set(asked.get() + 1);
                if asked.get() < 3 {
                    return Ok(State::Active);
                }
                // The writer is behind the process: this arrives after the exit was noticed.
                fs::write(&l.path, "start\nlast words").unwrap();
                Ok(State::Exited)
            },
            |item| {
                if let Item::Data(b) = item {
                    out.extend(b);
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(end, End::Exited);
        assert_eq!(out, b"start\nlast words");
    }

    #[test]
    fn a_process_left_behind_does_not_keep_the_follower() {
        let l = log(&[], Some(b""));
        let started = Instant::now();
        let end = follow(
            Cursor::start(&l.path),
            || {
                fs::write(&l.path, "0").unwrap();
                Ok(State::Exited)
            },
            |_| {
                // Every read finds something new.
                let mut f = fs::OpenOptions::new().append(true).open(&l.path)?;
                io::Write::write_all(&mut f, b"more")
            },
        )
        .unwrap();
        assert_eq!(end, End::StillWritten);
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn a_pod_that_is_gone_ends_following_at_once() {
        let l = log(&[], Some(b"belongs to the next pod"));
        let end = follow(
            Cursor::start(&l.path),
            || Ok(State::Gone),
            |_| panic!("nothing to pass on"),
        )
        .unwrap();
        assert_eq!(end, End::Gone);
    }
}
