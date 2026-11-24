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
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::commands;
use crate::control::{self, Conn, Server};
use crate::proc::{self, ProcId, Tether, Tree};

/// How long a stopped service gets to exit before it is killed.
pub(crate) const GRACE: Duration = Duration::from_secs(10);

/// How long `bp down` waits for a supervisor to exit after `shutdown` answered, before killing it.
pub(crate) const EXIT_WAIT: Duration = Duration::from_secs(10);

/// How long the supervisor waits for a connected client to send its request. Requests are served
/// one at a time, so a silent client must not keep everyone else waiting.
const REQUEST_WAIT: Duration = Duration::from_secs(10);

const PROTOCOL: u32 = 1;

/// Prefix of the line a starting supervisor prints on stdout. Found anywhere in a line, so that a
/// test binary's own output around it does not matter.
const READY: &str = "BP-SUPERVISOR ";

/// Ledger data of the supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) endpoint: String,
    /// Set once it reported that it listens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) process: Option<ProcId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub(crate) enum Request {
    Start(Start),
    Stop { name: String },
    Status,
    Shutdown,
}

/// A service to start, fully rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Start {
    pub(crate) name: String,
    pub(crate) run: Vec<String>,
    /// Absolute.
    pub(crate) cwd: PathBuf,
    /// Added to the supervisor's environment (inherited from `bp up`).
    #[serde(default)]
    pub(crate) env: BTreeMap<String, String>,
    /// stdout and stderr go here, appended.
    pub(crate) log: PathBuf,
}

/// A service as `status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ServiceStatus {
    pub(crate) name: String,
    pub(crate) process: ProcId,
    /// `None` while it runs.
    #[serde(default)]
    pub(crate) exit: Option<String>,
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
    #[serde(default)]
    warning: Option<String>,
}

/// Builds the command that runs a supervisor for an endpoint: `bp supervise <endpoint>`, or the
/// test binary re-running itself in tests.
pub(crate) struct Launcher {
    build: Box<dyn Fn(&str) -> Command + Send + Sync>,
    check: TetherCheck,
}

/// Whether a supervisor that would not outlive the caller is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TetherCheck {
    On,
    /// Tests run under whatever job the test runner has, and their supervisors need not outlive
    /// them.
    #[cfg(test)]
    Off,
}

impl TetherCheck {
    /// Fails if the tether kills; otherwise returns what to warn about.
    fn judge(self, tether: impl FnOnce() -> io::Result<Tether>) -> Result<Option<String>> {
        match self {
            #[cfg(test)]
            TetherCheck::Off => Ok(None),
            TetherCheck::On => match tether().context("cannot tell what may end it")? {
                Tether::Free => Ok(None),
                Tether::Unclear(why) => Ok(Some(why.to_owned())),
                Tether::Kills(why) => Err(anyhow!(why)),
            },
        }
    }
}

/// A supervisor that listens.
#[derive(Debug)]
pub(crate) struct Launched {
    pub(crate) process: ProcId,
    /// For the user: it may not outlive the caller.
    pub(crate) warning: Option<String>,
}

impl Launcher {
    /// Starts supervisors that do not check whether they outlive the caller; they must run
    /// [`serve`] with [`TetherCheck::Off`] too.
    #[cfg(test)]
    pub(crate) fn unchecked(build: impl Fn(&str) -> Command + Send + Sync + 'static) -> Launcher {
        Launcher {
            build: Box::new(build),
            check: TetherCheck::Off,
        }
    }

    /// Fails if it is already clear that a supervisor would be killed together with the caller.
    /// For before anything is created; the supervisor checks again, and more precisely.
    pub(crate) fn preflight(&self) -> Result<()> {
        self.check.judge(proc::detached_tether).map(drop)
    }

    /// `bp supervise <endpoint>` with this very binary.
    pub(crate) fn current_exe() -> Result<Launcher> {
        let exe = std::env::current_exe().context("cannot locate the bp binary")?;
        Ok(Launcher {
            build: Box::new(move |endpoint| {
                let mut cmd = Command::new(&exe);
                cmd.arg("supervise").arg(endpoint);
                cmd
            }),
            check: TetherCheck::On,
        })
    }

    /// Starts a supervisor listening on `endpoint`, with `cwd` as its working directory and its
    /// own diagnostics in `log`, and waits until it listens. It is not waited for afterwards: it
    /// outlives us.
    pub(crate) fn launch(&self, endpoint: &str, cwd: &Path, log: &Path) -> Result<Launched> {
        let log = open_log(log)?;
        let mut cmd = (self.build)(endpoint);
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
                (Some(process), None) if process.pid == child.id() => Ok(Launched {
                    process,
                    warning: ready.warning,
                }),
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
pub(crate) fn request(endpoint: &str, request: &Request) -> Result<Value> {
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
        Reply::Error { message } => Err(Refused(message).into()),
    }
}

/// A running supervisor answered that it could not do what it was asked.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// Whether `error` is an answer of a running supervisor, not a failure to get one.
pub(crate) fn is_answer(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Refused>().is_some()
}

/// Whether `error` means that nothing listens on the supervisor's endpoint.
pub(crate) fn is_unreachable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<control::Unreachable>().is_some()
}

/// A running service, as the supervisor holds it.
struct Service {
    tree: Tree,
    process: ProcId,
    exit: Option<ExitStatus>,
}

impl Service {
    fn exit(&mut self) -> Option<ExitStatus> {
        if self.exit.is_none() {
            self.exit = self.tree.child().try_wait().ok().flatten();
        }
        self.exit
    }

