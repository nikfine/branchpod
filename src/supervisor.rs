//! The supervisor: one process per pod that runs the pod's services and stops them.
//!
//! `bp up` starts it ([`Launcher::launch`]) as `bp supervise <endpoint>`, detached from the caller,
//! and waits for one line on its stdout saying that it listens (or why it cannot). From then on it
//! is driven over its control channel ([`crate::control`]), one JSON line per request: `start` a
//! service (fully rendered by `bp up`, so the supervisor needs no config), `stop` one, `status`,
//! `shutdown`. The supervisor never writes the ledger: `bp up` and `bp down` record what it
//! reports, under the pod lock, so the ledger keeps a single writer.
//!
//! A service's stdout and stderr go to its log file through an inherited handle, without passing
//! through the supervisor, so they keep flowing if the supervisor dies. Services are not restarted;
//! one that exits keeps its exit status until it is stopped.
//!
//! The protocol is internal (the same binary on both ends) and carries a version anyway, so that a
//! supervisor left running by an older `bp` is detected rather than misread.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::commands;
use crate::control::{self, Conn, Server};
use crate::proc::{self, ProcId};

/// How long a stopped service gets to exit before it is killed.
pub const GRACE: Duration = Duration::from_secs(10);

/// How long `bp down` waits for a supervisor to exit after `shutdown` answered, before killing it.
pub const EXIT_WAIT: Duration = Duration::from_secs(10);

const PROTOCOL: u32 = 1;

/// Prefix of the line a starting supervisor prints on stdout. Found anywhere in a line, so that a
/// test binary's own output around it does not matter.
const READY: &str = "BP-SUPERVISOR ";

/// Ledger data of the supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub endpoint: String,
    /// Set once it reported that it listens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    Start(Start),
    Stop { name: String },
    Status,
    Shutdown,
}

/// A service to start, fully rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    pub name: String,
    pub run: Vec<String>,
    /// Absolute.
    pub cwd: PathBuf,
    /// Added to the supervisor's environment (inherited from `bp up`).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// stdout and stderr go here, appended.
    pub log: PathBuf,
}

/// A service as `status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub name: String,
    pub process: ProcId,
    /// `None` while it runs.
    #[serde(default)]
    pub exit: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Message<T> {
    v: u32,
    #[serde(flatten)]
    body: T,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "lowercase")]
enum Reply {
    Ok {
        #[serde(default)]
        data: Value,
    },
    Error {
        message: String,
    },
}

/// What a starting supervisor prints once.
#[derive(Debug, Serialize, Deserialize)]
struct Ready {
    #[serde(default)]
    process: Option<ProcId>,
    #[serde(default)]
    error: Option<String>,
}

/// Builds the command that runs a supervisor for an endpoint: `bp supervise <endpoint>`, or the
/// test binary re-running itself in tests.
pub struct Launcher(Box<dyn Fn(&str) -> Command + Send + Sync>);

impl Launcher {
    pub fn new(build: impl Fn(&str) -> Command + Send + Sync + 'static) -> Launcher {
        Launcher(Box::new(build))
    }

    /// `bp supervise <endpoint>` with this very binary.
    pub fn current_exe() -> Result<Launcher> {
        let exe = std::env::current_exe().context("cannot locate the bp binary")?;
        Ok(Launcher::new(move |endpoint| {
            let mut cmd = Command::new(&exe);
            cmd.arg("supervise").arg(endpoint);
            cmd
        }))
    }

    /// Starts a supervisor listening on `endpoint`, with `cwd` as its working directory and its
    /// own diagnostics in `log`, and waits until it listens. It is not waited for afterwards: it
    /// outlives us.
    pub fn launch(&self, endpoint: &str, cwd: &Path, log: &Path) -> Result<ProcId> {
        let log = open_log(log)?;
        let mut cmd = (self.0)(endpoint);
        cmd.current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log);
        let mut child = proc::spawn_detached(&mut cmd).context("cannot start the supervisor")?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let mut lines = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            // No time limit: the supervisor either answers, or exits and the pipe closes.
            if lines.read_line(&mut line)? == 0 {
                let status = child.wait()?;
                bail!("the supervisor exited before it was ready ({status})");
            }
            let Some((_, text)) = line.split_once(READY) else {
                continue;
            };
            let ready: Ready = serde_json::from_str(text.trim())
                .with_context(|| format!("unexpected answer from the supervisor: {text:?}"))?;
            return match (ready.process, ready.error) {
                (Some(process), None) if process.pid == child.id() => Ok(process),
                (_, Some(error)) => Err(anyhow!("the supervisor cannot start: {error}")),
                _ => bail!("unexpected answer from the supervisor: {text:?}"),
            };
        }
    }
}

