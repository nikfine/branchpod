# Agent instructions

Guidance for AI coding agents (and humans) working on branchpod.

## Source of truth

- Design decisions and scope: [docs/design/initial-decisions.md](docs/design/initial-decisions.md).
  Do not reopen a recorded decision silently — propose a change to that file instead.
- Work tracking lives in GitHub issues and milestones, not in files in the repository.

## Branches and pull requests

- One branch per issue, named `<issue number>-<short-slug>` (e.g. `7-bp-down`), created from an
  up-to-date `main`. Small maintenance without an issue: `chore-<slug>`.
- Changes reach `main` through pull requests; the PR body links the issue (`Closes #7`). CI runs on
  pull requests.
- MSRV: raise `rust-version` freely when a newer language or library feature is needed, in the same PR
  as the toolchain of the `msrv` CI job. Older Rust versions are not supported; the field and the job
  only guarantee that the declared version is true.

## Principles

- **Clean only what we created.** Every resource `bp` creates is recorded in the ledger and labelled;
  teardown and `gc` never touch anything else.
- **No vendor lock-in.** The core never calls IDEs, orchestrators or container runtimes by name. Concrete
  tools are configured commands; integrations are plugins on top of the versioned `--json` / event contract.
- **Cross-platform by default.** Windows, macOS and Linux are first-class. Any OS-specific code sits behind
  a small interface with an implementation per platform.
- **No shell in between.** Spawn processes with an argument array; never build command lines by string
  concatenation.
- The `--json` output and the event schema are public API: version them and treat breaking changes as such.

## Repository hygiene

This is a public repository. Before committing, make sure nothing private slips in:

- no local absolute paths, user names, hostnames or e-mail addresses;
- no details of other (private) projects;
- no raw output captured from a real machine (process lists, `docker` output, logs) — reduce it to a
  synthetic example.

Personal, machine-specific agent settings belong in `CLAUDE.local.md` / `.claude/settings.local.json`
(both git-ignored), not in this file.

## Language

Code, comments, commit messages and documentation are in English.
