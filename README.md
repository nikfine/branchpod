# branchpod

> A resource runtime for git worktrees: spin up everything a branch needs, track it, and clean it up completely.

A fresh git worktree has the committed files and nothing else. `branchpod up` gives it ports, git-ignored files,
containers and dev servers from a config file; `branchpod down` removes exactly what was created, also when the
worktree is already gone. Any tool can create the worktree and call them. For Windows, macOS and Linux.

## Installation

There are no prebuilt binaries: build it with Rust 1.89 or newer and a linker (Build Tools for Visual Studio on
Windows, `cc` elsewhere). At run time it needs `git` 2.31 or newer on `PATH`.

```
cargo install --locked --git https://github.com/nikfine/branchpod
```

## Usage

`branchpod.toml`: a port, a git-ignored file from the main checkout and a service. To try it without a project of
your own, follow [Getting started](docs/getting-started.md).

```toml
version = 1
port.web.range = [8001, 8020]
file = [{ path = ".env", mode = "copy", optional = true }]
# Replace with the command of your dev server.
service = [{ name = "web", run = ["your-dev-server", "--port", "{{port.web}}"] }]
```

- `branchpod up ../feature` creates everything the config describes.
- `branchpod status ../feature` shows what is recorded.
- `branchpod logs web ../feature` prints the output of a service.
- `branchpod down ../feature` removes everything that was created.

## Before you rely on it

- This is an early version (0.x): the config format, the state on disk and the output may still change.
- Run `down` before a worktree is removed, moved or its path is used again, and look for `clean` in its output.
- `down` deletes a copied or rendered file even if it was edited since, without a warning.
- `up` does not wait until a service is ready. All [limitations](docs/limitations.md).

## Documentation

- [Commands](docs/commands.md) with exit codes, [configuration](docs/config.md)
- [Design decisions](docs/design/initial-decisions.md), including what is not built yet; [contributing](CONTRIBUTING.md)
<!-- The skill for agents goes here (#55). -->

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
