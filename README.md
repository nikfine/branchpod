# branchpod

> A resource runtime for git worktrees: spin up everything a branch needs, track it, and clean it up completely.

**Status: design stage — nothing to install yet.**

`branchpod` (`bp`) does not compete with worktree creators such as Orca, worktrunk or gtr. Whoever creates the
worktree calls `bp up <path>`; `bp` provisions the per-worktree environment and records every resource it
creates, so `bp down` and `bp gc` can release all of it — even when the worktree directory was deleted by
someone else.

## Goals

- **Cross-platform:** Windows, macOS, Linux.
- **Tool-agnostic:** services, containers, file sync and databases are plain commands in config —
  no built-in assumption about Docker, Vite, Mutagen or anything else.
- **Leave no trace:** a write-ahead ledger of created resources, reverse-order idempotent teardown,
  supervised process trees that die with their owner, and `gc` for leftovers.
- **No vendor lock-in:** the core never calls IDEs or orchestrators; integrations are plugins on top of a
  versioned `--json` / event contract.

See [docs/design/initial-decisions.md](docs/design/initial-decisions.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
