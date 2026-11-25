//! The supervisor: one process per pod that runs the pod's services and stops them.
//!
//! `bp up` starts it ([`Launcher::launch`]) as `bp supervise <endpoint>`, detached from the caller,
//! and waits for one line on its stdout saying that it listens (or why it cannot). From then on it
//! is driven over its control channel ([`crate::control`]), one JSON line per request: `start` a
//! service (fully rendered by `bp up`, so the supervisor needs no config), `stop` one, `status`,
//! `shutdown`. The supervisor never writes the ledger: `bp up` and `bp down` record what it
//! reports, under the pod lock, so the ledger keeps a single writer.
//!
//! A service's stdout and stderr are one pipe, which the supervisor reads into the service's log
//! file ([`crate::logs::pump`]); should the supervisor die, the service fails to write. Services are
//! not restarted; one that exits keeps its exit status, and when that was, until it is stopped.
//!
//! The protocol is internal (the same binary on both ends) and carries a version anyway, so that a
//! supervisor left running by an older `bp` is detected rather than misread.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::commands;
use crate::control::{self, Conn, Server};
use crate::ledger::now_ms;
use crate::logs::pump::Pump;
use crate::proc::{self, LeaderExit, ProcId, Program, Tether, Tree};

/// How long a stopped service gets to exit before it is killed.
pub(crate) const GRACE: Duration = Duration::from_secs(10);

/// How long `bp down` waits for a supervisor to exit after `shutdown` answered, before killing it.
pub(crate) const EXIT_WAIT: Duration = Duration::from_secs(10);

/// How long one `stop` or `shutdown`, or a start that failed, waits for the last output of the
/// services to reach their logs. A process that left a service's tree can hold the pipe open for
/// as long as it lives.
const DRAIN: Duration = Duration::from_millis(500);

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
    /// stdout and stderr end up here, appended.
    pub(crate) log: PathBuf,
}

/// A service as `status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ServiceStatus {
    pub(crate) name: String,
    pub(crate) process: ProcId,
    /// `None` while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ended: Option<Ended>,
    /// Set instead of `ended` by a supervisor of an older version: text of the platform.
    #[serde(default, skip_serializing)]
    pub(crate) exit: Option<String>,
}

/// How and when a service exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Ended {
    pub(crate) exit: Exit,
    /// Milliseconds since the Unix epoch.
    pub(crate) at_ms: u64,
}

/// How a process exited: `{"code": 1}` or `{"signal": 9}`. Part of the public `--json` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Exit {
    /// On Windows the code as a signed number: an NTSTATUS such as 0xc000013a is negative.
    Code(i32),
    /// Unix only.
    Signal(i32),
}

impl From<ExitStatus> for Exit {
    fn from(status: ExitStatus) -> Exit {
        #[cfg(unix)]
        if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
            return Exit::Signal(signal);
        }
        // A process that has exited without a signal has a code.
        Exit::Code(status.code().unwrap_or_default())
    }
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Exit::Code(code) if *code < 0 => write!(f, "code {code}, {code:#010x}"),
            Exit::Code(code) => write!(f, "code {code}"),
            Exit::Signal(signal) => write!(f, "signal {signal}"),
        }
    }
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

/// Builds the program that runs a supervisor for an endpoint: `bp supervise <endpoint>`, or the
/// test binary re-running itself in tests.
pub(crate) struct Launcher {
    build: Box<dyn Fn(&str) -> Program + Send + Sync>,
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
    pub(crate) fn unchecked(build: impl Fn(&str) -> Program + Send + Sync + 'static) -> Launcher {
        Launcher {
            build: Box::new(build),
            check: TetherCheck::Off,
        }
    }

