# Configuration

The reference of `branchpod.toml` and `branchpod.local.toml`. A first example is in the
[getting started guide](getting-started.md).

- [Files and layering](#files-and-layering)
- [Example](#example)
- [Keys](#keys): [`version`](#version), [`[port.<name>]`](#portname), [`[[file]]`](#file),
  [`[[command]]`](#command), [`[[service]]`](#service)
- [Variables](#variables): [filters](#filters), [a literal `{{`](#a-literal-)
- [Order](#order)
- [Errors](#errors)

`branchpod config` prints the effective configuration of a worktree. Variables are neither rendered nor checked; `up`
checks them.

## Files and layering

| File | Read from | Meant to be |
|---|---|---|
| `branchpod.toml` | the root of the worktree | committed: every branch has its own version |
| `branchpod.local.toml` | the root of the main checkout | git-ignored: personal overrides |

A `branchpod.local.toml` inside a linked worktree is never read. Without a main checkout (a bare repository, a
separate git dir such as a submodule's) only `branchpod.toml` is read, `up` fails if there is a `[[file]]` rule other
than `ignore`, and `{{main}}` is not available. In the main checkout itself both files come from the same directory.

The local file is layered on top of the project file:

- tables are merged key by key: `[port.web]` in the local file changes that port and keeps the others;
- every other value is replaced as a whole. `[[file]]`, `[[command]]` and `[[service]]` are arrays: a local
  `[[file]]` replaces **all** file rules of the project file, and `file = []` removes them.

A key of a table cannot be removed, so a port of the project file can be changed but not dropped.

Either file alone is a valid configuration. The local file must then set `version` itself; layered on a project
file, it need not. With neither file, `up` and `config` fail and name both places. `status`, `down` and `logs` do
not read the configuration: `down` undoes what was recorded at `up`, whatever the files say by then.

Both files are UTF-8 (a byte order mark is accepted) and valid TOML.

## Example

A web application that runs in a container, next to a dev server:

```toml
version = 1

[port.web]
range = [8001, 8020]

[port.vite]
range = [5201, 5220]

[[file]]
path = ".env"
mode = "template"
replace = [{ find = "localhost:8000", with = "localhost:{{port.web}}" }]

[[file]]
path = "node_modules"
mode = "link"

[[file]]
path = ".vscode/settings.json"
mode = "copy"
optional = true

[[command]]
name = "app"
create = [
    "docker", "run", "-d",
    "--name", "example-{{pod.id}}",
    "--label", "bp.pod={{pod.id}}",
    "-p", "127.0.0.1:{{port.web}}:80",
    "-v", "{{worktree | slash}}:/app",
    "example/app",
]
destroy = ["docker", "rm", "-f", "-v", "example-{{pod.id}}"]

[[service]]
name = "vite"
run = ["node", "node_modules/vite/bin/vite.js", "--port", "{{port.vite}}", "--strictPort"]
env = { APP_URL = "http://localhost:{{port.web}}" }
```

Personal changes, in `branchpod.local.toml` of the main checkout:

```toml
[port.web]
range = [9001, 9020]
```

## Keys

| Section | Creates | `down` |
|---|---|---|
| `[port.<name>]` | a port from a range, as `{{port.<name>}}` | releases it |
| `[[file]]` | a file or directory from the main checkout: `copy`, `reflink`, `link`, `template` | removes it |
| `[[command]]` | whatever the `create` command creates | runs `destroy` |
| `[[service]]` | a long-running process | stops it, see [below](#service) |

Unknown keys are errors. Names of ports, commands and services match `[a-z][a-z0-9_-]*`, at most 64 characters.

### `version`

Required. The version of the format: `1`.

### `[port.<name>]`

| Key | Type | |
|---|---|---|
| `range` | `[first, last]` | Required. Inclusive, `0 < first <= last <= 65535`. |

The first free TCP port of the range, starting at a position derived from the worktree's path and the port's name,
so a worktree tends to get the same port every time. A port is free if no other pod holds it and it can be bound
now. Nothing keeps it bound: another program can still take it before your service does. On Windows a listener on
one specific address that is not loopback is not noticed.

Use it as `{{port.<name>}}`.

### `[[file]]`

A file or directory carried over from the main checkout into the worktree.

| Key | Type | Default | |
|---|---|---|---|
| `path` | string | required | Destination, relative to the root of the worktree. |
| `mode` | string | required | See below. |
| `source` | string | `path` | Source, relative to the main checkout. |
| `optional` | boolean | `false` | Skip the rule if the source does not exist, instead of failing. Not with `ignore`. |
| `vars` | boolean | `true` | `template` only: substitute `{{name}}` in the text. With `false`, `replace` is required. |
| `replace` | array of `{ find, with }` | `[]` | `template` only, see below. |

| `mode` | Source | Creates |
|---|---|---|
| `copy` | file or directory | A copy. |
| `reflink` | file or directory | A copy-on-write clone where the file system supports it, otherwise a copy. |
| `link` | file or directory | A link into the main checkout: a symlink, or a junction for a directory on Windows. A link to a file on Windows needs Developer Mode or administrator rights; without them it is an error. |
| `template` | file, UTF-8 | The text with variables substituted and `replace` applied. |
| `ignore` | none | Nothing. Meant for a future check of git-ignored files. `source` is not allowed. |

**Paths** are relative and stay inside: no absolute path, `..` or `:`. `/` and `\` both separate. They are not
templates: variables are not substituted. A destination may be listed once, and case does not make it another one.
Rules must not overlap (`a` and `a/b.txt`): whether `up` fails depends on their order and on the source.

**`replace`** rules run after the substitution, in order, each on the result of the one before. `find` is literal
text and must occur, otherwise it is an error: the source has changed. Every occurrence is replaced. Variables are
substituted in `with` (also with `vars = false`) but not in `find`.

**Nothing is overwritten.** An existing destination is an error, and nothing is created through a link. Missing
parent directories are created. In a copied directory, links are recreated as links. Have git ignore the destination,
or it shows up as untracked. A pattern with a trailing slash (`node_modules/`) matches directories only, not the
symlink that `link` makes on macOS and Linux: write `node_modules`.

With any rule other than `ignore`, `up` refuses to run in the main checkout, as every destination is its own source
there.

**Teardown** removes the destination if it is still a file, directory or link as created, and leaves it in place
(exit code 3) otherwise, as it does with what is in use on Windows. A link is removed as a link, never what it points
to. Created parent directories are removed if they are empty.

A copied or rendered **file** whose content has changed is not deleted but moved to
[`saved`](commands.md#state), and the path is printed. If it cannot be moved it stays (exit code 3): to give it up,
delete it. A copied **directory** is not compared and goes with all that was changed in it or put into it. Teardown
takes what **you** put at a recorded path, in a new worktree at the same place, for its own: it deletes a directory,
and a file with the same content. Run `down` before a path is used again.

### `[[command]]`

Anything created by one command and removed by another: a container, a volume, a sync session.

| Key | Type | Default | |
|---|---|---|---|
| `name` | string | required | Unique among commands. |
| `create` | array of strings | required | Program and arguments. |
| `destroy` | array of strings | none | Program and arguments. Without it, there is nothing to undo. |
| `output` | `"stdout"` or `"file"` | `"stdout"` | What becomes of the output of `create`, see below. |
| `gc` | array of strings | none | Accepted but not used yet, and its variables are not checked. |

**How they run.** Without a shell: no pipes, `&&` or `$VAR`. Standard input is closed, the environment is that of
`up` or `down`. An exit code other than 0 is a failure, and the end of stderr is shown. There is no time limit.

- `create` runs in the root of the worktree. `destroy` too while it exists, otherwise in the
  [state directory](commands.md#state): have it use absolute paths, such as `{{main}}`.
- A bare program name is searched on `PATH`. A relative one with a directory (`./scripts/up.sh`) is taken from the
  working directory. On Windows a `.cmd` or `.bat` script is named with its extension (`npx.cmd`), which other
  systems do not have: call the program behind it if the config is to work everywhere.
- On Windows, a process that a command leaves running keeps the output of `up` or `down` open
  ([#82](https://github.com/nikfine/branchpod/issues/82)).

**`output = "stdout"`** is UTF-8 text without surrounding whitespace, available to later commands and services as
`{{command.<name>.stdout}}`. If `create` prints nothing, there is no such variable and a reference to it fails. The
text is recorded and shown by `up` and `status --json`.

**`output = "file"`** keeps the output byte for byte in a file, whose absolute path is `{{command.<name>.file}}`;
`{{command.<name>.stdout}}` is then not available. Use it for output that may hold secrets, such as an env file: it
is in that file only. On Unix only its owner can read it; on Windows it inherits the permissions of the state
directory. Blank output is an error. The file is removed by a clean `down`. The command must not have the name of a
Windows device (`nul`, `con`, `com1`, ...).

```toml
[[command]]
name = "env"
create = ["docker", "inspect", "--format", "{{{{range .Config.Env}}{{{{println .}}{{{{end}}", "example-base"]
output = "file"

[[command]]
name = "app"
create = ["docker", "run", "-d", "--name", "example-{{pod.id}}", "--env-file", "{{command.env.file}}", "example/app"]
destroy = ["docker", "rm", "-f", "-v", "example-{{pod.id}}"]
```

**`destroy`** is rendered at `up` and recorded, so changing the config later does not change it. It may use the
built-in variables, ports, the output of earlier commands, and its own output. `create` cannot use its own.

- Every value in it must be a single line without a NUL character, also before filters: a warning printed next to
  an id must not become part of it. Otherwise `up` fails.
- If it uses its own `stdout`, `create` must print exactly one line. If `create` fails or is interrupted, the output
  is unknown: `destroy` cannot run and `down` exits with 3. A name that is known beforehand, such as
  `example-{{pod.id}}`, or `output = "file"` does not have this problem.
- It also runs when `create` failed, since the resource may exist in part. It should succeed if there is nothing to
  remove; otherwise `down` keeps exiting with 3. With `output = "file"` the file may then be missing.

### `[[service]]`

A long-running process. All services of a pod run under one supervisor process, which `up` starts.

| Key | Type | Default | |
|---|---|---|---|
| `name` | string | required | Unique among services. |
| `run` | array of strings | required | Program and arguments, found and run as those of a command. |
| `cwd` | string | the root | Working directory, relative to the root of the worktree. Same rules as a file path: not a template, stays inside. It must exist when the service starts. Use `/`. |
| `env` | table of strings | `{}` | Added to the environment of `up`. Variables are substituted in the values. Not recorded. |

`up` returns once the process exists, not once it is ready. A service that exits is not restarted; `status`
shows it. Standard input is closed; stdout and stderr go to one log, shown by `branchpod logs <name>`, which keeps about 30 MB.

`down` stops a service with what it started. On Windows that is the whole process tree, at once. On macOS and Linux
it is the process group, with `SIGTERM`, then `SIGKILL` after 10 seconds: a process that left the group (`setsid`)
survives.

## Variables

| Variable | Value |
|---|---|
| `{{pod.id}}` | Identifier of the pod: `<directory name>-<hash of the path>`, only `a-z`, `0-9` and `-`. |
| `{{worktree}}` | Absolute path of the worktree. |
| `{{main}}` | Absolute path of the main checkout. |
| `{{branch}}` | Name of the branch checked out at `up`. |
| `{{port.<name>}}` | The port. |
| `{{command.<name>.stdout}}` | Output of an earlier command. |
| `{{command.<name>.file}}` | File with the output of an earlier command that has `output = "file"`. |

Spaces inside the braces are allowed. A reference to an unknown name is an error, never an empty string, and so is
one to a name without a value: `branch` on a detached HEAD, `main` without a main checkout, a path that is not
UTF-8. A value is not substituted again.

| Where | Available |
|---|---|
| template text, `replace.with` | built-in variables, ports |
| `command.create` | and the output of earlier commands |
| `command.destroy` | and its own output |
| `service.run`, values of `service.env` | built-in variables, ports, the output of all commands |

Nowhere else: not in names, paths, `cwd`, `find` or the keys of `env`.

### Filters

`{{name | filter}}`, applied from left to right:

| Filter | Result |
|---|---|
| `json` | Escaped for use **inside** a JSON string. It adds no quotes: `"{{worktree \| json}}/src"`. |
| `slash` | A Windows path with `/`: `C:/work/feature`. Changes nothing on macOS and Linux. A path that starts with `\\?\` is an error, except on a network share (`\\?\UNC\server\share` becomes `//server/share`). |

Combined, `slash` comes first: `{{worktree | slash | json}}`. An unknown filter is an error.

### A literal `{{`

Write `{{{{` for a literal `{{`, in arguments and templates. `}}` outside a reference needs no escape.

| Written | Rendered |
|---|---|
| `{{{{.Config.Image}}` | `{{.Config.Image}}` |
| `{{{{{{port.web}}}}` | `{{8001}}` |
| `{"a":{"b":{}}}` | unchanged |

For a file full of `{{ }}` (Vue, Blade), set `vars = false` and use `replace`.

## Order

`up` creates ports, files, commands and services, in this order; within a kind in the order of the config, ports by
name. So a template can use a port but not the output of a command, and a command can use a file.

Before anything is created, the whole config is checked with stand-in values for ports and command output. What only
shows during creation fails midway: a command that fails, a program that is not found, a missing `cwd`. Then nothing
is rolled back; what was created is recorded, and `down` removes it.

`down` stops services, then the supervisor, removes linked files, and then the rest in reverse order. A failed step
does not stop the others.

## Errors

| Message contains | Cause |
|---|---|
| `no configuration: neither ... nor ... exists` | There is no `branchpod.toml` in the worktree and no `branchpod.local.toml` in the main checkout. |
| `is the whole configuration here ... so it must set` | There is only a local file, and it has no `version`. |
| `no branchpod.toml in` | No main checkout, so only the project file counts, and it is missing. |
| `did not contain valid UTF-8` | The configuration file is not UTF-8 (`>` in PowerShell 5.1 writes UTF-16). |
| `unsupported config version` | `version` is not `1`. |
| `unknown field` | A misspelt key, or one that this build does not know. |
| `path must be relative`, `path must not contain` | A path that could leave the worktree or the main checkout. |
| `is listed twice`, `is defined twice` | Two rules for one destination, or two commands or services with one name. |
| `source ... does not exist` | Missing in the main checkout. Set `optional = true` if it may be. |
| `already exists; bp does not overwrite` | The destination is there: committed to git, or created by another tool. |
| `there is no main checkout` | A file rule or `{{main}}` in a bare repository or with a separate git dir. |
| `is the main checkout` | `up` ran there with file rules. Run it in a linked worktree. |
| `is not UTF-8 text` | The source of a template is not UTF-8. |
| `replace rule ... matches nothing` | The source no longer contains the text of `find`. |
| `unknown variable` | Misspelt, or not available in this place. With a hint at `{{{{` it is probably another tool's template. |
| `variable ... is not available` | A known name without a value here; the message says why. |
| `unknown filter`, `empty filter` | Only `json` and `slash` exist. |
| `unterminated` | `{{` without `}}`. |
| `spans several lines or contains a NUL character` | A value used in `destroy` that is more than an id. |
| `printed nothing`, `printed several lines` | Output that `destroy` needs, or that goes to a file, cannot be used. |
| `output is not valid UTF-8` | Output of a command without `output = "file"`. |
| `no free port` | Every port of the range is taken. |
| `program not found` | Not on `PATH`, or a Windows script without its extension. |
| `already has resources recorded` | The pod is up, or an earlier `up` failed: run `down` first. If the worktree is a new one at a path used before, `down` takes what is at the recorded paths for its own, even if it is yours: it deletes directories and moves changed files to `saved`. Move them away first. |
| `looks like an unexpanded variable` | The caller passed `%NAME%` or `$NAME` as the path. |

All of them exit with code 1. The other codes are in the [command reference](commands.md).
