# Initial design decisions

Captured from the design discussion that started the project (2026-11-22/23). The implementation plan is
written separately; this file records what was decided and why.

## Problem

The predecessor was a project-specific PowerShell script that started a per-worktree instance
(container + dev server). Its defects drove the requirements:

1. Git-ignored files from the main checkout were not carried over, except for a hard-coded list.
2. Teardown left resources behind: anonymous container volumes (`docker rm -f` without `-v`), dev-server
   processes found by terminal title only, nothing cleaned when the worktree was already deleted,
   ports re-derived from a branch hash on every run.
3. Half of the script was workarounds for the shell itself (encodings, stderr handling).

Lessons from running it on Windows, which the new tool must handle by design:

- Callers may run `bp` from `cmd.exe` with an OEM console code page: never decode child output with the
  console encoding, keep paths as OS strings, and treat non-ASCII paths as the normal case.
- Hook runners may pass placeholders unexpanded (`%VAR%` or `$VAR` literally): detect and reject them
  instead of treating them as paths.
- An IDE may start its session in a fresh worktree *before* calling the setup hook, so files provisioned by
  `bp up` can arrive after tools have already read their config.
- Hook output is often invisible: `bp` keeps its own log of every run.

## Scope

**In scope**

- Provision and tear down everything a worktree needs; the worktree itself may be created by any tool.
- Cross-platform: Windows, macOS, Linux.
- Anything is a configurable command: dev server (vite, bun, ...), container runtime (Docker, Podman, none),
  file sync (Mutagen), database.
- Clean only what `bp` created ("what we created is what we wipe").
- Background services under a `bp` supervisor; logs to files, `bp logs -f`.
- Config: a project file committed to git plus a user file layered on top (ports, paths).
- Open source from day one: MIT OR Apache-2.0.

**Out of scope**

- Creating worktrees as a feature to compete on (Orca, worktrunk, gtr, grove-wt already do it).
- Cleaning foreign resources: global prune, unused images of other projects, `gc --deep`.
- Compacting Docker Desktop disk images (vhdx / raw).
- A database per worktree in v1: one shared database for all worktrees (machine resources).

## Core model: resources

| Kind | Create | Destroy | Example |
|---|---|---|---|
| `git` | `git worktree add` (optional thin wrapper) | unlink links, then `git worktree remove`, `prune` | the worktree |
| `file` | copy / reflink / link / template | delete; links removed **as links** | `.env`, `node_modules` |
| `port` | allocate from a range, verify by binding | release in the registry | web, dev server |
| `command` | `create` command, stdout captured into a variable or a file | `destroy` with those variables, `gc` query | container, volume, sync session |
| `service` | long-running process under the supervisor | the whole process tree dies | dev server |

Rules:

- **Write-ahead ledger** stored in the user data dir (not inside the worktree): intent is written before a
  resource is created, its id after. Partial creation is visible to teardown.
- **Teardown** runs in reverse order, every step idempotent; errors are collected, not fatal; leftovers go to
  `gc`.
- **Order matters on Windows:** stop processes before deleting files (locks); remove links before any
  recursive delete so it never follows a junction into the main checkout.
- **`gc` is a primary path, not a fallback:** a worktree removed behind our back (IDE auto-cleanup,
  `git worktree remove`) is a normal situation. `gc` matches ledgers without worktrees, recorded process
  groups (pid + start time against pid reuse), unused registry ports, and user-defined `gc` queries
  (labels).
- **Labels everywhere** (`bp.pod=<id>`) on containers, volumes, networks, sync sessions — `gc` finds
  leftovers without the ledger.

## Teardown (`bp down`)

- Driven by the ledger alone: the pod is found by its recorded worktree path, commands are undone in the
  worktree root while it exists and in the `bp` home otherwise (not the pod's state directory: a daemon
  started by `destroy` would keep it as its working directory). A path given for a deleted worktree is
  matched in canonical form (deepest existing ancestor canonicalized, `..` resolved), so `..`, 8.3 names
  and `/var` vs `/private/var` still find the pod.
- Every resource that is not destroyed is visited, pending and failed ones included. Each outcome is
  recorded (`destroyed`, or `failed` with the error and the data kept for the next attempt); a failure
  never stops the other steps.
- Order: the reverse of creation, except that services stop first, then their supervisor, and file
  links are removed before anything else.
