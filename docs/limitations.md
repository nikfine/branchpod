# Limitations

## Tearing down

- **Run `down` before a path is used again**, also for a worktree that was removed without it. Otherwise `up` of the
  new worktree refuses with `already has resources recorded`, and the `down` it asks for takes what is at the
  recorded paths for its own, even if it is yours: it deletes directories, and moves files with other contents to
  `saved`, see [State](commands.md#state). Move them away first.
- **Run `down` before `git worktree move`.** A pod is found by the recorded path: at the new path there is `no pod`,
  and files that moved along are not removed.
- **Only `clean` shows that a pod was torn down.** `down` also exits with `0` when it finds no pod, see
  [Commands](commands.md).
- **`down` moves an edited copy to `saved`** in the [state directory](commands.md#state) and prints the path. Nothing
  brings it back to the main checkout or cleans `saved` up. Keep lasting changes in the main checkout. A file that
  was cloned copy-on-write (`reflink`) is compared by size and modification time only.
- **Directories.** A copied directory is removed with everything that was changed in it or put into it later: list a
  file that matters in a rule of its own. A linked one *is* the
  directory of the main checkout: what is written there shows up in every worktree. On Windows a copied directory or
  file that is in use cannot be removed: `down` leaves it, exits with `3` and can be run again.
- **Only what `destroy` can undo is undone.** If it fails, or needs output that was never captured, `down` keeps
  exiting with `3`, and `up` refuses at that path: for now the way out is a worktree at another path. There is no
  command yet that cleans up or forgets such a resource. Messages that mention `bp gc` refer to one that does not
  exist yet.

## Services and commands

- **`up` does not wait until a service is ready**, only until its process exists. A service that exits later is not
  restarted until the next `up`; `status` shows that, with its exit code. It does not see what such a service left running, and
  `running` does not mean that it is ready.
- **`status` shows the end of the log of a service that does not run**, secrets that it printed included, also with
  `--json`.
- **macOS and Linux: a service can outlive its supervisor.** If the supervisor is killed, services are not
  ([#15](https://github.com/nikfine/branchpod/issues/15),
  [#16](https://github.com/nikfine/branchpod/issues/16)); `down` stops them. A killed process may also still hold
  its files and sockets, such as a port, for a moment when it already counts as `gone`
  ([#84](https://github.com/nikfine/branchpod/issues/84)).
- **macOS and Linux: `down` stops the process group of a service.** A process that left it (`setsid`) survives.
- **Windows: `up` refuses to start services** when the program that called it would take them down with itself
  (a Job Object that kills its processes on close). `cargo run` is such a caller.
- **Windows: a daemon that a command leaves running keeps whoever waits for the end of the output of `up` or
  `down` waiting** ([#82](https://github.com/nikfine/branchpod/issues/82)).

- **`up` does not know whether a container runs.** It only runs the `start` of its command, and `status` does not look
  at commands at all.
- **`up` on a pod that is there keeps its ports.** If another program has taken one meanwhile, whoever needs it fails,
  or uses another one if it may. `down` and `up` choose a free one.

## Versions and systems

- **Any change of the config takes `down` and `up`** for a pod that is there, also one of `branchpod.local.toml`, which
  all worktrees share.
- **A pod of version 0.1.0 is not brought up again**: run `down` and `up` once.
- **Windows 10 has not been tested**, although it is meant to work. CI runs on a Windows Server image, macOS on
  Apple silicon and Ubuntu.
- **Run `down` for every pod before an upgrade or a downgrade.** State written by a newer format, and a supervisor
  that speaks another version of the internal protocol, are refused. On Windows the executable is in use while a pod
  with services is up, so replacing it may fail.
- **A bare repository or a separate git dir** (a submodule's) has no main checkout: file rules other than `ignore`
  and `{{main}}` do not work there.
- **The Windows binary is not signed**: SmartScreen or an antivirus may object, see
  [The Windows binary](windows.md). A build of your own can be flagged, too: it happened to one of ours, and the
  vendor's online check then reported the same file as clean.
