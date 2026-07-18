# pact — Claude Code instructions

See `~/.claude/CLAUDE.md` for the global git/CI workflow policy that applies here too (small commits, tests+CI before push, branch kept current with main). This file adds pact-specific detail on top of it.

## What this is

pact is a Rust CLI that orchestrates multiple AI coding agent CLIs (Claude Code, GitHub Copilot CLI, Codex, Gemini CLI) running in parallel on the same repo, via git worktrees + shared dependency caching + an MCP coordination server (file leases + messages). **The README is the source of truth for design decisions — read it first in a new session, before this file.**

Workspace layout (`crates/`):
- `pact-cli` — the `pact` binary, clap command definitions
- `pact-core` — `Orchestrator`: ties workspace lifecycle, dependency prep, and agent launch together
- `pact-vcs` — `WorkspaceManager`: git worktree lifecycle, diffing, conflict detection, `merge_all`
- `pact-agents` — per-CLI adapters (Claude Code, Copilot, Codex, Gemini), process supervision
- `pact-coord` — the MCP coordination server (SQLite-backed file leases, messaging)
- `pact-deps` — shared dependency materialization across workspaces

## Git & CI workflow — always follow this

- **Small, frequent commits.** One logical concern per commit — a single new function/command, a single struct change, a single test file. A multi-layer feature (e.g. "close the merge loop") should land as a sequence of small commits, each independently buildable, not one large commit at the end.
- **Every commit must build and pass tests on its own.** Before committing: `cargo build --workspace` and `cargo test --workspace` must both be clean. Don't leave an intermediate commit in a broken state, even temporarily.
- **Run the CI checks locally before pushing.** This repo's CI (`.github/workflows/ci.yml`) runs `cargo build --workspace --verbose` and `cargo test --workspace --verbose` on ubuntu/macos/windows. Also run `cargo clippy --workspace --all-targets` locally — CI doesn't currently gate on it, but it should be clean anyway.
- **Keep your branch current with `main` before pushing.** `main` is protected (no direct pushes, no force-push, no deletion, PR + all 3 CI matrix checks required) — rebase or merge `main` into your branch first so the PR doesn't go stale.
- **No AI attribution trailers** — never `Co-Authored-By: Claude`/`Co-authored-by: Copilot` unless explicitly asked.
- **Meaningful commit messages** — imperative, specific, states *why* when not obvious (this repo's existing commit style already leans toward detailed doc comments explaining *why*, not just *what* — match that).

## Testing conventions in this repo

- Pure-logic pieces get `#[cfg(test)] mod tests` unit tests inline in the same file (see `pact-vcs/src/lib.rs`, `pact-core/src/lib.rs`).
- Anything that needs a real git repo (e.g. `merge_all`) gets an integration test under `crates/<crate>/tests/*.rs` against a real throwaway repo built with `std::env::temp_dir()` — not mocked git. See `crates/pact-vcs/tests/merge_all.rs` for the pattern (helper `init_repo()`, always `cleanup(&repo)` at the end of the test).
- **Never spawn a real agent CLI (claude/copilot/codex/gemini) in a test.** It costs real money and can hang. Where agent-invoking logic needs test coverage, inject a stub closure/fake instead (see `ArbiterResolver` in `pact-vcs` — pact-core builds the real agent-spawning closure, tests pass a stub that never touches a real process).
- Before hand-verifying any new git-interaction behavior, reproduce the exact git scenario by hand in a scratch repo first (`git init` + manual branches) to confirm the real git behavior matches what the code assumes — this codebase has already been burned once by an incorrect assumption about how `git merge`'s 3-way merge handles single-line-context conflicts.

## Other practices

- Doc comments here lean long and explain *why*, including references to the specific issue/trial report that motivated a piece of code, and what's been *confirmed* by hand vs. only reasoned about. Keep that standard — it's load-bearing for a solo-maintained project.
- Anything implemented but not exercised against a real paid agent call (e.g. the Gemini adapter, Arbiter's live agent path) should say so explicitly in a doc comment, matching the existing "implemented-not-live-verified" convention (see issue #6, #9).
- Clean up scratch/temp repos created during manual verification (`AppData/Local/Temp/claude/.../scratchpad` or similar) — don't leave them behind.
