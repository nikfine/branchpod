# Commands

The path is the root of a worktree. Without one, the worktree around the current directory is meant. `status`, `down`
and `logs` refuse a directory inside a worktree that has a pod, with code `1`.

| Command | What it does | Exit codes |
|---|---|---|
| `branchpod up [PATH]` | Creates what the config describes. A pod that is there already is brought up again, see [Up again](#up-again). | `0` the pod is up, `1` failed or refused |
| `branchpod status [PATH] [--json]` | Shows what is recorded for the pod and whether it still runs, see [Status](#status). Only looks. | `0` healthy, or there is no pod; `4` degraded; `5` busy; `1` failed, for example without a path outside a git worktree |
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

## Up again

After a reboot, or when a service has died, `up` starts what does not run any more. It destroys nothing, creates
nothing anew and does not touch services that run. It prints `is up again`, or `is already up` if no service had to be
started.

- Services are started again.
- The [`start`](config.md#command) of each command is run every time. A command that has a `destroy` but no `start` is
  `left as it is`.
- Ports stay as they are and are not checked. Files stay as they are, and deleted ones are not brought back.

It refuses, with `already has resources recorded` and the reason, if the config has changed since (other than in
comments), if an earlier `up` or `down` did not finish, or if this is not the worktree the pod was created for but a new one at
the same path. Then run `down` first.

## Status

`status` ends with one of these, and with a hint at what to do unless it is `healthy`:

| | Exit code | Meaning |
|---|---|---|
| `healthy` | `0` | Everything is as `up` left it. Not that a service is ready: that is not checked. |
| `degraded` | `4` | A process does not run as it should, something failed, or `up` or `down` did not finish. The hint says what to do: `up`, or `down` and then `up`. |
| `busy` | `5` | `up` or `down` is at work. Wait: do not take the pod down. |

For a service that does not run, the last 10 lines of its log are shown (at most 4 KB), whatever they are: a service
may have printed secrets, and nothing is masked. The supervisor gets 3 seconds to answer.

`--json` prints one document:

| Field | |
|---|---|
| `schema` | `1` |
| `pod` | Id and paths; `null` when there is no pod. `unfinished` is `up` or `down` when that command has not finished: it failed, was interrupted, or is at work. |
| `resources[]` | `kind`, `name`, and from the record `status`, `data`, `error`. |
| `resources[].runtime` | Only of the supervisor and the services. `state`, see below; `exit` (`{"code": 1}` or `{"signal": 9}`) and `exited_at_ms` (since 1970, UTC) of a service that exited; `error`; `log` of a service that does not run. |
| `resources[].runtime.log` | `path`; `tail`, its last lines; `truncated`, there are more; `lossy`, some bytes were not UTF-8 and were replaced; `error`. |
| `health` | `state` as above, `problems[]` (what is not as it should be; may be empty when `pod.unfinished` is all), `hint`. `null` when there is no pod. |

| `runtime.state` | |
|---|---|
| `running` | Its process runs. |
| `unresponsive` | The supervisor runs but does not answer. |
| `exited` | The service exited. An older supervisor does not tell `exit`. |
| `orphaned` | The service runs without its supervisor. |
| `gone` | It does not run, and how it ended is not known. |
| `not_started` | No process was started, or that was not recorded. |
| `unknown` | The record cannot be read. |

## State

What was created, command output and service logs are kept outside the worktree, in the *state directory*:
`$BRANCHPOD_HOME` if it is set, otherwise

| System | State directory |
|---|---|
| Windows | `%LOCALAPPDATA%\branchpod\data` |
| macOS | `~/Library/Application Support/branchpod` |
| Linux | `$XDG_DATA_HOME/branchpod`, by default `~/.local/share/branchpod` |

Besides, `up` writes one small file, `.branchpod`, into the git directory of the worktree (`.git/worktrees/<name>`), to
tell the worktree from a later one at the same path. `down` removes it.

Every command must see the same state directory. Arguments of commands and services are recorded there as they were run, so do
not pass secrets in them.

Edited files that `down` kept are in `saved/<pod id>/<time>/`, where `<time>` is in milliseconds since 1970, under
the path they had in the worktree. `down` never removes them: do it yourself once you do not need them, and mind that
they may hold secrets.
