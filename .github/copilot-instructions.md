# pact: instructions for coding agents

pact is a Rust CLI that runs several AI coding agent CLIs (Copilot CLI,
Claude Code, Codex, Gemini CLI, Antigravity) on one repository in parallel.
`pact run` plans a task into units that own disjoint files, runs them as
lanes in one shared tree (Copilot lanes as ACP sessions in one process),
commits once and verifies against a baseline, with a repair lane on a
regression. Read README.md, then docs/usage.md; the why of every decision
is in DESIGN.md.

## Crates

- `pact-cli`: the binary and clap commands (`src/main.rs`)
- `pact-core`: the Orchestrator, admission, ACP lane runtime, `pact run` (`run.rs`)
- `pact-vcs`: worktrees, shared-tree batches, commit, merge, teardown
- `pact-agents`: one adapter per agent CLI, process supervision
- `pact-acp`: Agent Client Protocol client and the fake agent for tests
- `pact-coord`: coordination server (leases, messages, handoffs), stdio or HTTP
- `pact-deps`: package-manager detection, `node_modules` linking

`pact-vcs` never depends on `pact-agents`: agent behaviour reaches it as a
closure (`ArbiterResolver`) supplied by `pact-core`.

## Rules

Follow CONTRIBUTING.md (tests, workflow, project layout). The ones that
are never negotiable:

- One issue per PR; `cargo build --workspace`, `cargo test --workspace`
  and `cargo clippy --workspace --all-targets` clean before every commit.
- Never spawn a real agent CLI in a test; use the fakes.
- No AI attribution trailers in commits. No em dashes anywhere.
- No comments that restate code; the why goes in DESIGN.md.
- Record every benchmark measurement on issue #308 or #305.