- Exit code: `0` nothing is left, `3` resources are left behind (run it again once the cause is fixed),
  `1` teardown could not run (the ledger cannot be read). `2` stays with usage errors.
- Once nothing is left, the pod's state directory is removed too (`pod.json` last, so an interruption
  leaves a pod with nothing to tear down); its lock file is kept. Files with command output
  (`output = "file"`) go with it and not earlier: after an exit with code `3` a `destroy` that reads one
  runs again.
- Known limitation until `gc` exists: a resource that cannot be undone from the ledger (a command whose
  `destroy` needs output never captured, kinds without teardown yet) keeps `bp down` at exit code `3`;
  there is no way to forget it.

### Files that are in use

On Windows a directory cannot be removed while a process has a file open inside it or its working
directory there; teardown must never stop halfway through such a directory.

- A copied directory is renamed to a trash name next to it (`<name>.bp-trash-<unique>`, same volume)
  and deleted from there. On Windows the rename fails while anything inside is in use — an open file, even
  one opened with `FILE_SHARE_DELETE`, or a process's working directory — and then nothing has been
  touched. The trash name is chosen and recorded in the ledger (`moved`) before the rename, so every
  interruption is finished by the next run: a leftover trash is deleted, a remaining directory is moved to
  the recorded name. Single files and links are removed in one operation.
- Removal and the rename retry with backoff on the transient sharing errors (a scanner or indexer holds a
  file for a moment).
- A lock that persists is reported with the processes holding it when the OS can tell: on Windows the
  Restart Manager (`RmGetList`) over the files of the path. It does not report a process whose only tie
  is its working directory (checked by hand); the report then still says that the path is in use. On
  Unix open files do not block removal and nothing is reported.

## Process supervision

One supervisor process per worktree, no global daemon; control over a local socket / named pipe.

- **Start.** `bp up` starts it after the commands, only when the config has services, as the hidden
  `bp supervise <endpoint>` of the same binary, detached from the caller: its own session (Unix); no
  console window, its own process group and, when the caller's Job Object allows it, outside that job
  (Windows; "The caller's Job Object" below is about when it does not). `bp`'s
  standard handles are not inherited: an IDE hook waiting for the end of `bp`'s output must not wait for
  the supervisor. Known gap on Windows (#76): other inheritable handles of `bp` still are, and some callers
  leave it one on their output; those do wait. Its working directory is the `bp` home, never the worktree or the pod directory, which
  Windows could then not remove.
- **Readiness.** The supervisor prints one line once it listens (or why it cannot) and `bp up` reads it.
  No time limit: if the supervisor dies first, the pipe closes and `up` fails.
- **Control.** One JSON line per request and per answer, one connection per request, versioned:
  `start` (a service fully rendered by `bp up`, so the supervisor needs no config), `stop`, `status`,
  `shutdown`. The protocol is internal; the public contract stays `--json` and events. Unix: a socket
  in a directory only the user can enter (`$XDG_RUNTIME_DIR/branchpod`, else `<temp>/branchpod-<uid>`):
  socket paths are limited to about 104 bytes, more than a data directory path leaves. Windows: a named
  pipe created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, rejecting remote clients, with an explicit DACL for
  the current user and SYSTEM only (the default one lets everyone open it for reading) and a spare
  instance always open; clients connect at identification level only. Endpoints carry a random part
  and are recorded in the ledger. The supervisor serves one request at a time and gives up on a client
  that sends nothing for 10 s. Clients have no time limit for the answer: a supervisor that listens but
  hangs keeps `bp down` waiting, like any other step.
- **Ledger.** The supervisor never writes the ledger: it reports, and `bp up` / `bp down` record under
  the pod lock, so the ledger keeps a single writer. The supervisor is a resource of its own
  (`supervisor`: endpoint, pid + start time); each service records its pid + start time. The start
  time is the OS's own token (Linux `/proc/<pid>/stat` field 22, macOS `proc_pidinfo`, Windows
  `GetProcessTimes`), not wall-clock time; a pid with another start time is another process.
- **Services** are not restarted; one that exits keeps its status until it is stopped. Their stdout and
  stderr go to `pods/<id>/logs/<service>.log` through an inherited handle, not through the supervisor,
  so output keeps flowing if the supervisor dies. Rotation and `bp logs` are #17. The environment is
  passed to the supervisor but not recorded (it may hold secrets).
