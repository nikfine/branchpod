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
| `command` | `create` command, stdout captured into variables | `destroy` with those variables, `gc` query | container, volume, sync session |
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

## Process supervision

One supervisor process per worktree, no global daemon; control over a local socket / named pipe.

- Windows: Job Object with `KILL_ON_JOB_CLOSE`.
- Linux: own process group (`setsid`) + `PR_SET_PDEATHSIG`; cgroups via `systemd-run --user --scope` when
  available.
- macOS: process group + kqueue; if the supervisor is SIGKILLed, `gc` finishes the group.

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
  reference to it is an unknown name and fails when planning. Output that a command's own `destroy`
  references must be a single line: a warning printed next to the id would otherwise become part of it.
  Output nothing in `destroy` depends on is kept whole.
- A program given as a relative path with a directory part (`./scripts/up.sh`) is resolved against the
  command's working directory on every platform; Windows would otherwise resolve it against `bp`'s own.
  A bare name is searched on `PATH`.
- On Windows a `.cmd` / `.bat` program is named with its extension (`npx.cmd`): spawning without a
  shell does not resolve it through `PATHEXT`.

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
- **TOML config.** `branchpod.toml` is read from the worktree (each branch carries its own version);
  `branchpod.local.toml` is read from the main checkout (git-ignored files do not exist in a fresh
  worktree) and layered on top: tables merge key by key, arrays are replaced. Chosen over YAML for the
  absence of implicit typing (`no`, `0755`) and significant indentation, and for comment-preserving edits
  (`toml_edit`) needed by future `bp init` / `bp config set`. The schema has a required `version`.
- **Variables** are written `{{name}}` (`port.<name>`, `command.<name>.stdout`, `pod.id`, ...). An unknown
  name is an error, never an empty string.
- **Ledger** is JSON Lines per pod in the user data directory (`$BRANCHPOD_HOME` overrides it); a torn last
  line is tolerated, corruption elsewhere is reported. Pods are found by their recorded worktree path, so
  a deleted worktree is still found.
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
