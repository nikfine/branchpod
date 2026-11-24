# branchpod

> A resource runtime for git worktrees: spin up everything a branch needs, track it, and clean it up completely.

A fresh git worktree has the committed files and nothing else: no `.env`, no free port for the dev server, no
container of its own. `branchpod up` provides these from a config file and records everything it creates;
`branchpod down` removes exactly that, also when the worktree directory is already gone.

It does not create worktrees: use git, an IDE or any other tool for that, and have it call `up` and `down`.
Containers, dev servers and file sync are plain commands in the config; `branchpod` knows none of these tools by
name. It runs on Windows, macOS and Linux.

This is an early version (0.x): the config format, the state on disk and the output may still change. Read
[Limitations](#limitations) before relying on it.

## Installation

There are no prebuilt binaries. Build it from source with Rust 1.89 or newer:

```
cargo install --locked --git https://github.com/nikfine/branchpod
```

The command is `branchpod`. Messages and the design document call it `bp`. At run time it needs `git` on `PATH`.

## Quick start

The example needs [Node.js](https://nodejs.org) 20.6 or newer. The commands are the same in every shell. Start
with a new repository:

```
git init demo
cd demo
```

Create four files in it. `branchpod.toml`:

```toml
version = 1

# A free port from this range, different in every worktree.
[port.web]
range = [8001, 8020]

# Git-ignored, so git does not bring it into a worktree: rendered from the one in the main checkout.
[[file]]
path = ".env"
mode = "template"
replace = [{ find = "PORT=8000", with = "PORT={{port.web}}" }]

# Anything that can be created and destroyed, such as a container. Here: a ref that remembers the commit
# the worktree had at `up`.
[[command]]
name = "base"
create = ["git", "update-ref", "refs/pods/{{pod.id}}", "HEAD"]
destroy = ["git", "-C", "{{main}}", "update-ref", "-d", "refs/pods/{{pod.id}}"]

# A process that runs until `down`.
[[service]]
name = "web"
run = ["node", "--env-file=.env", "server.js"]
```

`server.js`:

```js
const http = require("node:http");
const port = process.env.PORT;
http
  .createServer((req, res) => res.end(`hello from port ${port}\n`))
  .listen(port, "127.0.0.1", () => console.log(`listening on http://127.0.0.1:${port}`));
```

`.env`:

```
PORT=8000
```

`.gitignore`:

```
.env
```

Commit, create a worktree and bring it up:

```
git add .
git commit -m "Demo"
git worktree add ../demo-feature
branchpod up ../demo-feature
```

The id, the port and the pids will be different:

```
pod demo-feature-1a2b3c4d is up (/work/demo-feature)
  port       web                      8014
  file       .env                     template
  command    base                     (no output)
  supervisor supervisor               pid 4320
  service    web                      pid 4321
```

A *pod* is everything created for one worktree. Look at it, and at what the service printed:

```
branchpod status ../demo-feature
branchpod logs web ../demo-feature
```

Open the address from the log, then tear the pod down and remove the worktree:

```
branchpod down ../demo-feature
git worktree remove ../demo-feature
```

```
pod demo-feature-1a2b3c4d (/work/demo-feature)
  destroyed service    web
  destroyed supervisor supervisor
  destroyed command    base
  destroyed file       .env
  destroyed port       web
clean
```

## Commands

The path is optional and defaults to the current directory.

| Command | What it does | Exit codes |
|---|---|---|
| `branchpod up [PATH]` | Creates what the config describes. Refuses if the pod already has anything recorded: run `down` first. | `0` everything was created, `1` failed |
| `branchpod status [PATH] [--json]` | Shows what is recorded for the pod. | `0`, also when there is no pod; `1` failed |
| `branchpod down [PATH]` | Tears down everything recorded, in reverse order. | `0` nothing is left, also when there is no pod; `3` something is left: fix the cause and run it again; `1` could not run |
| `branchpod logs SERVICE [PATH] [-f] [-n N]` | Prints the output of a service: the last 200 lines, `-n N` or `-n all`. `-f` follows it until the service is gone. | `0`; `1` no such pod, service or log, or failed |
| `branchpod config [PATH] [--json]` | Prints the effective configuration, without rendering variables. Creates nothing. | `0`; `1` no configuration, or it is invalid |

A usage error exits with `2`.

When `up` fails before creating anything, there is nothing to clean up. When it fails midway, what was created
stays, and the message lists it with the `down` command to run.

State (what was created, service logs) is kept outside the worktree, in `$BRANCHPOD_HOME` if it is set and in the
user's local data directory otherwise.

## Configuration

`branchpod.toml` is committed and read from the worktree, so every branch has its own version.
`branchpod.local.toml` is personal, should be git-ignored, and is read from the **main checkout** only. It is
layered on top: tables are merged, arrays such as `[[file]]` are replaced as a whole. Either file can be the whole
configuration.

| Section | Creates | `down` |
|---|---|---|
| `[port.<name>]` | a port from a range, as `{{port.<name>}}` | releases it |
| `[[file]]` | a file or directory from the main checkout: `copy`, `reflink`, `link`, `template` | removes it |
| `[[command]]` | whatever the `create` command creates | runs `destroy` |
| `[[service]]` | a long-running process | stops its process tree |

They are created in this order. Commands and services are argument arrays and run without a shell. Values are
written `{{name}}`, optionally with a filter: `{{worktree | json}}`. An unknown name is an error, never an empty
string.

**Every key, variable and rule: [docs/config.md](docs/config.md).**

## Limitations

- **Windows 10 has not been tested**, although it is meant to work. CI runs on a Windows Server image, macOS on
  Apple silicon and Ubuntu.
- **`up` does not wait until a service is ready**, only until its process exists. A service that exits later is
  not restarted, and `status` still shows it as `live`: it shows what was recorded. `logs` shows why it ended.
- **macOS and Linux: a service can outlive its supervisor.** If the supervisor is killed, services are not
  ([#15](https://github.com/nikfine/branchpod/issues/15),
  [#16](https://github.com/nikfine/branchpod/issues/16)); `down` stops them. A killed process may also still hold
  its files and sockets for a moment when it already counts as gone
  ([#84](https://github.com/nikfine/branchpod/issues/84)).
- **`down` deletes a copied or rendered file even if it was edited since**, without a warning
  ([#67](https://github.com/nikfine/branchpod/issues/67)). Keep lasting changes in the main checkout.
- **Directories.** A copied directory is removed with everything that was put into it later. A linked one *is* the
  directory of the main checkout: what is written there shows up in every worktree. On Windows a copied directory
  that is in use cannot be removed: `down` leaves it, exits with `3` and can be run again.
- **Windows: a daemon that a command leaves running keeps whoever waits for the end of the output of `up` or
  `down` waiting** ([#82](https://github.com/nikfine/branchpod/issues/82)).
- **Windows: `up` refuses to start services** when the program that called it would take them down with itself
  (a Job Object that kills its processes on close). `cargo run` is such a caller.
- **Only what `destroy` can undo is undone.** If it fails, or needs output that was never captured, `down` keeps
  exiting with `3`; there is no command yet that cleans up or forgets such a resource. Messages that mention
  `bp gc` refer to one that does not exist yet.
- **Heuristic antivirus scanners may flag a local build on Windows.** It happened to one of ours; the vendor's own
  online check then reported the same file as clean.

## Documentation

- [docs/config.md](docs/config.md): configuration reference
- [docs/design/initial-decisions.md](docs/design/initial-decisions.md): design decisions, including what is not
  built yet
- [CONTRIBUTING.md](CONTRIBUTING.md): working on `branchpod`

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
