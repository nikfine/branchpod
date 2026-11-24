# Commands

The path is optional and defaults to the current directory.

| Command | What it does | Exit codes |
|---|---|---|
| `branchpod up [PATH]` | Creates what the config describes. Refuses if the pod already has anything recorded: run `down` first. | `0` everything was created, `1` failed |
| `branchpod status [PATH] [--json]` | Shows what is recorded for the pod. | `0`, also when there is no pod; `1` failed, for example outside a git worktree |
| `branchpod down [PATH]` | Tears down everything recorded, in reverse order. | `0` nothing is left, or there is no pod (see below); `3` something is left: fix the cause and run it again; `1` could not run |
| `branchpod logs SERVICE [PATH] [-f] [-n N]` | Prints the output of a service: the last 200 lines, `-n N` or `-n all`. `-f` follows it until the service is gone. | `0`; `1` no such pod, service or log, or failed |
| `branchpod config [PATH] [--json]` | Prints the effective configuration. Does not render or check variables. Creates nothing. | `0`; `1` no configuration, or it is invalid |

A usage error exits with `2`.

**`down` with code `0` is not proof that something was torn down.** It prints `clean` when it did, and
`no pod for ...` when it found none: the path is wrong, `BRANCHPOD_HOME` is another one, the worktree was moved, or
the pod is already gone.

**`down` keeps edited files.** A copied or rendered file that is not as `up` made it is shown as `saved` instead of
`destroyed`, and its new path is printed on stderr (`kept as ...`). That alone does not change the exit code.

When `up` fails before creating anything, there is nothing to clean up. When it fails midway, what was created
stays, and the message lists it with the `down` command to run.

One `up` or `down` runs at a time for a pod. A second one says whom it waits for, and waits without a limit.

## State

What was created, command output and service logs are kept outside the worktree, in the *state directory*:
`$BRANCHPOD_HOME` if it is set, otherwise

| System | State directory |
|---|---|
| Windows | `%LOCALAPPDATA%\branchpod\data` |
| macOS | `~/Library/Application Support/branchpod` |
| Linux | `$XDG_DATA_HOME/branchpod`, by default `~/.local/share/branchpod` |

Every command must see the same one. Arguments of commands and services are recorded there as they were run, so do
not pass secrets in them.

Edited files that `down` kept are in `saved/<pod id>/<time>/`, where `<time>` is in milliseconds since 1970, under
the path they had in the worktree. `down` never removes them: do it yourself once you do not need them, and mind that
they may hold secrets.
