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

### Passthrough caching strategy

`passthrough::run` warms the package manager's own global cache instead of
building pact-specific sharing, for ecosystems that already cache well:
pnpm, yarn, uv, poetry, pipenv, Cargo, and Go modules all cache once and
reuse across projects by default, so the only job here is warming that
cache before the agent's first real command. Maven and Gradle need no
command at all -- `~/.m2` and `~/.gradle/caches` populate lazily on any
build invocation, so an explicit fetch step would only add time. A
non-zero exit is logged, not returned as an error: a transient network
failure here shouldn't fail the whole `spawn`, since the agent can still
retry the install itself once it starts working.

Plain pip/venv gets no custom store (a Phase 1 decision): pip already has
its own global download cache (`~/.cache/pip`) shared across projects,
covering the expensive part (network fetch). A hardlink-based store on top
of that would mean hardlinking into freshly created venvs, which risks
embedding absolute paths from the wrong venv (activation scripts, `.pth`
files, console script shebangs) -- a correctness risk, not just extra
engineering, so it's left as future work rather than shipped provisionally.

### ReadOnlyHardlink tradeoff

A hardlink shares the same underlying file record as its content-store
entry, so marking the destination read-only also freezes the canonical
store copy after first use -- intentional, not a side effect to work
around. The tradeoff: a package that writes into its own installed files
after materialization (a native-build step, a binary downloader, a
git-hook installer) fails loudly instead of silently corrupting every
other workspace sharing that store entry. That failure is the point.

### Package manager detection

## pact-cli — command-line surface
