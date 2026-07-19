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

### AgentEvent normalization

`AgentEvent` is shared across every adapter (Claude Code, Copilot CLI,
Codex, Gemini), even though each CLI's actual output schema is different --
each adapter's own `parse_line` maps its specific shape onto this enum.
`Other` is a catch-all for anything not explicitly modeled, but it's still
surfaced to callers, never silently dropped: an unrecognized event is far
more likely to be a real message an adapter hasn't been taught about yet
than something safe to ignore.

`CoordStatus` is a separate variant, not bundled into `Init`, because Claude
Code reports every MCP server's status inside its one init event, but
Copilot CLI reports them as their own standalone events, and a line can
report several servers at once. Each adapter's `parse_line` emits zero or
more `CoordStatus` events per line as its own schema demands; the
connectivity check that consumes them (`pact-core`) doesn't need to know
which shape produced them.

### Process group kill

`Supervisor` (below) covers the Ctrl-C path; `kill_if_alive` in `pact-vcs`
(used by `teardown`) covers killing a specific workspace's agent process on
demand -- both need to reach an agent's *whole* process tree, not just the
directly-spawned PID, since a Bash tool call spawns a child shell that a
plain `Child::kill()` leaves running. On Windows, `taskkill /F /T /PID`
terminates the full descendant tree in one call. On Unix,
`pact_agents::run_and_stream` spawns every agent process via
`command_group`'s `group_spawn` (`process_group(0)`), making the child its
own process group leader, so its pgid equals its pid -- meaning the
already-recorded pid alone is enough to kill the whole group via
`kill(-pid, SIGKILL)`, without needing to persist a separate pgid. The Unix
path is implemented from documented POSIX process-group semantics and
`command_group`'s own source, and is CI-verified on real Linux/macOS
runners (`crates/pact-agents/tests/group_kill.rs`, issue #6) -- but a real
agent's own process tree on real Unix hardware remains unconfirmed, since
this project's primary dev environment is Windows.

### Supervisor

`Supervisor` tracks every live child process group across however many
concurrent `run_and_stream` calls share it, so one process-wide Ctrl-C
handler can kill all of them (registering the whole group, not just the
tracked child -- see "Process group kill" above for why) instead of the
single-shot, one-child assumption `run_and_stream`'s old self-installed
handler made. Single-`spawn` and `spawn-many` both go through a
`Supervisor` now: `spawn` just creates its own with exactly one registrant
for the duration of that one call, so its observable behavior (one
handler, killing one child, installed and torn down within a single
`run_and_stream` call) is unchanged; only the mechanism moved from a bare
function into this small object so `spawn-many` can share one across N
threads.

The Ctrl-C handler recovers a poisoned mutex guard (`unwrap_or_else(|p|
p.into_inner())`) rather than bailing out of the handler: a prior panic
while holding the lock (e.g. inside another thread's own cleanup) must not
make every other live child unkillable on Ctrl-C. A failure to install the
handler at all (e.g. an outer caller already installed one) is logged, not
fatal -- the agent process(es) just won't be killed on Ctrl-C in that case.

### run_and_stream

Every raw stdout line is appended to `log_path` as-is (not the
re-serialized `AgentEvent`) so schema drift or fields the parser doesn't
know about yet aren't lost -- then parsed and handed to `on_event`.
`on_pid` is called once, immediately after spawning, so the caller can
persist the PID before this function blocks -- that's what lets a
`teardown` invoked from a different process find and kill a still-running
agent.

stderr is drained on its own thread into the same log file (prefixed
`[stderr] `) rather than left inherited or piped-but-undrained -- either
of those risks interleaved garbage in the terminal or a full-pipe deadlock
if the child writes enough of it.

`parse_line` is adapter-supplied and returns zero or more events for one
raw line, not exactly one, because not every adapter's schema maps one
line to one event: confirmed necessary for Copilot CLI, whose
`assistant.message` events can carry both response text and one or more
tool calls in the same line. Claude Code's schema happens to be
one-event-per-line, but this function doesn't assume that of anyone.

Not every adapter emits an explicit `Result`-shaped event -- Codex's
`turn.completed`, confirmed directly, carries no success/failure signal at
all, so it never produces one. Falling back to `success: false`
unconditionally when none was seen would misreport every successful Codex
run as a failure; the process's own exit code is the honest fallback
signal instead.

### MCP config format confirmation

`write_mcp_json_config`'s JSON shape was confirmed to work for both Claude
Code's `--mcp-config` and Copilot CLI's `--additional-mcp-config @<path>`
by deliberately pointing both real CLIs at a broken command and observing
a loud, non-silent failure -- not just inferred from documentation.

### Adapter-specific quirks

### Claude Code safety default

`ClaudeCodeAdapter`'s default is `--allowedTools` (a curated safe-operation
allowlist covering file read/write/edit/search plus the VCS and
package-manager commands a coding task actually needs), not
`bypassPermissions` -- confirmed directly that an explicit `--allowedTools`
list makes Claude Code deny an out-of-scope tool call cleanly and
immediately in headless mode, rather than hang waiting for an approval
prompt no TTY can answer. `bypassPermissions` alone was the *documented*
fix for the hang; this is a real, verified safer alternative that isn't
all-or-nothing. The allowlist (`DEFAULT_ALLOWED_TOOLS`) isn't
user-configurable yet (see the README's Known limitations) -- the point
for now is proving the mechanism is genuinely safer than the old
bypass-everything default, not claiming this exact list is final.

`--allowedTools` is always passed, harmless alongside an explicit
`--permission-mode` override too (including `bypassPermissions` itself).
`safety_override`, when given, is passed as a raw `--permission-mode`
value; when absent, no `--permission-mode` flag is passed at all --
confirmed that Claude Code's own baseline default mode, combined with the
allowlist, is what produces the clean-deny-not-hang behavior this default
relies on. The MCP config is rendered to a `{"mcpServers": {...}}` JSON
file and passed via `--mcp-config` -- confirmed against the real CLI: a
malformed config is rejected with a loud error before the session starts,
so getting the file wrong is never a silent no-op.

### Claude Code output schema

`parse_line` is modeled directly against real output captured from
`claude -p --output-format stream-json --verbose` (see README), not
secondhand docs. One event in, one event out in every case observed so
far, but it returns a `Vec` to match the shared `AgentAdapter` interface
other adapters need.

`parse_assistant` reports the first recognized content block (text or
tool_use) rather than collecting all of them into a `Vec`, since in
practice Claude Code emits one block per line in stream-json mode.
Anything genuinely mixed falls back to `Other` with the full message
preserved.

### Copilot CLI safety default

Unlike Claude Code, no confirmed non-hanging alternative to
`--allow-all-tools` exists yet (see the README and issue #2's
investigation): `--allow-tool` works for in-scope actions, but a task
needing a tool outside that list hangs (confirmed directly, 50s/zero
output) rather than denying cleanly the way Claude Code's `--allowedTools`
does. Until that's investigated further, `--allow-all-tools` stays the
only working default -- stated plainly in
`default_safety_description` rather than implying parity with Claude
Code's safer one. It also has no gradient (unlike Claude Code's six
permission modes): Copilot CLI's own `--help` states it's "required for
non-interactive mode", so `build_command`'s `safety_override` parameter
has nothing meaningful to override here.

### Copilot CLI output schema

`copilot.rs`'s `parse_line` is modeled directly against real output
captured from `copilot -p ... --output-format json` (see README),
including a real tool-call-forcing task to confirm `toolRequests`' field
names -- `name`/`arguments`, not Claude Code's `name`/`input`. Unlike
Claude Code (one content block per line), Copilot CLI can bundle response
text *and* one or more tool calls into a single `assistant.message` event
-- confirmed directly: a file-writing task produced one line with
non-empty `content` alongside a non-empty `toolRequests` array. Returning
a `Vec` from `parse_line` is what makes that safe to represent without
dropping either half. The MCP config passed via `--additional-mcp-config
@<path>` is the same `{"mcpServers": {...}}` shape Claude Code uses
(confirmed identical) -- the `@` prefix means "load from file" per Copilot
CLI's own docs; without it the argument would be parsed as an inline JSON
string instead.

## pact-coord — MCP coordination server

Advisory, glob-based, TTL-expiring file leases plus a threaded message log
between agents -- not enforcement, and deliberately not deep semantic
dependency analysis (see the README). Runs as its own process (`pact
mcp-serve`, launched by the agent CLI itself over stdio, not run in-process
by the orchestrator) speaking MCP via `rmcp`, backed by a SQLite database
shared across every agent in one repo's session.

### Database placement

The coordination database is *not* placed under `.pact-<repo>/` alongside
per-workspace bookkeeping (locks, metadata, logs). Those are
blast-radius-limited to the one agent whose workspace they belong to; this
database is depended on by *every* agent in the session. That directory
sits directly inside the same tree as each workspace (e.g.
`workspaces/<id>/../../state.db` is a trivially short relative path), and
headless launches default to `bypassPermissions`, so a careless broad shell
command in any one workspace could reach and corrupt state every other
agent depends on. Placing it under the platform's local data directory,
keyed by a hash of the repo root, isn't a hard security boundary (an
agent's Bash tool can still reach anywhere given an absolute or crafted
path) but removes it from being stumbled into by accident via
`../..`-style relative paths, which is the realistic risk.

