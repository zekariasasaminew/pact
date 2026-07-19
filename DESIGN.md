# pact — design notes

This document holds the *why* behind non-obvious decisions in pact's source:
empirical findings from manual testing, trial-report-driven fixes, tradeoffs
considered and rejected, and anything confirmed by hand rather than just
reasoned about. It exists so the code itself can stay comment-light — naming
and structure carry the *what*, this document carries the *why* and the
history. See `CLAUDE.md` for the convention this follows going forward.

Organized by crate, roughly in dependency order (`pact-vcs`/`pact-agents`/
`pact-coord`/`pact-deps` first, since `pact-core` and `pact-cli` build on
them).

## pact-vcs — git worktree lifecycle, merge-all

### Workspace lifecycle

### Teardown edge cases

### commit_all

### merge_all

### Semantic auto-resolution

### Arbiter resolver hook

## pact-core — Orchestrator

### spawn / spawn_many concurrency

### Coordination config wiring

### Weaver — task overlap prediction

### Arbiter — agent invocation

## pact-agents — adapters and process supervision

### Process group kill

### Adapter-specific quirks

## pact-coord — MCP coordination server

### Lease system

## pact-deps — dependency materialization

### Windows `.cmd` shim resolution

`cmdutil::run` routes every spawned package-manager command through `cmd /C`
on Windows. npm/pnpm/yarn (and sometimes poetry/pipenv, depending on install
method) ship as `.cmd` shims, not `.exe`. `std::process::Command` doesn't
consult `PATHEXT` the way a real shell does, so `Command::new("npm")` fails
with "program not found" even though `npm` works fine typed interactively.
`cmd /C` restores that resolution; other platforms get a plain, direct spawn.

### Package manager detection

## pact-cli — command-line surface
