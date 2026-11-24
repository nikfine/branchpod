# Contributing

Working rules (branches, pull requests, principles) are in [AGENTS.md](AGENTS.md), for people and coding
agents alike. This file describes how `branchpod` is used to develop `branchpod` itself.

## One worktree per session

Every session, human or agent, works in its own git worktree. `branchpod up` carries the git-ignored
personal files over into it, and `branchpod down` removes what it created.

Whoever opens the session runs the commands below: `up` comes before an agent starts and `down` after it is
closed, so an agent does not run them for its own session. They are plain commands rather than hooks of a
particular tool, so they are the same for every agent and IDE.

### Setup, once per machine

Install a binary built from `main`. Sessions always use that one, never the build of the branch they work
on, so a broken branch cannot break its own environment. Run this in an up-to-date, clean checkout of
`main`, and again whenever you want a newer version:

```
cargo install --locked --path .
```

The command is `branchpod`. Messages call it `bp`, which is only an optional alias.

Then create `branchpod.local.toml` in the **main checkout**. It is git-ignored, and it is read from there
only: a copy inside a worktree has no effect.

```toml
version = 1

[[file]]
path = "CLAUDE.local.md"
mode = "copy"
optional = true

# Windows only, see below.
[[file]]
path = ".claude/settings.local.json"
mode = "copy"
optional = true
```

List the personal files of your own agents. `optional = true` skips a file you do not have. Only list
git-ignored files: any other would show up as untracked in the worktree, easy to commit by accident. Do not
carry over `target/`: where the file system cannot clone files, copying it costs more than building from
scratch.

**`.claude/settings.local.json` is for Windows only.** In a worktree Claude Code uses
[the file at the main checkout's root](https://code.claude.com/docs/en/settings), except on Windows and in a
few other cases, where [a saved permission stays with that worktree](https://code.claude.com/docs/en/worktrees).
So leave this rule out on macOS and Linux: the copy is not needed there, and Claude Code may apply its
permissions on top of those of the main checkout.

**Why `copy`:** on Windows, linking a single file needs Developer Mode or administrator rights. On macOS and
Linux `mode = "link"` suits a file edited in place, such as `CLAUDE.local.md`: edits then show up in every
worktree. A tool that saves by replacing the file turns the link into a regular file, and `down` then exits
with code 3 and leaves it: move the edits to the main checkout, delete the file and run `down` again.

**Edits to a copy are lost:** `down` deletes the copied file even if it has changed since, without a warning.
Make lasting changes, such as permissions an agent should keep, in the main checkout.

### Around a session

Run these from the main checkout, one at a time, and stop at the first one that fails. The worktree goes
outside the repository; a sibling directory is used here as an example.

Before the session:

```
git fetch origin
git worktree add --no-track -b 123-short-slug ../branchpod-worktrees/123-short-slug origin/main
branchpod up ../branchpod-worktrees/123-short-slug
```

`--no-track` keeps `origin/main` from becoming the upstream of the new branch; push it the first time with
`git push -u origin HEAD`. For a branch that already exists, use `git worktree add <path> <branch>`.

After the session:

```
branchpod down ../branchpod-worktrees/123-short-slug
git worktree remove ../branchpod-worktrees/123-short-slug
```

- **Run `up` before starting the agent.** `branchpod` never overwrites a file it did not create, so `up`
  fails if a file it has to carry over is already there, for example because the agent saved its settings.
  For the same reason, do not let another tool copy these files too (such as a `.worktreeinclude` file).
- **Close the session before `down`.** On Windows a process still working inside the worktree makes
  `git worktree remove` fail halfway: the files are deleted and the worktree is unregistered, but an empty
  directory stays behind. Close the process and delete that directory.
- **Go on only after `down` exits with 0 and prints `clean`.** Code 3 means that something is left behind: the
  message says what and why. Fix the cause and run `down` again. `no pod for ...` also exits with 0, but
  nothing was torn down: the path is wrong, `BRANCHPOD_HOME` points elsewhere, or the pod is already gone.
- **Run `down` before a path is used again**, also for a worktree that was removed without it. Otherwise `up`
  of the new worktree refuses with `already has resources recorded`, and `down` would delete the files
  recorded for the old one, even if they are not the ones `branchpod` made.
- **`git worktree remove` deletes ignored files without asking**, and refuses if there are uncommitted changes
  or untracked files. Look at those before reaching for `--force`.
- The local branch stays. After a squash merge git does not see it as merged: delete it with `git branch -D`.

### Trying the build you work on

Do not point a development build at your own session: `cargo run -- down` there would tear down the session's
environment, and `up` would find its files already in place. `BRANCHPOD_HOME` separates only the state, not
the files of a worktree. Use a throwaway repository and a state directory of your own, one per session, so
that a changed state format cannot get in the way of another session. In the session's worktree, in a
**separate terminal** (the variable stays set in it):

```
# bash
trial="$PWD.trial"; export BRANCHPOD_HOME="$trial/home"

# PowerShell
$trial = "$PWD.trial"; $env:BRANCHPOD_HOME = "$trial/home"
```

Then, in either shell:

```
git init "$trial/main"
git -C "$trial/main" commit --allow-empty -m init
git -C "$trial/main" worktree add "$trial/wt"
```

Create `branchpod.local.toml` in `main` of that repository with what you want to try. Build, and run the
binary itself:

```
cargo build
target/debug/branchpod up "$trial/wt"
target/debug/branchpod status "$trial/wt"
target/debug/branchpod down "$trial/wt"
```

Not `cargo run -- up`: on Windows Cargo runs programs in a Job Object that kills them when Cargo itself is
killed, and `up` refuses to start services there. After a clean `down`, delete the trial directory and close
the terminal.