### WAL mode

WAL is needed because the coordination database is opened concurrently by
a separate OS process per running agent (each `pact mcp-serve` is its own
process), not just separate threads in one process. `busy_timeout` means a
writer under real contention blocks briefly instead of immediately erroring
with `SQLITE_BUSY` -- prior art's "40-50 concurrent agents" claim implies
that contention is the normal case, not an edge case.

### Per-agent read cursors

A cursor per agent (rather than a shared `read_at` column on the message
itself) is what makes broadcasts work correctly: each recipient needs to
see a message once independently of whether other recipients have already
seen it, which a single mutable "read" flag on the row can't represent.

### Lease system

## pact-deps — dependency materialization

Detects a workspace's package manager(s) and makes sure dependencies are
ready before the agent's first real command runs. Most ecosystems (pnpm,
yarn, uv, poetry, pipenv, Cargo, Go modules, Maven, Gradle) already have a
good global shared cache, so those just get their normal install/fetch
command run (`passthrough`). npm (flat, per-project `node_modules`, no
built-in sharing) is routed through a lockfile-hash-keyed content store
instead (`store`), materialized via reflink or read-only hardlink so a
second-plus workspace doesn't pay for a full reinstall. Plain pip/venv is
intentionally left as passthrough-only (see "Passthrough caching strategy"
below).

