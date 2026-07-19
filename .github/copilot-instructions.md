# Copilot Instructions for pact

## What this project is

pact is a Rust CLI that orchestrates multiple AI coding agent CLIs (Claude Code, GitHub Copilot CLI, Codex, Gemini CLI) running in parallel on the same git repo, via git worktrees, shared dependency caching, and an MCP coordination server (file leases + messaging), without them fighting each other. The README is the source of truth for design decisions.

## Owner's git workflow — always follow this

- **Small, frequent commits.** One logical concern per commit — a single new function/command, a single struct/type change, a single test file. Don't batch a multi-layer feature into one commit; land it as a sequence of small commits, each independently buildable.
- **Every commit builds and passes tests on its own.** `cargo build --workspace` and `cargo test --workspace` must both be clean before each commit — never leave an intermediate commit broken.
- **Run CI checks locally before pushing.** CI (`.github/workflows/ci.yml`) runs `cargo build --workspace --verbose` and `cargo test --workspace --verbose` on ubuntu/macos/windows; also run `cargo clippy --workspace --all-targets` locally.
- **Keep the branch current with `main` before pushing.** `main` is protected (PR required, all 3 CI matrix checks required, no force-push/direct-push/deletion) — rebase/merge `main` into your branch first.
- **No Co-authored-by trailers.** Never add `Co-authored-by: Copilot`/`Co-Authored-By: Claude` or any AI attribution to commits.
- **Conventional, specific commit messages.** Imperative mood, states *why* when not obvious from the diff.

## Architecture

```
crates/
  pact-cli    — the `pact` binary; clap command definitions (src/main.rs)
  pact-core   — Orchestrator: ties workspace lifecycle + dependency prep + agent launch together
  pact-vcs    — WorkspaceManager: git worktree lifecycle, diffing, conflict detection, merge_all
  pact-agents — per-CLI adapters (Claude Code, Copilot, Codex, Gemini) + process supervision
  pact-coord  — MCP coordination server (SQLite-backed file leases, messaging)
  pact-deps   — shared dependency materialization across workspaces
```

`pact-vcs` has no dependency on `pact-agents` by design — anywhere agent-invoking behavior needs to hook into pact-vcs's git logic (e.g. the Arbiter conflict resolver), it's threaded through as a generic closure parameter (`ArbiterResolver`), and `pact-core` (which does depend on `pact-agents`) supplies the real implementation. Keep that separation when adding similar hooks.

## Testing rules

- Pure-logic code gets inline `#[cfg(test)] mod tests` unit tests in the same file.
- Anything needing a real git repo gets an integration test under `crates/<crate>/tests/*.rs` against a real throwaway repo (`std::env::temp_dir()`), never mocked git — see `crates/pact-vcs/tests/merge_all.rs`.
- **Never spawn a real agent CLI in a test** — it costs real money and can hang. Inject a stub closure/fake instead of a real `claude`/`copilot`/`codex`/`gemini` process.
- Confirm any new git-interaction assumption by hand in a scratch repo before relying on it in code — git's 3-way merge behavior around single-line-context conflicts has already surprised this codebase once.

## Code style

- **Default to no comments.** Naming and structure carry the *what* and *why*. Exceptions: brief `///` summaries on public API (real documentation, not narrative), `// SAFETY:` on unsafe blocks, and a short load-bearing note at a genuinely non-obvious branch naming can't carry. Clap's `///` on CLI command/flag definitions is `--help` text, not a code comment, and stays verbose.
- `DESIGN.md` at the repo root holds this project's *why*: empirical findings confirmed by hand, trial-report-driven fixes, rejected alternatives, organized by crate. Point to it by section name (`-- see DESIGN.md ("pact-vcs > merge_all")`) instead of writing that context inline. Say explicitly when something is implemented but not live-verified against a real paid agent call (matches the existing convention for the Gemini adapter and Arbiter's live-agent path) — in `DESIGN.md`, not a comment.
- Prefer editing existing files over creating new ones; avoid abstraction the current task doesn't need.