    /// What it starts for `endpoint`.
    #[cfg(all(test, unix))]
    fn program(&self, endpoint: &str) -> Program {
        (self.build)(endpoint)
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
            build: Box::new(move |endpoint| Program {
                exe: exe.clone(),
                args: vec!["supervise".into(), endpoint.into()],
            }),
            check: TetherCheck::On,
        })
    }

    /// Starts a supervisor listening on `endpoint`, with `cwd` as its working directory and its
    /// own diagnostics in `log`, and waits until it listens. It is not waited for afterwards: it
    /// outlives us.
    pub(crate) fn launch(&self, endpoint: &str, cwd: &Path, log: &Path) -> Result<Launched> {
        let log = open_log(log)?;
        let program = (self.build)(endpoint);
        let (mut child, stdout) =
            proc::spawn_detached(&program, cwd, log).context("cannot start the supervisor")?;
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
                (Some(process), None) if process.pid == child.pid() => Ok(Launched {
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

/// Asks the supervisor at `endpoint` for its services and gives up with [`NoAnswer`] after `limit`.
///
/// The whole request runs on a thread of its own that is left behind then: a supervisor that hangs
/// can block connecting (a full backlog, no free pipe instance) and writing as well as reading, and
/// not all of these can be given a time limit. For a process that is about to end.
pub(crate) fn services_within(endpoint: &str, limit: Duration) -> Result<Vec<ServiceStatus>> {
    let (sender, receiver) = sync_channel(1);
    let endpoint = endpoint.to_owned();
    thread::Builder::new()
        .name("status request".into())
        .spawn(move || {
            let _ = sender.send(request(&endpoint, &Request::Status));
        })
        .context("cannot start a thread")?;
    match receiver.recv_timeout(limit) {
        Ok(answer) => {
            serde_json::from_value(answer?).context("unexpected answer from the supervisor")
        }
        Err(RecvTimeoutError::Timeout) => Err(NoAnswer(limit).into()),
        Err(RecvTimeoutError::Disconnected) => bail!("the request was not completed"),
    }
}

/// The supervisor did not answer in time.
#[derive(Debug)]
pub(crate) struct NoAnswer(Duration);

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no answer within {:?}", self.0)
    }
}

impl std::error::Error for NoAnswer {}

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
    ended: Option<Ended>,
    /// When it exited, in milliseconds since the Unix epoch: set as soon as that happens.
    ended_at: Arc<OnceLock<u64>>,
    pump: Pump,
}

impl Service {
    fn ended(&mut self) -> Option<Ended> {
        if self.ended.is_none()
            && let Ok(Some(status)) = self.tree.child().try_wait()
        {
            self.ended = Some(Ended {
                exit: status.into(),
                // Not noted yet if it exited just now.
                at_ms: *self.ended_at.get_or_init(now_ms),
            });
        }
        self.ended
    }

    /// Stops its process tree, also what is left of it once the service process has exited.
    fn stop(&mut self) -> io::Result<()> {
        self.tree.stop(self.process, GRACE)
    }
}

