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
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

CI (`.github/workflows/ci.yml`) runs the build and the tests on ubuntu,
macos and windows, plus the Python, TypeScript and VS Code binding jobs.

- Pure logic gets an inline `#[cfg(test)] mod tests` in the same file.
- Anything that needs git gets an integration test under
  `crates/<crate>/tests/` against a real throwaway repo in
  `std::env::temp_dir()`, never mocked git (pattern:
  `crates/pact-vcs/tests/merge_all.rs`, `init_repo()` then `cleanup()`).
- **Never spawn a real agent CLI in a test**: it costs money and can hang.
  Use the fakes: the `fake_acp_copilot` binary (pact-acp's fake agent:
  prose prompts get the reply in `FAKE_ACP_REPLY_FILE`, JSON tasks write
  files; see `crates/pact-cli/tests/pact_run.rs`), the `fake_agent` shim, or a stub
  closure such as `ArbiterResolver`.
- A fixture that passes or fails regardless of the code under test (`true`,
  `false`) tests only the plumbing; use one that depends on the real
  condition (`crates/pact-vcs/tests/require_passing_tests.rs`).
- Expensive real-concurrency tests live in
  `crates/pact-cli/tests/slow_integration.rs`, `#[ignore]`d; run them with
  `cargo test --ignored`.
- Reproduce any new git behaviour by hand in a scratch repo before relying
  on it; git's 3-way merge has surprised this codebase before.
- `docs_cli_grammar.rs` parses every `pact` command in README.md, SKILL.md,
  GETTING_STARTED.md and docs/usage.md against the real CLI.

## Workflow

- One GitHub issue per finding or feature, filed before the code; one PR
  per issue: branch from `main`, PR, all CI checks green, squash merge.
- Every commit builds and passes `cargo test --workspace` on its own; small
  commits, one concern each; rebase on `main` before pushing.
- Imperative commit messages that say why. No AI attribution trailers.
- Default to no comments: naming carries the what. Exceptions: `///` on
  public API, `// SAFETY:`, clap `///` help text. The why goes in
  `DESIGN.md`, referenced by section name; say there when something is
  implemented but not verified against a real paid agent call.

## Project layout

- `pact-cli` -- the `pact` binary and its `clap` commands (`src/main.rs`).
- `pact-core` -- the `Orchestrator`: workspaces, dependency prep, admission
  by memory (`admission.rs`), lane launch, the ACP lane runtime
  (`acp_runtime.rs`) and `pact run` (`run.rs`).
- `pact-vcs` -- git worktree lifecycle, the PID-aware lock that serializes
  `git worktree add`/`remove`, shared-tree batches, `commit_all`,
  `merge_all`, teardown and its safety checks. No dependency on
  `pact-agents`: agent hooks such as `ArbiterResolver` arrive as closures.
- `pact-deps` -- detects package managers; links the repo root's
  `node_modules` (`link.rs`) or passes through to each ecosystem's own
  install and cache (`passthrough.rs`).
- `pact-agents` -- the `AgentAdapter` trait (`adapter.rs`), one module per
  agent CLI, process spawn, streaming and supervision.
- `pact-acp` -- the Agent Client Protocol client: one agent process, one
  session per lane, plus the fake agent used by tests.
- `pact-coord` -- the coordination server (file leases, messages,
  handoffs), over stdio (`pact mcp-serve`) or Streamable HTTP with one
  route per lane.

## Adding a new agent CLI adapter

Implement `pact_agents::AgentAdapter` (`crates/pact-agents/src/adapter.rs`;
`claude_code.rs` and `copilot.rs` are two different shapes). You need
`build_command`/`build_launch` (a headless launch including the CLI's
unattended-safety flag: there is no TTY to answer a permission prompt),
`parse_line` (one stdout line to zero or more `AgentEvent`s; one line can
carry several), `coord_server_name` and `default_safety_description`. If the
CLI has an Agent Client Protocol mode, also `supports_acp` and
`build_acp_launch`. Register the variant in `AgentKind` and `adapter()`.

Then live-verify it against the real installed CLI, including a
`claim_files` call through the coordination server: this project has been
burned by trusting CLI documentation over the binary. Record the result in
`DESIGN.md` and update the Safety table in README.md and the known
limitations in `docs/usage.md`.

## Adding a new package-manager detector

Detection is in `crates/pact-deps/src/detect.rs`; an ecosystem with its
own cache (cargo, go modules, uv, pnpm, yarn, poetry, pipenv) is wired
through to its native install in `passthrough.rs`. npm's `node_modules` is
linked from the repo root by default (`link.rs`). Plain pip/venv gets no
shared store on purpose (venvs are not reliably relocatable; see
"Dependency sharing leans on what already exists" in
`docs/design/history/readme.md`).

## Filing a bug

Include: your OS, which agent CLI (and version) you were running, the
exact `pact` command, and -- if you can -- the raw NDJSON log from
`<state-dir>/logs/<workspace-id>.jsonl` (see State layout in
`docs/usage.md` for where that lives). A repro against a scratch repo is the most
useful thing you can attach; this project has consistently found real bugs
only by actually running things, not by reading code.

## Commit style

Small, logically-scoped commits over one large one. Reference the issue a
commit resolves (`closes #N`) where applicable.