fn open_log(path: &Path) -> Result<fs::File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("cannot open log {}", path.display()))
}

/// Sends `request` to the supervisor at `endpoint` and returns the data of its answer. Fails with
/// [`control::Unreachable`] when nothing listens there.
pub fn request(endpoint: &str, request: &Request) -> Result<Value> {
    let conn = control::connect(endpoint)?;
    let message = Message {
        v: PROTOCOL,
        body: request,
    };
    conn.write_line(&serde_json::to_string(&message)?)?;
    let line = conn.read_line()?;
    let reply: Message<Reply> = serde_json::from_str(&line)
        .with_context(|| format!("unexpected answer from the supervisor: {line:?}"))?;
    if reply.v != PROTOCOL {
        bail!(
            "the supervisor speaks protocol v{}, this bp v{PROTOCOL}; it was started by \
             another version of bp",
            reply.v
        );
    }
    match reply.body {
        Reply::Ok { data } => Ok(data),
        Reply::Error { message } => Err(anyhow!(message)),
    }
}

/// Whether `error` means that nothing listens on the supervisor's endpoint.
pub fn is_unreachable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<control::Unreachable>().is_some()
}

/// A running service, as the supervisor holds it.
struct Service {
    child: Child,
    process: ProcId,
    exit: Option<ExitStatus>,
}

impl Service {
    fn exit(&mut self) -> Option<ExitStatus> {
        if self.exit.is_none() {
            self.exit = self.child.try_wait().ok().flatten();
        }
        self.exit
    }
}

/// The supervisor process: listens on `endpoint` and serves requests until `shutdown`.
pub fn serve(endpoint: &str) -> Result<()> {
    let mut server = match Server::bind(endpoint) {
        Ok(server) => server,
        Err(e) => {
            announce(&Ready {
                process: None,
                error: Some(format!("{e:#}")),
            });
            return Err(e);
        }
    };
    let me = proc::identify(std::process::id());
    announce(&Ready {
        process: me,
        error: me
            .is_none()
            .then(|| "cannot identify own process".to_owned()),
    });
    if me.is_none() {
        bail!("cannot identify own process");
    }

    let mut services: BTreeMap<String, Service> = BTreeMap::new();
    loop {
        let conn = match server.accept() {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("bp supervise: {e:#}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let (reply, quit) = match handle(&conn, &mut services) {
            Ok((data, quit)) => (Reply::Ok { data }, quit),
            Err(e) => (
                Reply::Error {
                    message: format!("{e:#}"),
                },
                false,
            ),
        };
        let message = Message {
            v: PROTOCOL,
            body: reply,
        };
        let text = serde_json::to_string(&message).expect("serializable");
        if let Err(e) = conn.write_line(&text) {
            eprintln!("bp supervise: {e:#}");
        }
        drop(conn);
        if quit {
            return Ok(());
        }
    }
}

fn announce(ready: &Ready) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "{READY}{}",
        serde_json::to_string(ready).expect("serializable")
    );
    let _ = out.flush();
}

/// Handles one request; the answer's data and whether to exit afterwards.
fn handle(conn: &Conn, services: &mut BTreeMap<String, Service>) -> Result<(Value, bool)> {
    let line = conn.read_line()?;
    let message: Message<Value> = serde_json::from_str(&line).context("malformed request")?;
    if message.v != PROTOCOL {
        bail!(
            "this supervisor speaks protocol v{PROTOCOL}, the request is v{}; it was started \
             by another version of bp",
            message.v
        );
    }
    let request: Request = serde_json::from_value(message.body).context("malformed request")?;
    match request {
        Request::Start(start) => {
            let process = start_service(&start, services)?;
            Ok((serde_json::to_value(process)?, false))
        }
        Request::Stop { name } => {
            if let Some(mut service) = services.remove(&name) {
                proc::stop_tree(&mut service.child, GRACE)
                    .with_context(|| format!("cannot stop service {name:?}"))?;
            }
            Ok((Value::Null, false))
        }
        Request::Status => {
            let list: Vec<ServiceStatus> = services
                .iter_mut()
                .map(|(name, s)| ServiceStatus {
                    name: name.clone(),
                    process: s.process,
                    exit: s.exit().map(|status| status.to_string()),
                })
                .collect();
            Ok((serde_json::to_value(list)?, false))
        }
        Request::Shutdown => {
            let mut errors = Vec::new();
            for (name, mut service) in std::mem::take(services) {
                if let Err(e) = proc::stop_tree(&mut service.child, GRACE) {
                    errors.push(format!("{name}: {e}"));
                }
            }
            if !errors.is_empty() {
                // Stay up: the services left can still be stopped through us.
                bail!("cannot stop services: {}", errors.join("; "));
            }
            Ok((Value::Null, true))
        }
    }
}