/// Starts noting when `exit` happens. Should that fail, the time is when somebody asks.
fn watch(name: &str, exit: io::Result<LeaderExit>) -> Arc<OnceLock<u64>> {
    let at = Arc::new(OnceLock::new());
    let noted = Arc::clone(&at);
    let watching = exit.and_then(|exit| {
        thread::Builder::new()
            .name("service exit".into())
            .spawn(move || {
                if exit.wait().is_ok() {
                    let _ = noted.set(now_ms());
                }
            })
    });
    if let Err(e) = watching {
        eprintln!("bp supervise: cannot wait for service {name:?} to exit: {e}");
    }
    at
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
    proc::collect_exits().context("cannot collect the exit status of services")?;
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
                service.pump.wait(Instant::now() + DRAIN);
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
                    ended: s.ended(),
                    exit: None,
                })
                .collect();
            Ok((serde_json::to_value(list)?, false))
        }
        Request::Shutdown => {
            let mut errors = Vec::new();
            let mut drain = DRAIN; // for all of them together
            // Only what was stopped is forgotten: the rest can still be stopped through us.
            services.retain(|name, service| match service.stop() {
                Ok(()) => {
                    let started = Instant::now();
                    service.pump.wait(started + drain);
                    drain = drain.saturating_sub(started.elapsed());
                    false
                }
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
        && service.ended().is_none()
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
    let (pump, output) = Pump::start(&start.log)
        .with_context(|| format!("cannot open log {}", start.log.display()))?;
    let mut cmd = Command::new(commands::resolve_program(program, &start.cwd, &start.env));
    cmd.args(args)
        .current_dir(&start.cwd)
        .envs(&start.env)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output);
    let mut tree = Tree::spawn(&mut cmd).map_err(|e| commands::spawn_error(program, args, e))?;
    // It holds our copies of the pipe's writing end: the pump must see the service close the last.
    drop(cmd);
    let Some(process) = proc::identify(tree.child().id()) else {
        // Exited at once (a zombie has no identity): a start that failed.
        let status = tree.child().wait()?;
        pump.wait(Instant::now() + DRAIN);
        bail!(
            "{program} exited at once with {status}; see {}",
            start.log.display()
        );
    };
    let ended_at = watch(&start.name, tree.leader_exit());
    services.insert(
        start.name.clone(),
        Service {
            tree,
            process,
            ended: None,
            ended_at,
            pump,
        },
    );
    Ok(process)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use crate::test_support::{TREE_ENV, tree};
    use crate::test_support::{
        fail, launcher, long_running, read_log, short_running, wait_for_log,
    };

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

    #[test]
    fn starts_reports_and_stops_services() {
        let r = running();
        assert!(proc::is_running(r.process));

        let web = start(&r, "web", long_running("web-started")).unwrap();
        wait_for_log(&r.dir.join("logs/web.log"), "web-started");
        assert!(proc::is_running(web));
        let err = format!("{:#}", start(&r, "web", long_running("x")).unwrap_err());
        assert!(err.contains("already running"), "{err}");

        let status = statuses(&r);
        assert_eq!(status.len(), 1);
        assert_eq!((status[0].process, status[0].ended), (web, None));

        request(&r.endpoint, &Request::Stop { name: "web".into() }).unwrap();
        assert!(proc::wait_gone(web, Duration::from_secs(5)));
        // Stopping what is not running is not an error.
        request(&r.endpoint, &Request::Stop { name: "web".into() }).unwrap();

        request(&r.endpoint, &Request::Shutdown).unwrap();
        assert!(proc::wait_gone(r.process, EXIT_WAIT));
        let err = request(&r.endpoint, &Request::Status).unwrap_err();
        assert!(is_unreachable(&err), "{err:#}");
    }

    fn statuses(r: &Running) -> Vec<ServiceStatus> {
        services_within(&r.endpoint, Duration::from_secs(10)).unwrap()
    }

    /// Waits until `name` is reported to have exited.
    fn ended(r: &Running, name: &str) -> Ended {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let service = statuses(r).into_iter().find(|s| s.name == name).unwrap();
            if let Some(ended) = service.ended {
                return ended;
            }
            assert!(Instant::now() < deadline, "still running");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn an_exit_is_noted_when_it_happens_not_when_somebody_asks() {
        let r = running();
        let before = now_ms();
        // Long enough not to be a start that failed.
        let process = start(&r, "short", short_running(3)).unwrap();
        assert!(proc::wait_gone(process, Duration::from_secs(10)));
        // A margin: the process can be gone a moment before its exit is noted.
        let gone = now_ms() + 500;
        std::thread::sleep(Duration::from_secs(2));
        let first = ended(&r, "short");
        assert_eq!(first.exit, Exit::Code(3));
        assert!(
            (before..=gone).contains(&first.at_ms),
            "{before} {} {gone}",
            first.at_ms
        );
        assert_eq!(ended(&r, "short"), first);
        // It is not running any more, so another one may take its name.
        start(&r, "short", long_running("again")).unwrap();
        assert_eq!(statuses(&r)[0].ended, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_service_that_was_killed_ended_with_a_signal() {
        let r = running();
        let process = start(&r, "web", long_running("web")).unwrap();
        proc::kill(process).unwrap();
        assert_eq!(ended(&r, "web").exit, Exit::Signal(libc::SIGKILL));
    }

    /// As `nohup`-like wrappers and some daemons leave it to what they start.
    #[cfg(unix)]
    #[test]
    fn an_exit_is_noticed_although_the_caller_ignores_sigchld() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = dunce::canonicalize(tmp.path()).unwrap();
        let endpoint = control::new_endpoint("test").unwrap();
        let ignoring = Launcher::unchecked(|endpoint| {
            let program = launcher().program(endpoint);
            let script = "trap '' CHLD; exec \"$0\" \"$@\"";
            let mut args = vec!["-c".into(), script.into(), program.exe.into_os_string()];
            args.extend(program.args);
            Program {
                exe: "/bin/sh".into(),
                args,
            }
        });
        let process = ignoring
            .launch(&endpoint, &dir, &dir.join("supervisor.log"))
            .unwrap()
            .process;
        let r = Running {
            _tmp: tmp,
            dir,
            endpoint,
            process,
        };
        start(&r, "short", short_running(3)).unwrap();
        assert_eq!(ended(&r, "short").exit, Exit::Code(3));
    }

    #[test]
    fn an_answer_of_an_older_supervisor_is_understood() {
        let old = r#"[{ "name": "a", "process": { "pid": 1, "start": 2 }, "exit": null },
                      { "name": "b", "process": { "pid": 3, "start": 4 }, "exit": "exit code: 1" }]"#;
        let old: Vec<ServiceStatus> = serde_json::from_str(old).unwrap();
        assert_eq!((old[0].ended, old[0].exit.as_deref()), (None, None));
        assert_eq!(
            (old[1].ended, old[1].exit.as_deref()),
            (None, Some("exit code: 1"))
        );
    }

    #[test]
    fn exits_are_structured() {
        let json = |exit| serde_json::to_string(&exit).unwrap();
        assert_eq!(json(Exit::Code(1)), r#"{"code":1}"#);
        assert_eq!(json(Exit::Signal(9)), r#"{"signal":9}"#);
        assert_eq!(Exit::Code(3).to_string(), "code 3");
        assert_eq!(
            Exit::Code(0xc000_013a_u32 as i32).to_string(),
            "code -1073741510, 0xc000013a"
        );
    }

    /// Nobody is ever served. On Windows the second one cannot even connect.
    #[test]
    fn a_supervisor_that_hangs_is_given_up_on() {
        let endpoint = control::new_endpoint("test").unwrap();
        let server = Server::bind(&endpoint).unwrap();
        for _ in 0..3 {
            let started = Instant::now();
            let err = services_within(&endpoint, Duration::from_millis(300)).unwrap_err();
            assert!(err.downcast_ref::<NoAnswer>().is_some(), "{err:#}");
            assert!(started.elapsed() < Duration::from_secs(5));
        }
        drop(server);
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
        let env = BTreeMap::from([(TREE_ENV.into(), mode.into())]);
        start_tree_as(r, tree(), env)
    }

    #[cfg(windows)]
    fn start_tree_as(
        r: &Running,
        run: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> (ProcId, ProcId) {
        let log = r.dir.join("logs/tree.log");
        let start = Start {
            name: "tree".into(),
            run,
            cwd: r.dir.clone(),
            env,
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

    /// As `npx vite` is: found on the `PATH` of the service, and stopped with all that runs behind
    /// `cmd`.
    #[cfg(windows)]
    #[test]
    fn a_service_behind_a_script() {
        let r = running();
        let bin = r.dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("bp-test-launch"), "#!/bin/sh\n").unwrap();
        std::fs::write(bin.join("bp-test-launch.cmd"), "@%*\r\n").unwrap();
        let mut run = vec!["bp-test-launch".to_owned()];
        run.extend(tree());
        let path = format!("{};{}", bin.display(), std::env::var("PATH").unwrap());
        let env = BTreeMap::from([(TREE_ENV.into(), "wait".into()), ("Path".into(), path)]);
        let (leader, child) = start_tree_as(&r, run, env);
        assert!(proc::is_running(leader));
        stop_tree(&r);
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

    /// Our copies of a pipe's writing end are closed after the spawn, and no other service holds one.
    /// Not on macOS: there std creates a pipe and marks it close-on-exec in two steps, so a process
    /// that another test of this binary starts in between would hold it.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_log_is_complete_as_soon_as_its_own_service_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let mut services = BTreeMap::new();
        for name in ["one", "two"] {
            let start = Start {
                name: name.into(),
                run: long_running("started"),
                cwd: dir.path().to_path_buf(),
                env: BTreeMap::new(),
                log: dir.path().join(format!("{name}.log")),
            };
            start_service(&start, &mut services).unwrap();
        }
        for name in ["one", "two"] {
            let service = services.get_mut(name).unwrap();
            wait_for_log(&dir.path().join(format!("{name}.log")), "started");
            assert!(!service.pump.wait(Instant::now()));
            service.stop().unwrap();
            assert!(service.pump.wait(Instant::now() + Duration::from_secs(10)));
        }
    }

    /// Starts a service that leaves a process behind, outside its group, with its output open.
    #[cfg(unix)]
    fn start_leaving(r: &Running) -> ProcId {
        // Job control gives the background process a group of its own.
        let script = "set -m; sleep 60 & echo child $! started; exec sleep 300";
        start(
            r,
            "leaving",
            vec!["bash".into(), "-c".into(), script.into()],
        )
        .unwrap();
        let log = r.dir.join("logs/leaving.log");
        wait_for_log(&log, " started");
        let text = read_log(&log);
        let (_, text) = text.split_once("child ").unwrap();
        let pid = text.split_once(' ').unwrap().0.parse().unwrap();
        proc::identify(pid).expect("the child runs")
    }

    #[cfg(unix)]
    #[test]
    fn a_process_that_left_the_tree_does_not_delay_stopping() {
        let r = running();
        let child = start_leaving(&r);
        let started = Instant::now();
        let stop = Request::Stop {
            name: "leaving".into(),
        };
        request(&r.endpoint, &stop).unwrap();
        request(&r.endpoint, &Request::Shutdown).unwrap();
        let exited = proc::wait_gone(r.process, EXIT_WAIT);
        let elapsed = started.elapsed();
        let outlived = proc::is_running(child);
        let _ = proc::kill(child);
        assert!(exited && elapsed < Duration::from_secs(8), "{elapsed:?}");
        assert!(outlived, "the child was in the group: nothing was tested");
    }

    /// The price of logs that pass through the supervisor. On Windows it dies in any case.
    #[cfg(unix)]
    #[test]
    fn a_service_that_writes_ends_after_a_killed_supervisor() {
        let r = running();
        let script = "while :; do echo tick; sleep 0.1; done";
        let web = start(&r, "web", vec!["sh".into(), "-c".into(), script.into()]).unwrap();
        wait_for_log(&r.dir.join("logs/web.log"), "tick");
        proc::kill(r.process).unwrap();
        let gone = proc::wait_gone(web, Duration::from_secs(10));
        let _ = proc::stop_recorded_tree(web, Duration::from_secs(1));
        assert!(gone);
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
        // On Unix a killed process can count as gone while its files are still open: on Linux while
        // any thread but the first is exiting, on macOS (by the kernel source) in any case. Until
        // they are closed connecting works, and the channel breaks. An answer is never right.
        let deadline = Instant::now() + Duration::from_secs(5);
        let err = loop {
            let err = request(&r.endpoint, &Request::Status).unwrap_err();
            let broken = !is_unreachable(&err) && !is_answer(&err);
            if !(cfg!(unix) && broken && Instant::now() < deadline) {
                break err;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
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
