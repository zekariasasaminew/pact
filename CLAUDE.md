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
- **Meaningful commit messages** — imperative, specific, states *why* when not obvious from the diff alone.
- **Backlog work goes issue → branch → PR → CI → merge, one issue per PR.** File a GitHub issue for each distinct finding/feature before writing code for it (even when several are decided in the same conversation) — don't bundle unrelated concerns into one PR just because they were discussed together. This is the standing pattern for every shakedown/review/adoption-notes pass this project has gone through; keep following it by default, not just when told each time.

## Testing conventions in this repo

- Pure-logic pieces get `#[cfg(test)] mod tests` unit tests inline in the same file (see `pact-vcs/src/lib.rs`, `pact-core/src/lib.rs`).
- Anything that needs a real git repo (e.g. `merge_all`) gets an integration test under `crates/<crate>/tests/*.rs` against a real throwaway repo built with `std::env::temp_dir()` — not mocked git. See `crates/pact-vcs/tests/merge_all.rs` for the pattern (helper `init_repo()`, always `cleanup(&repo)` at the end of the test).
- **Never spawn a real agent CLI (claude/copilot/codex/gemini) in a test.** It costs real money and can hang. Where agent-invoking logic needs test coverage, inject a stub closure/fake instead (see `ArbiterResolver` in `pact-vcs` — pact-core builds the real agent-spawning closure, tests pass a stub that never touches a real process).
- Before hand-verifying any new git-interaction behavior, reproduce the exact git scenario by hand in a scratch repo first (`git init` + manual branches) to confirm the real git behavior matches what the code assumes — this codebase has already been burned once by an incorrect assumption about how `git merge`'s 3-way merge handles single-line-context conflicts.
- **A fixture whose command is `true`/`false` (or any input that trivially always succeeds/fails regardless of what's under test) only tests the plumbing, not the feature** — issue #240/#11's own critique, confirmed against this repo's prior suite: `require_passing_tests` was covered exclusively by `true`/`false` gates, which couldn't have caught #232's "the gate can't even run in this environment" bug by construction, since neither fixture needs an environment to run in. When a fixture *can* trivially pass/fail independent of the real condition being tested, replace it with one that can't (a content-based check, a real-but-cheap command) — see `crates/pact-vcs/tests/require_passing_tests.rs`'s `fails_if_b_txt_exists` for the pattern.
- **Real concurrency can surface races a fast synthetic approximation doesn't.** `crates/pact-cli/tests/slow_integration.rs` is a separate, `#[ignore]`d tier for tests expensive enough (a real N-way concurrent spawn, a real `npm ci`) that they don't belong in default `cargo test --workspace` — run explicitly with `cargo test --ignored`. Its first test caught a real, previously-undiscovered race (issue #240: `store_hit` reporting, computed via an unlocked pre-check, could misreport a hit as a miss under real concurrent contention that the fast fake-agent-based tests never triggered) on its first run. When a fix targets a race/contention bug, prefer proving it at the fast, deterministic unit level once you understand the mechanism (see `populate_if_absent_reuses_a_slow_populate_instead_of_duplicating_it`'s use of an artificial delay instead of a real slow operation) — but don't skip writing at least one real, slow, `#[ignore]`d integration test too if the bug was only found through real concurrency in the first place, since that's the only tier proven to catch that class of bug here.

## Comments

Default to no comments — see `~/.claude/CLAUDE.md`'s "Comments" section for the full policy (naming/structure carries the *what* and *why*; brief `///` public-API docs and `// SAFETY:` comments are the exceptions; clap's `///` on CLI command/flag definitions is `--help` text, not a code comment, and stays verbose).

`DESIGN.md` at the repo root is where this project's *why* actually lives: empirical findings from manual testing, trial-report-driven fixes, tradeoffs considered and rejected, what's been confirmed by hand vs. only reasoned about — organized by crate. When code needs that context, point to a `DESIGN.md` section by name (`-- see DESIGN.md ("pact-vcs > merge_all")`) rather than writing it inline. Read it, and keep it current when you add or change something worth recording there.

## Other practices

- Anything implemented but not exercised against a real paid agent call (e.g. the Gemini adapter, Arbiter's live agent path) should say so explicitly — in `DESIGN.md`, not an inline comment — matching the existing "implemented-not-live-verified" convention (see issue #6, #9).
- Clean up scratch/temp repos created during manual verification (`AppData/Local/Temp/claude/.../scratchpad` or similar) — don't leave them behind.