- **Stop.** `bp down` asks the supervisor to stop each service, then to shut down, and waits for it to
  exit (killing it after 10 s). A stopped service gets `SIGTERM` to its group and 10 s before `SIGKILL`
  (Unix); on Windows its job is terminated at once, and 10 s is the limit for all of it to be gone.
  When nothing listens on the supervisor's endpoint (or the channel breaks, as it does when the
  supervisor is being killed, and its recorded process is gone), a service's tree is stopped by its recorded leader: on Unix
  `killpg(pid)` unless the pid now belongs to another process (a pid is not reused while a group with
  that id exists, so a group whose leader is gone is still ours). A service that never reported its
  pid was known only to that supervisor and is considered gone with it (it is, once the platform
  measures below tie services to the supervisor's life).
- Windows: Job Objects, see "Services on Windows".
- Linux: own process group + `PR_SET_PDEATHSIG` (#15); cgroups via `systemd-run --user --scope` when
  available.
- macOS: process group + kqueue (#16); if the supervisor is SIGKILLed, `gc` finishes the group.
- Known limitation: while a supervisor runs, Windows keeps the `bp` executable in use, so replacing it
  in place (an upgrade) may fail until the pods with services are down.

### Services on Windows

- **Two levels of Job Objects**, all with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The supervisor puts itself
  into a job of its own before it listens; every service gets a job nested in it. Only the supervisor has
  handles to them, not inherited, so when it ends, killed or not, the system closes them and kills the
  trees. Nested jobs need Windows 8 / Server 2012, below our floor
  ([nested jobs](https://learn.microsoft.com/en-us/windows/win32/procthread/nested-jobs)). That the handle
  of a killed process counts as closed is not spelled out there; the tests check it.
- **No gap at the start.** A service is created suspended, put into its job and then resumed. It is born in
  the supervisor's job, so a supervisor killed in between takes it along. `std` gives no stable handle to
  the main thread: it is found in a thread snapshot (Toolhelp). Rejected: calling `CreateProcessW` ourselves
  with `PROC_THREAD_ATTRIBUTE_JOB_LIST` (it would replace the program lookup and argument quoting of `std`).
  If the service cannot be put into its job it is killed and the start fails.
- **Stop** terminates the service's job (there is no request to exit first) and waits for every process that
  was in it, also when the service process itself exited before. The job's count of active processes alone
  is not enough: it can reach zero before the processes are gone and their files free. What has not exited
  after 10 s is an error: the service stays recorded and the pod is not clean.
- **`bp down` without a supervisor.** The trees are already being killed, but not necessarily gone. `down`
  waits for the recorded leader; a leader that refuses to be terminated ("access denied", as a process
  being terminated does) counts as gone only once it has exited. The rest of the tree is not recorded: the
  retries of file removal cover the moment it takes.
- **The caller's Job Object.** A supervisor that stays in a job that kills it on close dies with the IDE hook
  that ran `bp up`, after the pod was reported as up.
  - A job with `KILL_ON_JOB_CLOSE` that does not allow breakaway (`BREAKAWAY_OK` or `SILENT_BREAKAWAY_OK`):
    `bp up` fails and says what to do. Checked twice: by `bp up` before anything is created (only if the
    config has services), and by the supervisor, which alone knows where it ended up. Breaking away moves up
    the chain of jobs only to the first one that forbids it, so a start with `CREATE_BREAKAWAY_FROM_JOB` can
    succeed and still end in a job that kills.
  - A job without `KILL_ON_JOB_CLOSE` that does not allow breakaway: `bp up` goes on and warns on stderr.
    Refusing would rule out places where everything runs in some job.
  - Known gap: a process can query only
    [its innermost job](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-queryinformationjobobject).
    An outer job that kills behind an inner one that does not gets the warning, not the error.
  - There is no switch to skip the check. Tests of the library do without it in code; `cargo run` and
    `cargo test` run programs in exactly such a job, see [CONTRIBUTING.md](../../CONTRIBUTING.md).
- **Accepted limitations.** Our jobs do not allow breakaway, so `CREATE_BREAKAWAY_FROM_JOB` fails in a
  service and its children ("access denied"): nothing that a service starts may outlive it. The exception
  is what others start on its behalf, such as a process created through WMI (`Win32_Process.Create`), which
  is [not in the job](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects); `bp` neither
  sees nor stops it.
- A caller's job with UI restrictions: nesting into it works on Windows 11 (tried). Microsoft's pages disagree
  on it ([nested jobs](https://learn.microsoft.com/en-us/windows/win32/procthread/nested-jobs),
  [AssignProcessToJobObject](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-assignprocesstojobobject));
  other versions are not checked. Where it does not work, the supervisor does not start and says so.

## Git-ignored files

Explicit rules `copy | reflink | link | template | ignore` plus `bp doctor`, which lists ignored paths not
covered by any rule. Copying everything by default is rejected (vendor dirs, caches, large build output).
Templates substitute per-worktree values (ports, URLs) into copied files.

Rules for file resources:

- A missing source is an error; `optional = true` skips the rule instead.
- An existing destination is an error: `bp` never overwrites (and so would later delete) a file it did not
  create, such as a committed `.env`. Nothing is created through a link either.
- Creation is planned before the ledger intent is written: source, destination and template rendering are
  checked without touching the disk, and the intent carries the planned path, so an interrupted creation is
  undone from the ledger alone.
- Teardown removes a path only while it is still what was created (file, directory, link) and is reached
  without passing through a link; otherwise it is left in place and reported. A copied directory is removed
  with whatever was written into it; parent directories created for it are removed while empty.
- `link`: symlink on Unix; on Windows a junction for directories (no privileges needed) and a symlink for
  files. Without the right to create a file symlink (Developer Mode or admin) it is an error with a hint,
  not a silent copy: a copy would quietly stop propagating edits. Hard links are rejected (editors that
  save by rename break them).
- `template`: UTF-8 text, a single file. `{{var}}` substitution is strict; `replace` rules run after it and a
  rule that matches nothing is an error (drift in the source is noticed). Files that legitimately contain
  `{{ }}` (Blade, Vue) set `vars = false` and keep only `replace`; the `with` side is always substituted.
  A few literal brackets can stay in template mode written as `{{{{` (see "Literal `{{`"). `find` is
  literal and matches the rendered text, so an escaped `{{{{` is matched as `{{`. Filters work wherever
  variables do, so in `with` also when `vars = false`.

## Command resources

- `create` and `destroy` are argument arrays run without a shell, stdin closed, output captured. A
  non-zero exit fails the step; its stderr is quoted (tail) in the error and kept whole for the run log.
- stdout is decoded as UTF-8, never with the console code page; invalid UTF-8 is an error. A leading
  byte order mark is dropped and surrounding whitespace trimmed; the result is
  `{{command.<name>.stdout}}`.
- The ledger stores `destroy` fully rendered, captured output included, so teardown needs neither the
  config nor the worktree. When `destroy` does not reference its own output (a deterministic name such
  as `bp-{{pod.id}}`), it is rendered into the intent, so an interrupted creation is still undone;
  otherwise such a resource is left to `gc` by label.
- Teardown must never run with an empty or garbled id. Empty output defines no variable, so any
  reference to it is an unknown name and fails when planning. A rendered `destroy` argument must be a
  single line, whichever command's output it comes from: a warning printed next to an id would otherwise
  become part of it. Nor may it contain a NUL (UTF-16 output), which no process can be given. Both hold for
  the values it uses before filters too, since `json` would turn these characters into escapes. Output that
  no `destroy` depends on is kept whole.
- A program given as a relative path with a directory part (`./scripts/up.sh`) is resolved against the
  command's working directory on every platform; Windows would otherwise resolve it against `bp`'s own.
  A bare name is searched on `PATH`.
- `output = "file"` keeps the output in a file instead, see "Command output in a file".
- On Windows a `.cmd` / `.bat` program is named with its extension (`npx.cmd`): spawning without a
  shell does not resolve it through `PATHEXT`.

### Command output in a file

For output that other tools read from a file, typically the environment of a base container:

```toml
[[command]]
name = "env"
create = ["docker", "inspect", "--format", "{{{{range .Config.Env}}{{{{println .}}{{{{end}}", "base-app"]
output = "file"

[[command]]
name = "app"
create = ["docker", "run", "-d", "--name", "bp-{{pod.id}}", "--env-file", "{{command.env.file}}", "example/app"]
destroy = ["docker", "rm", "-f", "-v", "bp-{{pod.id}}"]
```

- `output = "file"` (the default is `"stdout"`) writes the stdout of `create` to `pods/<id>/outputs/<name>`
  byte for byte: nothing is decoded or trimmed. `{{command.<name>.file}}` is the absolute path;
  `{{command.<name>.stdout}}` is then unavailable, with an error that says so.
- Blank output (nothing, or only a byte order mark and whitespace) fails the step: an empty env file would
  be taken for a real one. Whitespace counts because tools end even an empty result with a line break.
- An env file has one variable per line, so the example does not carry values with line breaks (PEM, JSON)
  over: each further line is taken for a variable of its own, or rejected. Docker then quotes the line, or
  its part before `=`, on stderr, which shows in the error of `bp up`, the ledger and `bp status --json`.
- The output may hold secrets, so it is nowhere but in the file: not in the ledger, `bp status --json`, what
  `bp up` prints, or an error. The run log (#9) must not get the stdout of such a command either. stderr is
  not part of the output and is quoted in errors as for any command, as is the stderr of a command that
  reads the file: a tool that prints secrets there shows them.
- On Unix the file has mode 0600 from the moment it is created, in a directory with mode 0700: a container
  that mounts the file and runs as another user cannot read it. On Windows both inherit the permissions
  of the `bp` home; the default one is in the user's profile.
- The path is known before `create` runs, so the intent has it and `destroy` is always rendered into the
  intent, also when it references the file. The file is written atomically.
- `bp up` on a pod with nothing left to destroy starts by removing the whole `outputs` directory, which an
  earlier `down` may have failed to remove: its files may hold secrets, under names the config no longer
  has, and must not pass for new output. If it cannot be removed, `up` fails.
- After an interruption before the file was written, or a failed `create`, `bp down` still runs `destroy`
  but the file does not exist. `destroy` should therefore find the resource by its name or label, not need
  the file.
- The order of kinds is unchanged: later commands and services can use the file, file resources cannot.
- The name must not be a Windows device name (`nul`, `aux`, ...), on any platform.
- Compatibility: the ledger gets an optional `file` field, and older records are read as before. A `bp`
  without this feature refuses a config with `output` (`unknown field`, before anything is created), in
  `up` and `config`; its `status` and `down` read no config, ignore the new field and tear such a pod
  down, file included.
- Rejected: arguments that expand per line of output (`"-e", "{{command.env.lines}}"`). A variable would be
  a list that only some places accept, and the lines, with their secrets, would be in the process list and
  in the ledger. Mounting a worktree root entry by entry, the other case for expansion, stays a list in the
  config, at the price of a config change for each new top-level entry.

## Pod lock

- One `up` / `down` at a time per pod: IDE hooks may fire both for a worktree in quick succession, and the
  second one waits for the first.
- The lock is an OS file lock (`flock` / `LockFileEx`), not a pid file. The OS releases it when the holder
  exits, crashes or is killed, so a lock is never stale and nothing detects or takes one over. Child
  processes do not inherit it.
- Lock files live in `locks/<pod id>.lock` in the `bp` home, outside the pod directory, and are never
  deleted: removing one while a process waits on it would let two processes hold the lock at once.
- Waiting has no timeout: a waiter cannot tell a long `up` (first image pull) from a hung one, so any fixed
  limit either breaks the normal case or is too long to matter. The waiter reports who holds the lock
  (command, pid, how long), recorded in a separate file because a locked file cannot be read on Windows.
- Steps have no time limit either, for the same reason: `bp` never kills a running `up` / `down` or one of
  its commands. A hung step (a child that never exits, or a background descendant that keeps the
  inherited output pipes open) holds the lock until the user stops the holder the waiter names; what was
  created or intended stays in the ledger for `bp down`.
- `gc` takes a pod's lock only if it is free and skips busy pods instead of waiting.
- The lock is not re-entrant: taking it again in the same process is an error, not a deadlock.
- Lock order: the pod lock before the port registry lock, never the other way round.

## `bp up`

- Resources are created in a fixed order of kinds — ports, files, commands, then the supervisor and
  services — and within a kind in config order (ports by name: they never depend on each other). Templates and commands reference
  ports; a command (a container) may mount a file provisioned for it. The price: a file cannot reference
  `{{command.<name>.stdout}}` or `{{command.<name>.file}}`. A single ordered list or ordering by references
  would lift that, at the cost of a format change or implicit ordering.
- `up` is not re-entrant: if the ledger holds any resource that is not destroyed (live, pending or failed),
  it fails at once, lists them and asks for `bp down` first. Reconciling a live pod with a changed config
  is a separate feature, not a side effect of `up`.
- Before the first resource is created, the whole config is planned once with stand-in values for what
  only creation yields (the first port of each range, a placeholder for command output): a typo in the
  last command or a missing source fails before any port is allocated. Only what creation itself reveals
  (a command failing, empty output a `destroy` needs) fails midway.
- On failure nothing is rolled back: what was created stays recorded, and the error lists it with the
  `bp down` command to run. A resource rejected while planning records nothing.
- `up` in the main checkout is refused when the config has file rules: files are carried over from the
  main checkout into linked worktrees, so there every destination is its own source.
- Ports are allocated afresh: a registration the pod still holds in the registry with nothing live in its
  ledger is stale (a lost ledger) and is released first, so the port is bind-checked again.
- Built-in variables: `pod.id`, `worktree`, `main` (main checkout), `branch`. When one has no value (a
  detached HEAD, a bare repository, a path that is not UTF-8), a reference to it fails and says why.
- `up` returns once every service is started (its process exists), not once it is ready: readiness
  hooks are #18.

## File sync (Mutagen)

Plain resources, no special-casing in the core beyond a readiness hook:

- sidecar pattern: labelled volume + small container running the sync agent; the app container mounts the
  volume, so recreating the app does not break the sync;
- readiness: `mutagen sync flush` before services start and before `bp exec` (sync can stall silently);
- teardown: terminate the session **before** deleting the local directory, then remove the volume / remote
  copy (the classic disk leak);
- ignores are passed as an argument array, never through a shell (Git Bash rewrites `/data` into a
  Windows path).

Docker Desktop's built-in synchronized file shares require a paid subscription; open-source Mutagen with the
`docker://` transport does not.

## Containers and caches

- Remove containers with their anonymous volumes (`rm -v`), plus volumes, networks and images created for
  the worktree (tag + label).
- Build cache cannot be filtered by label: enforce an age / size limit instead.

## Integrations and vendor lock-in

- The core never calls IDEs or orchestrators and works without any of them.
- Inbound: any tool calls `bp up <path>` / `bp down <path>` (e.g. from worktree hooks).
- Outbound: lifecycle events (`pod.up`, `service.started`, `service.exited`, `pod.down`, `gc.cleaned`)
  delivered as JSON on stdin to external plugins `bp-<name>` on `PATH`, with a timeout; a failing plugin
  never breaks `up` / `down`.
- The `--json` output and event schema are a versioned public API.
- Example: an Orca plugin opens a log tab (`bp logs -f <service>`) on `service.started`; closing the tab
  does not stop the service. Orca also attributes listening ports to a worktree by process cwd, so services
  started with cwd inside the worktree show up without any plugin; container-published ports do not.

### Editor and IDE plugins

The contract is designed for editor integrations from day one; the plugins themselves are built one at a
time, driven by real use:

1. **Orca** first — used daily by the author, so it validates the contract.
2. **VS Code** second — the largest audience and an open API: pods and services in a tree view, logs in
   the Output panel, status bar item, up / down / restart actions.
3. **JetBrains** on demand — a single plugin covers PhpStorm, IntelliJ IDEA, WebStorm and the rest of the
   platform, but it is the most expensive to build and maintain.

What the contract must provide for them: `bp status --json`, a change stream (`bp events --json` /
`bp status --watch --json`), and stable identifiers for pods, services and resources.

Plugins live in this repository under `integrations/<name>` (one pull request can change the contract and
every client), each with its own toolchain and release channel (VS Code Marketplace, JetBrains Marketplace,
the Orca plugin catalog).

## Implementation

- **Rust.** A single binary with no runtime; first-class access to Windows APIs (Job Objects, named
  pipes, junctions); argument-array process spawning by default; paths as OS strings.
- **Supported operating systems:** Windows 10 and 11, macOS, Linux. The floor is what Rust requires
  of its targets at our MSRV (1.89), per the rustc platform support page; `bp` adds no older-OS
  workarounds:
  - Windows: 10+ (Server 2016+) since Rust 1.78 for the `*-pc-windows-*` targets. CI runs
    `windows-latest`, a Windows Server image; Windows 10 itself is not tested.
  - macOS: 11.0+ (Big Sur) on Apple silicon, 10.12+ (Sierra) on x86_64. CI runs `macos-latest`
    (Apple silicon).
  - Linux (glibc): kernel 3.2+ on x86_64, 4.1+ on ARM64, glibc 2.17+. CI runs `ubuntu-latest`.
- **TOML config.** `branchpod.toml` is read from the worktree (each branch carries its own version);
  `branchpod.local.toml` is read from the main checkout (git-ignored files do not exist in a fresh
  worktree) and layered on top: tables merge key by key, arrays are replaced. Without a project file the
  local file alone is the configuration and must set `version` itself, so `bp` works for a project whose
  team does not commit a config; with neither file it is an error naming both places. Chosen over YAML for the
  absence of implicit typing (`no`, `0755`) and significant indentation, and for comment-preserving edits
  (`toml_edit`) needed by future `bp init` / `bp config set`. The schema has a required `version`.
- **Variables** are written `{{name}}` (`port.<name>`, `command.<name>.stdout`, `command.<name>.file`, `pod.id`,
  ...). An unknown name is an error, never an empty string.
- **Filters** are written `{{name | filter}}`: left to right, without arguments, each from string to string.
  The variable is checked first; an unknown or empty filter (`{{a || b}}`) is an error.
  - `json` escapes a value for use inside a JSON string (`"`, `\` and U+0000 to U+001F, RFC 8259). It adds
    **no quotes**, so that the value can be a part of one: `"{{worktree | json}}/src"`. The result also fits
    a TOML basic string, unless the value contains U+007F.
  - `slash` writes a Windows path with `/`: `C:/work/feature-x`, `//server/share`. A value that starts with
    `\\?\UNC\server\share` (how a worktree on a network share is spelled) becomes `//server/share` too. Any
    other `\\?\` path in a value is an error on Windows, since `/` is not a separator there. On Unix it
    changes nothing: `\` is an ordinary character of a file name. Combined, `slash` comes first:
    `{{worktree | slash | json}}`.
  - Text of another tool that happens to be a valid reference, such as `{{ branch | json }}`, used to be an
    error and is now rendered: write `{{{{ branch | json }}`, or set `vars = false` for the file.
  - Rejected: a variable per spelling (`worktree.slash`), which multiplies with every path.
- **Literal `{{`** is written `{{{{`, in command arguments and templates alike, for tools that use the same
  brackets (Go templates in `docker` / `podman --format`, `kubectl -o go-template`, Helm):
  `["docker", "inspect", "-f", "{{{{.Config.Image}}", "app"]` passes `{{.Config.Image}}`. Input is read
  left to right, so `{{{{{{port.web}}}}` renders as `{{8003}}`. Only the opening bracket has an escape: a
  `}}` outside a reference is already literal, and making `}}}}` mean `}}` would silently change valid
  text such as minified JSON (`{"a":{"b":{"c":{}}}}`). `{{{{` was an error before, so the escape changes
  no working config. Rejected: `{{ "{{" }}` (verbose, needs quote-aware parsing and escaped quotes in TOML)
  and a backslash (collides with Windows paths such as `C:\{{pod.id}}`).
- **Ledger** is JSON Lines per pod in the user data directory (`$BRANCHPOD_HOME` overrides it); a torn last
  line is tolerated, corruption elsewhere is reported. Pods are found by their recorded worktree path, so
  a deleted worktree is still found.
- **The home is always absolute.** A relative `$BRANCHPOD_HOME` (or default) is made absolute once, against
  the directory `bp` is called from, and not canonicalized: the home need not exist yet. An empty value
  means "not set".
- **Pod id** is `<slug>-<8 hex of the path hash>`, limited to `[a-z0-9-]` for use in labels.
- The binary is installed as `branchpod`; `bp` is an optional alias (see "Name").

## Name

`branchpod`, short binary `bp` (optional alias: `bp` is taken on crates.io by a clipboard tool, so the
installer must not overwrite an existing `bp`).

Rejected: `grove` (taken; neighbour project grove-wt), `branchbox` (active neighbour project with the same
idea, macOS/Linux, devcontainer/compose — a useful reference), `bb` (babashka), `workfork` ("fork"
suggests repository or process forks).

## Prior art

- worktrunk — worktree manager with hooks, `copy-ignored` with reflink, experimental `tether`; no resource
  ledger or gc for hook-created resources.
- grove-wt — worktree creation, task DAG, CoW copy, btrfs subvolumes; no services, ports or gc.
- BranchBox — per-feature devcontainer / compose environments; cleans its compose project, not standalone
  volumes/networks; no Windows.