fn start_service(start: &Start, services: &mut BTreeMap<String, Service>) -> Result<ProcId> {
    if let Some(service) = services.get_mut(&start.name)
        && service.exit().is_none()
    {
        bail!("service {:?} is already running", start.name);
    }
    let (program, args) = start.run.split_first().context("empty argument array")?;
    if !start.cwd.is_dir() {
        bail!(
            "cannot run {program}: directory {} does not exist",
            start.cwd.display()
        );
    }
    let log = open_log(&start.log)?;
    let mut cmd = Command::new(commands::resolve_program(program, &start.cwd));
    cmd.args(args)
        .current_dir(&start.cwd)
        .envs(&start.env)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    proc::new_tree(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| commands::spawn_error(program, e))?;
    let Some(process) = proc::identify(child.id()) else {
        // Exited at once (a zombie has no identity): a start that failed.
        let status = child.wait()?;
        bail!(
            "{program} exited at once with {status}; see {}",
            start.log.display()
        );
    };
    services.insert(
        start.name.clone(),
        Service {
            child,
            process,
            exit: None,
        },
    );
    Ok(process)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Name of the env var that makes [`supervisor_process`] run.
    const ENDPOINT_ENV: &str = "BP_TEST_SUPERVISOR_ENDPOINT";

    /// Starts supervisors as this test binary re-running [`supervisor_process`].
    pub fn launcher() -> Launcher {
        let exe = std::env::current_exe().unwrap();
        Launcher::new(move |endpoint| {
            let mut cmd = Command::new(&exe);
            cmd.args([
                "--exact",
                "supervisor::tests::supervisor_process",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ENDPOINT_ENV, endpoint);
            cmd
        })
    }

    /// Not a test by itself: the supervisor process for tests.
    #[test]
    #[ignore]
    fn supervisor_process() {
        if let Some(endpoint) = std::env::var_os(ENDPOINT_ENV) {
            serve(endpoint.to_str().unwrap()).unwrap();
        }
    }

    /// Prints a line, then sleeps for a long time, using what every CI runner has.
    pub fn long_running(marker: &str) -> Vec<String> {
        if cfg!(windows) {
            // `ping` waits about one second between echoes.
            vec![
                "cmd".into(),
                "/d".into(),
                "/c".into(),
                format!("echo {marker}& ping -n 300 127.0.0.1 > nul"),
            ]
        } else {
            vec![
                "sh".into(),
                "-c".into(),
                format!("echo {marker}; exec sleep 300"),
            ]
        }
    }

    struct Running {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
        endpoint: String,
        process: ProcId,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = request(&self.endpoint, &Request::Shutdown);
            let _ = proc::kill(self.process);
        }
    }

    fn running() -> Running {
        let tmp = tempfile::tempdir().unwrap();
        let dir = dunce::canonicalize(tmp.path()).unwrap();
        let endpoint = control::new_endpoint("test").unwrap();
        let process = launcher()
            .launch(&endpoint, &dir, &dir.join("supervisor.log"))
            .unwrap();
        Running {
            _tmp: tmp,
            dir,
            endpoint,
            process,
        }
    }

    fn start(r: &Running, name: &str, run: Vec<String>) -> Result<ProcId> {
        let start = Start {
            name: name.into(),
            run,
            cwd: r.dir.clone(),
            // ASCII: `cmd` echoes in the OEM code page.
            env: BTreeMap::from([("BP_TEST_VALUE".into(), "v-test".into())]),
            log: r.dir.join("logs").join(format!("{name}.log")),
        };
        let data = request(&r.endpoint, &Request::Start(start))?;
        Ok(serde_json::from_value(data)?)
    }

    /// Lossy: `cmd` writes in the OEM code page.
    fn read_log(path: &Path) -> String {
        fs::read(path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    fn wait_for_log(path: &Path, text: &str) {
        for _ in 0..400 {
            if read_log(path).contains(text) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "{} never contained {text:?}: {:?}",
            path.display(),
            read_log(path)
        );
    }

    #[test]
    fn starts_reports_and_stops_services() {
        let r = running();
        assert!(proc::is_running(r.process));

        let web = start(&r, "web", long_running("web-started")).unwrap();
        wait_for_log(&r.dir.join("logs/web.log"), "web-started");
        assert!(proc::is_running(web));
        let err = format!("{:#}", start(&r, "web", long_running("x")).unwrap_err());
        assert!(err.contains("already running"), "{err}");

        let status: Vec<ServiceStatus> =
            serde_json::from_value(request(&r.endpoint, &Request::Status).unwrap()).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!((status[0].process, status[0].exit.as_deref()), (web, None));

        request(&r.endpoint, &Request::Stop { name: "web".into() }).unwrap();
        assert!(proc::wait_gone(web, Duration::from_secs(5)));
        // Stopping what is not running is not an error.
        request(&r.endpoint, &Request::Stop { name: "web".into() }).unwrap();

        request(&r.endpoint, &Request::Shutdown).unwrap();
        assert!(proc::wait_gone(r.process, EXIT_WAIT));
        let err = request(&r.endpoint, &Request::Status).unwrap_err();
        assert!(is_unreachable(&err), "{err:#}");
    }

    #[test]
    fn services_get_their_environment_and_directory() {
        let r = running();
        let run = if cfg!(windows) {
            vec![
                "cmd".into(),
                "/d".into(),
                "/c".into(),
                "echo %BP_TEST_VALUE% & cd & ping -n 300 127.0.0.1 > nul".into(),
            ]
        } else {
            vec![
                "sh".into(),
                "-c".into(),
                "echo $BP_TEST_VALUE; pwd; exec sleep 300".into(),
            ]
        };
        start(&r, "env", run).unwrap();
        let log = r.dir.join("logs/env.log");
        wait_for_log(&log, "v-test");
        wait_for_log(&log, &r.dir.file_name().unwrap().to_string_lossy());
    }

    #[test]
    fn a_failed_start_is_an_error() {
        let r = running();
        let err = format!(
            "{:#}",
            start(&r, "none", vec!["bp-no-such-program-\u{e9}".into()]).unwrap_err()
        );
        assert!(err.contains("program not found"), "{err}");
        let quick = if cfg!(windows) {
            vec!["cmd".into(), "/d".into(), "/c".into(), "exit 3".into()]
        } else {
            vec!["sh".into(), "-c".into(), "exit 3".into()]
        };
        // It may be gone before the supervisor looks, or be reported as started and exit.
        if let Ok(process) = start(&r, "quick", quick) {
            assert!(proc::wait_gone(process, Duration::from_secs(5)));
        }
    }

    #[cfg(unix)]
    #[test]
    fn stopping_a_service_stops_its_children() {
        let r = running();
        // The shell waits for a background child: both are in the service's process group.
        let run = vec![
            "sh".into(),
            "-c".into(),
            "sleep 300 & echo child $!; wait".into(),
        ];
        start(&r, "tree", run).unwrap();
        let log = r.dir.join("logs/tree.log");
        wait_for_log(&log, "child ");
        let text = read_log(&log);
        let pid: u32 = text.split_once("child ").unwrap().1.trim().parse().unwrap();
        let child = proc::identify(pid).expect("the child runs");
        request(
            &r.endpoint,
            &Request::Stop {
                name: "tree".into(),
            },
        )
        .unwrap();
        assert!(proc::wait_gone(child, Duration::from_secs(5)));
    }

    #[test]
    fn a_dead_supervisor_is_unreachable() {
        let r = running();
        proc::kill(r.process).unwrap();
        assert!(proc::wait_gone(r.process, Duration::from_secs(5)));
        let err = request(&r.endpoint, &Request::Status).unwrap_err();
        // On Unix the socket file of a killed supervisor stays: connecting is refused.
        assert!(is_unreachable(&err), "{err:#}");
    }

    #[test]
    fn a_second_supervisor_on_the_same_endpoint_fails_to_start() {
        let r = running();
        let err = launcher()
            .launch(&r.endpoint, &r.dir, &r.dir.join("second.log"))
            .unwrap_err();
        assert!(format!("{err:#}").contains("cannot start"), "{err:#}");
    }
}