    /// Stops its process tree, also what is left of it once the service process has exited.
    fn stop(&mut self) -> io::Result<()> {
        self.tree.stop(self.process, GRACE)
    }
}

/// The supervisor process: listens on `endpoint` and serves requests until `shutdown`.
pub(crate) fn serve(endpoint: &str, check: TetherCheck) -> Result<()> {
    let mut server = match prepare(endpoint, check) {
        Ok((server, process, warning)) => {
            announce(&Ready {
                process: Some(process),
                error: None,
                warning,
            });
            server
        }
        Err(e) => {
            announce(&Ready {
                process: None,
                error: Some(format!("{e:#}")),
                warning: None,
            });
            return Err(e);
        }
    };

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

/// Everything that can keep a supervisor from starting; also what to warn about.
fn prepare(endpoint: &str, check: TetherCheck) -> Result<(Server, ProcId, Option<String>)> {
    // Before `contain`: what we are in now is what the caller left us in.
    let warning = check.judge(proc::own_tether)?;
    proc::contain().context("cannot tie services to the supervisor")?;
    let server = Server::bind(endpoint)?;
    let me = proc::identify(std::process::id()).context("cannot identify own process")?;
    Ok((server, me, warning))
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
    let line = conn.read_line_within(REQUEST_WAIT)?;
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
            if let Some(service) = services.get_mut(&name) {
                service
                    .stop()
                    .with_context(|| format!("cannot stop service {name:?}"))?;
                services.remove(&name);
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
            // Only what was stopped is forgotten: the rest can still be stopped through us.
            services.retain(|name, service| match service.stop() {
                Ok(()) => false,
                Err(e) => {
                    errors.push(format!("{name}: {e}"));
                    true
                }
            });
            if !errors.is_empty() {
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
    let mut tree = Tree::spawn(&mut cmd).map_err(|e| commands::spawn_error(program, e))?;
    let Some(process) = proc::identify(tree.child().id()) else {
        // Exited at once (a zombie has no identity): a start that failed.
        let status = tree.child().wait()?;
        bail!(
            "{program} exited at once with {status}; see {}",
            start.log.display()
        );
    };
    services.insert(
        start.name.clone(),
        Service {
            tree,
            process,
            exit: None,
        },
    );
    Ok(process)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use crate::test_support::{TREE_ENV, tree};
    use crate::test_support::{fail, launcher, long_running};

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
            .unwrap()
            .process;
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
        // It may be gone before the supervisor looks, or be reported as started and exit.
        if let Ok(process) = start(&r, "quick", fail(3, "boom")) {
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

    /// Starts a service that has a child; both.
    #[cfg(windows)]
    fn start_tree(r: &Running, mode: &str) -> (ProcId, ProcId) {
        let log = r.dir.join("logs/tree.log");
        let start = Start {
            name: "tree".into(),
            run: tree(),
            cwd: r.dir.clone(),
            env: BTreeMap::from([(TREE_ENV.into(), mode.into())]),
            log: log.clone(),
        };
        let data = request(&r.endpoint, &Request::Start(start)).unwrap();
        wait_for_log(&log, " started");
        let text = read_log(&log);
        let (_, text) = text.split_once("child ").unwrap();
        let pid = text.split_once(' ').unwrap().0.parse().unwrap();
        (
            serde_json::from_value(data).unwrap(),
            proc::identify(pid).expect("the child runs"),
        )
    }

    #[cfg(windows)]
    fn stop_tree(r: &Running) {
        let stop = Request::Stop {
            name: "tree".into(),
        };
        request(&r.endpoint, &stop).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn stopping_a_service_stops_its_children() {
        let r = running();
        let (leader, child) = start_tree(&r, "wait");
        stop_tree(&r);
        // The answer came after all of it was gone.
        assert!(proc::wait_gone(leader, Duration::ZERO));
        assert!(proc::wait_gone(child, Duration::ZERO));
        assert!(proc::is_running(r.process));
    }

    #[cfg(windows)]
    #[test]
    fn stopping_a_service_that_exited_stops_its_children() {
        let r = running();
        let (leader, child) = start_tree(&r, "exit");
        assert!(proc::wait_gone(leader, Duration::from_secs(10)));
        assert!(proc::is_running(child));
        stop_tree(&r);
        assert!(proc::wait_gone(child, Duration::ZERO));
        assert!(proc::is_running(r.process));
    }

    #[cfg(windows)]
    #[test]
    fn a_killed_supervisor_takes_its_services_along() {
        let r = running();
        let (leader, child) = start_tree(&r, "wait");
        proc::kill(r.process).unwrap();
        assert!(proc::wait_gone(leader, Duration::from_secs(10)));
        assert!(proc::wait_gone(child, Duration::from_secs(10)));
    }

    #[test]
    fn a_tether_that_kills_is_refused_and_an_unclear_one_reported() {
        let judge = |tether| TetherCheck::On.judge(|| Ok(tether));
        assert_eq!(judge(Tether::Free).unwrap(), None);
        assert_eq!(judge(Tether::Unclear("hm")).unwrap().as_deref(), Some("hm"));
        let err = judge(Tether::Kills("why")).unwrap_err();
        assert_eq!(format!("{err:#}"), "why");
        // Not knowing is not a reason to start.
        assert!(
            TetherCheck::On
                .judge(|| Err(io::Error::other("x")))
                .is_err()
        );
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
