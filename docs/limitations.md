# Limitations

## Tearing down

- **Run `down` before a path is used again**, also for a worktree that was removed without it. Otherwise `up` of the
  new worktree refuses with `already has resources recorded`, and the `down` it asks for deletes the files at the
  recorded paths, even if they are not the ones `branchpod` made. Move them away first.
- **Run `down` before `git worktree move`.** A pod is found by the recorded path: at the new path there is `no pod`,
  and files that moved along are not removed.
- **Only `clean` shows that a pod was torn down.** `down` also exits with `0` when it finds no pod, see
  [Commands](commands.md).
- **`down` deletes a copied or rendered file even if it was edited since**, without a warning
  ([#67](https://github.com/nikfine/branchpod/issues/67)). Keep lasting changes in the main checkout.
- **Directories.** A copied directory is removed with everything that was put into it later. A linked one *is* the
  directory of the main checkout: what is written there shows up in every worktree. On Windows a copied directory or
  file that is in use cannot be removed: `down` leaves it, exits with `3` and can be run again.
- **Only what `destroy` can undo is undone.** If it fails, or needs output that was never captured, `down` keeps
  exiting with `3`, and `up` refuses at that path: for now the way out is a worktree at another path. There is no
  command yet that cleans up or forgets such a resource. Messages that mention `bp gc` refer to one that does not
  exist yet.

## Services and commands

- **`up` does not wait until a service is ready**, only until its process exists. A service that exits later is not
  restarted, and `status` still shows it as `live`: it shows what was recorded. `logs` shows what it printed, if
  anything; its exit code is not shown.
- **macOS and Linux: a service can outlive its supervisor.** If the supervisor is killed, services are not
  ([#15](https://github.com/nikfine/branchpod/issues/15),
  [#16](https://github.com/nikfine/branchpod/issues/16)); `down` stops them. A killed process may also still hold
  its files and sockets for a moment when it already counts as gone
  ([#84](https://github.com/nikfine/branchpod/issues/84)).
- **macOS and Linux: `down` stops the process group of a service.** A process that left it (`setsid`) survives.
- **Windows: `up` refuses to start services** when the program that called it would take them down with itself
  (a Job Object that kills its processes on close). `cargo run` is such a caller.
- **Windows: a daemon that a command leaves running keeps whoever waits for the end of the output of `up` or
  `down` waiting** ([#82](https://github.com/nikfine/branchpod/issues/82)).

## Versions and systems

- **Windows 10 has not been tested**, although it is meant to work. CI runs on a Windows Server image, macOS on
  Apple silicon and Ubuntu.
- **Run `down` for every pod before an upgrade or a downgrade.** State written by a newer format, and a supervisor
  that speaks another version of the internal protocol, are refused. On Windows the executable is in use while a pod
  with services is up, so replacing it may fail.
- **A bare repository or a separate git dir** (a submodule's) has no main checkout: file rules other than `ignore`
  and `{{main}}` do not work there.
- **Heuristic antivirus scanners may flag a local build on Windows.** It happened to one of ours; the vendor's own
  online check then reported the same file as clean.
