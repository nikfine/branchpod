# Contributing

Working rules (branches, pull requests, principles) are in [AGENTS.md](AGENTS.md), for people and coding
agents alike. This file describes how `branchpod` is used to develop `branchpod` itself.

## One worktree per session

Every session, human or agent, works in its own git worktree. `branchpod up` carries the git-ignored
personal files over into it, and `branchpod down` removes what it created.

The calls are plain commands around the session, so they work the same for any agent or IDE. Hooks of a
particular tool are not used. Claude Code, for example, has a `WorktreeCreate` hook, but it is there
["to replace the default `git worktree` logic entirely"](https://code.claude.com/docs/en/worktrees).

### Setup, once per machine

Install a binary built from `main`. Sessions always use that one, never the build of the branch they work
on, so a broken branch cannot break its own environment. Build it from an up-to-date, clean checkout of
`main`:

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

[[file]]
path = ".claude/settings.local.json"
mode = "copy"
optional = true
```

List the personal files of your own agents, and only git-ignored ones: an untracked file that is not ignored
makes `git worktree remove` refuse. `optional = true` skips a file you do not have. Do not carry over
`target/`: where the file system cannot clone files, copying it costs more than building from scratch.

Why `copy`: on Windows, linking a single file needs Developer Mode or administrator rights. On macOS and
Linux `mode = "link"` suits a file edited in place, such as `CLAUDE.local.md`: edits then show up in every
worktree. A tool that saves by replacing the file turns the link into a file, which `down` leaves in place.

**Edits to a copy are lost:** `branchpod down` deletes the copied file even if it has changed since, without
a warning. Make lasting changes, such as permissions an agent should keep, in the main checkout.

### Around a session

Run these from the main checkout, one at a time, and stop at the first one that fails. The worktree goes
outside the repository; a sibling directory is used here as an example.

Before the session:

```
git fetch origin
git worktree add -b 123-short-slug ../branchpod-worktrees/123-short-slug origin/main
branchpod up ../branchpod-worktrees/123-short-slug
```

After the session:

```
branchpod down ../branchpod-worktrees/123-short-slug
git worktree remove ../branchpod-worktrees/123-short-slug
```

- **Run `up` before starting the agent.** `branchpod` never overwrites a file it did not create, so `up`
  fails if the agent has already written its settings there. For the same reason, do not let another tool
  copy these files too (for example through a `.worktreeinclude` file).
- **Close the session before `down`.** On Windows a process still working inside the worktree makes
  `git worktree remove` fail halfway: the files are deleted and the worktree is unregistered, but an empty
  directory stays behind.
- **Continue only after exit code 0 from `down`.** Code 3 means that something is left behind: the message
  says what and why. Fix the cause and run `down` again.
- **`git worktree remove` deletes ignored files without asking**, and refuses if there are uncommitted changes
  or untracked files. Look at those before reaching for `--force`.
- A worktree that was removed without `down` still has a record: run `branchpod down` with its old path.

### Running the branch you work on

A development build uses the same state as the installed binary, and a worktree has one pod whichever build
calls it. `cargo run -- down` in your worktree would tear down the session's own environment, so point
development builds at a scratch state directory:

```
# bash
BRANCHPOD_HOME="$(mktemp -d)" cargo run -- status

# PowerShell
$env:BRANCHPOD_HOME = Join-Path $env:TEMP "branchpod-dev"; cargo run -- status
```
