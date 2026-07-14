# Contributing to pact

## Good first issues

Issues tagged [`good first issue`](https://github.com/zekariasasaminew/pact/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
are scoped to be finishable in an afternoon, with a specific starting
point pointed out in each one -- a new package-manager detector, a new
CLI flag, a diagnostic command, shell completions. Good places to start
without needing deep familiarity with the whole codebase first.

## Build from source

Requires a stable Rust toolchain ([rustup.rs](https://rustup.rs)). On
Windows you'll also need a linker -- either the MSVC Build Tools (the
default `stable-x86_64-pc-windows-msvc` toolchain expects one) or switch
to the `stable-x86_64-pc-windows-gnu` toolchain, which doesn't need one.
If you just want to run `pact` without any of this, see
[Getting started](README.md#getting-started) in the README for prebuilt
binaries instead.

```sh
git clone https://github.com/zekariasasaminew/pact.git
cd pact
cargo build --workspace
```

The binary is at `target/debug/pact` (or `target/release/pact` with
`cargo build --release --workspace`).

## Test

```sh
cargo test --workspace
cargo clippy --all-targets
```

There isn't a large unit-test suite -- this project's own practice is to
verify behavior end-to-end against real installed agent CLIs (Claude Code,
Copilot CLI, Codex) rather than mock them, since mocked I/O has already
hidden real bugs here before (a Windows `.cmd`-shim resolution bug, an
incorrect Codex flag, a hardcoded-`false` success bug -- see the README's
verification sections for the full list). If you're changing anything in
`pact-agents` or `pact-vcs`, the most valuable thing you can do is
actually run `pact spawn`/`spawn-many`/`teardown` against a scratch repo
with a real agent CLI installed and confirm the behavior you changed.

## Project layout

- `pact-vcs` -- git worktree lifecycle, the PID-aware lock that fixes
  concurrent `git worktree add`/`remove` races, workspace metadata,
  teardown (including the uncommitted-changes safety check).
- `pact-deps` -- dependency broker: detects the package manager in a
  workspace and either passes through to its own cache (`passthrough.rs`)
  or, for npm specifically, materializes from a shared content store
  (`store.rs`).
- `pact-agents` -- the `AgentAdapter` trait, one module per agent CLI
  (`claude_code.rs`, `copilot.rs`, `codex.rs`), the shared process-spawn/
  stream/supervise machinery (`process.rs`, `supervisor.rs`).
- `pact-coord` -- the MCP coordination server (file leases + messages),
  run as `pact mcp-serve`.
- `pact-core` -- ties the above together behind a stable `Orchestrator`
  interface (`spawn`, `spawn_many`, `list`, `diff`, `teardown`).
- `pact-cli` -- the `clap`-based CLI surface.

## Adding a new agent CLI adapter

Implement `pact_agents::AgentAdapter` (see `crates/pact-agents/src/adapter.rs`
for the trait, and `claude_code.rs`/`copilot.rs` for two real examples of
different shapes -- Claude Code and Copilot CLI both take a JSON
`--mcp-config` file, while Codex takes inline `-c` overrides instead). You
need:

- `build_command`: program name + args for a headless, non-interactive
  launch, including whatever unattended-safety flag that CLI needs (there
  is no TTY to answer an interactive permission prompt in this
  architecture -- every adapter needs *some* such setting).
- `parse_line`: turn one raw stdout line into zero or more `AgentEvent`s.
  Don't assume one-line-one-event -- confirmed necessary for Copilot CLI,
  whose events can carry both response text and tool calls on the same
  line.
- `coord_server_name` / `default_safety_description`: see the trait's doc
  comments.

Register the new variant in `AgentKind` (`adapter.rs`) and wire it into
`adapter()`'s match. Then **live-verify it against the real, installed
CLI** -- this project has been burned twice by trusting a CLI's own
documentation over what it actually does (a documented Codex flag that
didn't exist in the real binary; assumed event field names that turned
out to differ from Claude Code's). Update the README's adapter status
table once it's confirmed working end-to-end, including a real
coordination-server (`claim_files`) call through the generated MCP config.

## Adding a new package-manager detector

See `pact-deps/src/detect.rs` for the existing detection logic and
`passthrough.rs` for how an ecosystem with its own cache (cargo, go
modules, uv, pnpm, yarn, poetry, pipenv) is wired through to a plain
native install call. Only npm gets a custom content store (`store.rs`) --
see the README's Design decisions for why plain pip/venv was deliberately
not given one (venvs aren't reliably relocatable).

## Filing a bug

Include: your OS, which agent CLI (and version) you were running, the
exact `pact` command, and -- if you can -- the raw NDJSON log from
`<state-dir>/logs/<workspace-id>.jsonl` (see the README's State layout
section for where that lives). A repro against a scratch repo is the most
useful thing you can attach; this project has consistently found real bugs
only by actually running things, not by reading code.

## Commit style

Small, logically-scoped commits over one large one. Reference the issue a
commit resolves (`closes #N`) where applicable.