### The Windows MAX_PATH failure (issue #7)

A real failure mode found while verifying issue #7's fallback path, not a
synthetic test case: the store's key (platform/arch/libc/node/npm version
plus a 64-character lockfile hash) makes store-entry paths meaningfully
longer than a plain per-workspace `node_modules` would be. Confirmed
directly on Windows: `npm ci` populating a store entry for a package with a
postinstall step (`esbuild`) failed with `ENOENT` spawning `cmd.exe` -- not
because `cmd.exe` was missing, but because the fully-qualified path to the
file being installed exceeded Windows' legacy `MAX_PATH` (260 chars) once
nested under a long store-key directory name inside an already-long
temp/state-dir root.

`prepare_npm`'s populate-failure fallback (falling back to a plain,
unshared `npm install` for that one workspace) exists exactly for this
class of precondition-not-met failure -- it was hit for real, not
hypothetically, and the fallback (a shorter path) succeeded where store
population didn't. The same fallback also covers other real causes: a
network blip, a native build tool missing on that specific machine, a
registry issue -- none of which should leave a workspace with no
`node_modules` at all.

### Store key components

`platform_key` distinguishes store entries by OS, architecture, libc
flavor (Linux only), Node major version, and npm's own version -- see
issue #7's risk analysis for why each of these, beyond the original
os/arch/node-major set, turned out to matter: npm version because
different npm versions can lay out `node_modules` differently from an
identical lockfile, and libc flavor because packages that resolve a
platform-specific binary via `optionalDependencies` (esbuild, swc, sharp,
and others in that exact shape) pick a *different* one for musl (Alpine)
vs. glibc (Debian/Ubuntu) despite both reporting the same os/arch.

`libc_suffix` detects musl via the presence of a musl dynamic linker
(`ld-musl-*` in `/lib`), which is how musl libc (Alpine's default)
identifies itself; anything else on Linux is assumed glibc. Best-effort:
if detection is inconclusive, "glibc" is the safer assumption (the
overwhelming majority of non-Alpine Linux), not silently omitting the
dimension entirely.

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
