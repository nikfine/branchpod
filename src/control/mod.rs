//! The local control channel of a supervisor: a Unix domain socket, a named pipe on Windows.
//!
//! One connection per request: the client writes one line, the server answers with one line and
//! closes the connection. The server handles connections one at a time.
//!
//! - Unix: the socket lives in a directory only the user can enter (`$XDG_RUNTIME_DIR/branchpod`,
//!   otherwise `<temp>/branchpod-<uid>`), not in the pod directory: socket paths are limited to
//!   about 104 bytes, which a data directory such as `~/Library/Application Support/...` exceeds.
//! - Windows: `\\.\pipe\branchpod-<pod>-<nonce>`, created with `FILE_FLAG_FIRST_PIPE_INSTANCE`
//!   (nobody else can have created that name first) and rejecting remote clients. A spare pipe
//!   instance is always open while the server lives, so a client never mistakes the moment between
//!   two connections for a server that is gone.
//!
//! Endpoints carry a random part, so a stale endpoint of an earlier supervisor is never reused.

use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;

/// Longest line accepted, so that a broken peer cannot make us read without end.
const MAX_LINE: u64 = 1 << 20;

/// Nobody listens on the endpoint: the supervisor is gone (or its endpoint was removed).
#[derive(Debug)]
pub struct Unreachable(pub String);

impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "nothing listens on {}", self.0)
    }
}

impl std::error::Error for Unreachable {}

/// A fresh endpoint for a supervisor of pod `pod`.
pub fn new_endpoint(pod: &str) -> Result<String> {
    sys::new_endpoint(pod, &nonce(pod))
}

/// 16 hex digits that differ between supervisors of the same pod.
fn nonce(pod: &str) -> String {
    // The clock alone may not advance between two calls (100 ns steps on Windows).
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let digest = Sha256::digest(format!("{pod}\0{}\0{nanos}\0{count}", std::process::id()));
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Server(sys::Server);

impl Server {
    pub fn bind(endpoint: &str) -> Result<Server> {
        sys::Server::bind(endpoint)
            .map(Server)
            .with_context(|| format!("cannot listen on {endpoint}"))
    }

    /// Waits for the next client.
    pub fn accept(&mut self) -> Result<Conn> {
        self.0
            .accept()
            .map(Conn)
            .context("cannot accept a connection")
    }
}

/// One connection, for one request and its answer.
pub struct Conn(sys::Stream);

impl Conn {
    /// Like [`Conn::read_line`], but gives up when nothing arrives for `limit`: a server serving
    /// one connection at a time must not hang on a client that connects and stays silent.
    pub fn read_line_within(&self, limit: Duration) -> Result<String> {
        let _deadline = self.0.deadline(limit)?;
        self.read_line()
    }

    pub fn read_line(&self) -> Result<String> {
        let mut line = String::new();
        BufReader::new((&self.0).take(MAX_LINE))
            .read_line(&mut line)
            .context("cannot read from the control channel")?;
        if !line.ends_with('\n') {
            bail!("the control channel closed before a complete line");
        }
        line.pop();
        Ok(line)
    }

    pub fn write_line(&self, line: &str) -> Result<()> {
        let mut out = line.as_bytes().to_vec();
        out.push(b'\n');
        (&self.0)
            .write_all(&out)
            .and_then(|()| (&self.0).flush())
            .context("cannot write to the control channel")
    }
}

/// Connects to `endpoint`. Fails with [`Unreachable`] when nothing listens there.
pub fn connect(endpoint: &str) -> Result<Conn> {
    sys::connect(endpoint).map(Conn)
}

/// Removes what a server that died without cleaning up left at `endpoint` (a socket file on
/// Unix; a pipe disappears with its last handle). Call only once the server is known to be gone.
pub fn remove_stale(endpoint: &str) -> Result<()> {
    sys::remove_stale(endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_request_and_answer_per_connection() {
        let endpoint = new_endpoint("test-pod").unwrap();
        let mut server = Server::bind(&endpoint).unwrap();
        let client = std::thread::spawn({
            let endpoint = endpoint.clone();
            move || {
                let mut answers = Vec::new();
                for word in ["ping", "pong", "stop"] {
                    let conn = connect(&endpoint).unwrap();
                    conn.write_line(word).unwrap();
                    answers.push(conn.read_line().unwrap());
                }
                answers
            }
        });
        loop {
            let conn = server.accept().unwrap();
            let line = conn.read_line().unwrap();
            conn.write_line(&format!("got {line}")).unwrap();
            if line == "stop" {
                break;
            }
        }
        assert_eq!(client.join().unwrap(), ["got ping", "got pong", "got stop"]);
        drop(server);
        let err = connect(&endpoint).err().expect("nothing listens any more");
        assert!(err.downcast_ref::<Unreachable>().is_some(), "{err:#}");
    }

    /// Serves until a request arrives; connections without one are skipped.
    fn serve_one(server: &mut Server) -> String {
        loop {
            let conn = server.accept().unwrap();
            if let Ok(line) = conn.read_line_within(Duration::from_millis(500)) {
                conn.write_line(&format!("got {line}")).unwrap();
                return line;
            }
        }
    }

    #[test]
    fn a_client_gone_before_accept_does_not_break_the_server() {
        let endpoint = new_endpoint("test-pod").unwrap();
        let mut server = Server::bind(&endpoint).unwrap();
        // Connects to the waiting instance and leaves before the server accepts it.
        drop(connect(&endpoint).unwrap());
        let client = std::thread::spawn({
            let endpoint = endpoint.clone();
            move || {
                let conn = connect(&endpoint).unwrap();
                conn.write_line("hello").unwrap();
                conn.read_line().unwrap()
            }
        });
        assert_eq!(serve_one(&mut server), "hello");
        assert_eq!(client.join().unwrap(), "got hello");
    }

    #[test]
    fn a_silent_client_is_given_up_on() {
        let endpoint = new_endpoint("test-pod").unwrap();
        let mut server = Server::bind(&endpoint).unwrap();
        let silent = connect(&endpoint).unwrap();
        let conn = server.accept().unwrap();
        let started = std::time::Instant::now();
        assert!(conn.read_line_within(Duration::from_millis(300)).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(conn);
        drop(silent);
        let client = std::thread::spawn({
            let endpoint = endpoint.clone();
            move || {
                let conn = connect(&endpoint).unwrap();
                conn.write_line("next").unwrap();
                conn.read_line().unwrap()
            }
        });
        assert_eq!(serve_one(&mut server), "next");
        assert_eq!(client.join().unwrap(), "got next");
    }

    #[test]
    fn a_second_server_cannot_take_the_endpoint() {
        let endpoint = new_endpoint("test-pod").unwrap();
        let _server = Server::bind(&endpoint).unwrap();
        assert!(Server::bind(&endpoint).is_err());
    }

    #[test]
    fn endpoints_differ() {
        assert_ne!(new_endpoint("p").unwrap(), new_endpoint("p").unwrap());
    }
}
