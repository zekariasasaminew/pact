# pact design notes

The why behind pact's non-obvious decisions: measurements, findings from
manual testing, tradeoffs considered and rejected. The code stays
comment-light; naming and structure carry the what, and these files carry
the why and the history.

## Areas

Each area file opens with a one-screen summary of the current design, a
diagram and the list of superseded decisions, then the issue-by-issue
History.

| file | covers | code |
|---|---|---|
| [vcs.md](docs/design/vcs.md) | worktrees, shared-tree batches, commit, merge, gates, teardown | pact-vcs |
| [core-orchestrator.md](docs/design/core-orchestrator.md) | the Orchestrator: spawn, batches, Weaver, Arbiter, run records, `pact gc` | pact-core |
| [run-planner.md](docs/design/run-planner.md) | `pact run` (planner, verify, repair, ownership), `pact fleet`, `pact land`, `pact discard` | pact-core |
| [acp-runtime.md](docs/design/acp-runtime.md) | one agent process with one ACP session per lane, model selection, the protocol client | pact-core, pact-acp |
| [admission.md](docs/design/admission.md) | admission control and per-lane memory reservation | pact-core |
| [agents-adapters.md](docs/design/agents-adapters.md) | one adapter per agent CLI, safety profiles, lean profiles, models, process supervision | pact-agents |
| [coord.md](docs/design/coord.md) | the coordination server: leases, messages, handoffs, HTTP mode, history, SDKs, VS Code extension | pact-coord, bindings, editors |
| [deps.md](docs/design/deps.md) | dependency materialization: link mode, passthrough, package-manager detection | pact-deps |
| [cli.md](docs/design/cli.md) | the command-line surface: commands, flags, output, `pact ui`, `pact doctor` | pact-cli |
| [ci-release.md](docs/design/ci-release.md) | CI, release channels, packaging, the fake-agent harness and the test tiers | CI, tests |

Older material: [docs/design/history/readme.md](docs/design/history/readme.md)
holds the earlier README's roadmap and design notes.

## Adding a decision

Add a section at the end of the matching area file, with the issue number
in its heading. If it changes how the area works today, update the summary
at the top of that file; if it reverses an earlier decision, add a line
under Superseded decisions. Code comments cite the area file and the
section name.

## Where each section went

Until issue #369 this was one file, organized by crate. Code comments and
older docs cite its sections by name, sometimes as "crate > Section".
Every section moved unchanged to the area file listed here.


### pact-vcs

- PidLock origin: [vcs.md](docs/design/vcs.md)
- Lock timeout: 30s -> 30min (issue #230): [vcs.md](docs/design/vcs.md)
- PID reuse (issue #70): [vcs.md](docs/design/vcs.md)
- Windows delete-pending races are contention, not failure (issue #318): [vcs.md](docs/design/vcs.md)
- Workspace lifecycle: [vcs.md](docs/design/vcs.md)
- Workspace id: task-derived slug + random suffix (issue #122): [vcs.md](docs/design/vcs.md)
- Default slug stops at the first sentence (issue #271): [vcs.md](docs/design/vcs.md)
- Workspace names: `--name` (issue #234): [vcs.md](docs/design/vcs.md)
- Workspace teardown: [vcs.md](docs/design/vcs.md)
- Reparse points and worktree removal (issue #283): [vcs.md](docs/design/vcs.md)
- A crashed `pact` orphans its agent process tree (issue #108): [vcs.md](docs/design/vcs.md)
- commit_all: [vcs.md](docs/design/vcs.md)
- merge_all: [vcs.md](docs/design/vcs.md)
- Risk-aware merge ordering (issue #159): [vcs.md](docs/design/vcs.md)
- Overlap-aware risk scoring (issue #236): [vcs.md](docs/design/vcs.md)
- Test-gated merge (issue #65): [vcs.md](docs/design/vcs.md)
- Gate timing: `--gate each|final` (issue #309): [vcs.md](docs/design/vcs.md)
- Gate diagnosability, and the environment-vs-code distinction (issue #232): [vcs.md](docs/design/vcs.md)
- Dependencies for the integration worktree (issue #269): [vcs.md](docs/design/vcs.md)
- Semantic auto-resolution: [vcs.md](docs/design/vcs.md)
- TOML manifest structural merge (issue #272): [vcs.md](docs/design/vcs.md)
- N-agent-one-file benchmark as a regression test (issue #276): [vcs.md](docs/design/vcs.md)
- Sentinel-marker union-merge insertion (issue #87): [vcs.md](docs/design/vcs.md)
- Arbiter resolver hook: [vcs.md](docs/design/vcs.md)
- Persisted conflicts (issue #85): [vcs.md](docs/design/vcs.md)

### pact-core

- spawn / spawn_many concurrency: [core-orchestrator.md](docs/design/core-orchestrator.md)
- Shared-tree batches (issue #315): [core-orchestrator.md](docs/design/core-orchestrator.md)
- ACP lane runtime: one agent process, one session per lane (issue #331): [acp-runtime.md](docs/design/acp-runtime.md)
- `pact run`: pact owns the split (issue #305): [run-planner.md](docs/design/run-planner.md)
- `pact fleet`: independent tasks (issue #388): [run-planner.md](docs/design/run-planner.md)
- `pact land` and `pact discard` (issue #389): [run-planner.md](docs/design/run-planner.md)
- `pact ui` (issue #390): [cli.md](docs/design/cli.md)
- Admission control (issue #285): [admission.md](docs/design/admission.md)
- Per-lane memory reservation (issue #320): [admission.md](docs/design/admission.md)
- Coordination config wiring: [core-orchestrator.md](docs/design/core-orchestrator.md)
- Weaver: task overlap prediction: [core-orchestrator.md](docs/design/core-orchestrator.md)
- Weaver: negation and brand-name false positives (issue #239): [core-orchestrator.md](docs/design/core-orchestrator.md)
- Weaver: negation lists, read-only verbs, member access (issue #317): [core-orchestrator.md](docs/design/core-orchestrator.md)
- Arbiter: agent invocation: [core-orchestrator.md](docs/design/core-orchestrator.md)
- Arbiter diagnosability (issue #106): [core-orchestrator.md](docs/design/core-orchestrator.md)
- Arbiter's real-world resolution rate was 0/6 before the Write-fresh redesign (issue #106): [core-orchestrator.md](docs/design/core-orchestrator.md)
- Arbiter Write-fresh redesign (issue #106): [core-orchestrator.md](docs/design/core-orchestrator.md)
- Arbiter marker check (issue #380): [core-orchestrator.md](docs/design/core-orchestrator.md)
- Arbiter merge-state neutralization (issue #185): [core-orchestrator.md](docs/design/core-orchestrator.md)
- Arbiter scope enforcement (issue #146/#147): [core-orchestrator.md](docs/design/core-orchestrator.md)
- Arbiter decision records (issue #148): [core-orchestrator.md](docs/design/core-orchestrator.md)
- Repo-declared prepare commands (issue #301): [core-orchestrator.md](docs/design/core-orchestrator.md)
- Structured run metadata (issue #15): [core-orchestrator.md](docs/design/core-orchestrator.md)
- Sidecar records live in directories of their own (issue #343): [core-orchestrator.md](docs/design/core-orchestrator.md)
- Pruning the state directory: `pact gc` (issue #351): [core-orchestrator.md](docs/design/core-orchestrator.md)

### pact-agents

- AgentEvent normalization: [agents-adapters.md](docs/design/agents-adapters.md)
- `Phase` events and the end-of-run summary (issue #241): [agents-adapters.md](docs/design/agents-adapters.md)
- Process group kill: [agents-adapters.md](docs/design/agents-adapters.md)
- Supervisor: [agents-adapters.md](docs/design/agents-adapters.md)
- Orphaned grandchild cleanup, and a deeper pipe-inheritance root cause (issue #237): [agents-adapters.md](docs/design/agents-adapters.md)
- run_and_stream: [agents-adapters.md](docs/design/agents-adapters.md)
- Agent subprocesses get an explicitly closed stdin (issue #184): [agents-adapters.md](docs/design/agents-adapters.md)
- Windows multi-line prompt truncation (issue #210): [agents-adapters.md](docs/design/agents-adapters.md)
- MCP config format confirmation: [agents-adapters.md](docs/design/agents-adapters.md)
- Adapter-specific quirks: [agents-adapters.md](docs/design/agents-adapters.md)
- Claude Code safety default: [agents-adapters.md](docs/design/agents-adapters.md)
- Claude Code output schema: [agents-adapters.md](docs/design/agents-adapters.md)
- Codex adapter: [agents-adapters.md](docs/design/agents-adapters.md)
- Gemini adapter: [agents-adapters.md](docs/design/agents-adapters.md)
- Antigravity adapter (issue #9) -- the real, live-verified resolution: [agents-adapters.md](docs/design/agents-adapters.md)
- Copilot CLI safety default: [agents-adapters.md](docs/design/agents-adapters.md)
- Copilot CLI output schema: [agents-adapters.md](docs/design/agents-adapters.md)
- Safety profiles (issue #161): [agents-adapters.md](docs/design/agents-adapters.md)
- Copilot lean profile (issue #284): [agents-adapters.md](docs/design/agents-adapters.md)
- Claude lean profile (issue #288): [agents-adapters.md](docs/design/agents-adapters.md)
- Model selection and tiers (issue #387): [agents-adapters.md](docs/design/agents-adapters.md)
- The model a lane actually ran on (issue #374): [acp-runtime.md](docs/design/acp-runtime.md)

### pact-coord

- pact-coord: introduction: [coord.md](docs/design/coord.md)
- Database placement: [coord.md](docs/design/coord.md)
- WAL mode: [coord.md](docs/design/coord.md)
- Per-agent read cursors: [coord.md](docs/design/coord.md)
- Lease system: [coord.md](docs/design/coord.md)
- `pact clear-leases` (issue #209): [coord.md](docs/design/coord.md)
- Typed handoff/negotiation protocol (issue #163): [coord.md](docs/design/coord.md)
- Opt-in strict claim response (issue #162): [coord.md](docs/design/coord.md)
- `list_claims` MCP tool (issue #149): [coord.md](docs/design/coord.md)
- Known scaling limit: `expand_glob` cost (issue #72): [coord.md](docs/design/coord.md)
- Pre-initialize requests are answered, not fatal (issue #291): [coord.md](docs/design/coord.md)
- Known limitation: intermittent MCP connection status under concurrency (issue #105): [coord.md](docs/design/coord.md)
- Streamable HTTP mode: one server, one route per lane (issue #329): [coord.md](docs/design/coord.md)
- Coord status (issue #64): [coord.md](docs/design/coord.md)
- Coordination reachability: "no leases" vs. "no server" (issue #235): [coord.md](docs/design/coord.md)
- Operation log / `pact history` (issue #84): [coord.md](docs/design/coord.md)
- Persisted conflicts / `pact resolve` (issue #85): [coord.md](docs/design/coord.md)
- pact-coord SDK bindings v1 (issue #127): [coord.md](docs/design/coord.md)
- VS Code extension v1 (issue #128): [coord.md](docs/design/coord.md)

### pact-acp

- Why a protocol client instead of N processes (issues #306, #330): [acp-runtime.md](docs/design/acp-runtime.md)
- Shape: [acp-runtime.md](docs/design/acp-runtime.md)
- Testing without an agent: [acp-runtime.md](docs/design/acp-runtime.md)

### pact-deps

- pact-deps: introduction: [deps.md](docs/design/deps.md)
- Detection probes must not write to the project (issue #299): [deps.md](docs/design/deps.md)
- Link mode (issue #283): [deps.md](docs/design/deps.md)
- Content store removed in favor of npm's own cache (issue #233): [deps.md](docs/design/deps.md)
- Structured prep reporting (issue #12) -- historical, superseded above: [deps.md](docs/design/deps.md)
- Content store lock timeout, and --no-deps (issue #233): [deps.md](docs/design/deps.md)
- The Windows MAX_PATH failure (issue #7): [deps.md](docs/design/deps.md)
- Store key components: [deps.md](docs/design/deps.md)
- npm store manifest, verification, cleanup (issue #160): [deps.md](docs/design/deps.md)
- Windows `.cmd` shim resolution: [deps.md](docs/design/deps.md)
- Passthrough caching strategy: [deps.md](docs/design/deps.md)
- ReadOnlyHardlink tradeoff -- historical, superseded (issue #233): [deps.md](docs/design/deps.md)
- Package manager detection: [deps.md](docs/design/deps.md)
- Bun detector (issue #17): [deps.md](docs/design/deps.md)
- No committed lockfile (issue #26): [deps.md](docs/design/deps.md)

### pact-cli

- pact-cli: introduction: [cli.md](docs/design/cli.md)
- Doc/CLI grammar drift check (issue #238): [cli.md](docs/design/cli.md)
- SKILL.md routing re-verified against a real Copilot session (issue #203): [cli.md](docs/design/cli.md)
- pact init auto-registers SKILL.md with detected agent CLIs (issue #219): [cli.md](docs/design/cli.md)
- `teardown` bulk mode (issue #214, outside Windows Copilot report): [cli.md](docs/design/cli.md)
- `--union` renamed to `--append-only` (issue #11): [cli.md](docs/design/cli.md)
- `pact doctor --json` (issue #17): [cli.md](docs/design/cli.md)
- `pact inspect` (issue #16): [cli.md](docs/design/cli.md)
- Error messages suggest the next command (issue #123): [cli.md](docs/design/cli.md)
- winget manifest (issue #126): [ci-release.md](docs/design/ci-release.md)
- Homebrew tap (issue #125): [ci-release.md](docs/design/ci-release.md)
- Demo GIF re-recording (issue #124): [cli.md](docs/design/cli.md)
- `pact.toml` / `pact init` (issue #118): [cli.md](docs/design/cli.md)
- `pact demo` (issue #119): [cli.md](docs/design/cli.md)
- `--agent` auto-default (issue #121): [cli.md](docs/design/cli.md)
- `--agent`/prefix precedence in `spawn-many` (issue #37): [cli.md](docs/design/cli.md)
- `--task-file`: real briefs without the argv limit (issue #307): [cli.md](docs/design/cli.md)
- Copilot usage checkpoints (issue #300): [cli.md](docs/design/cli.md)
- Streamed event filtering (issue #38): [cli.md](docs/design/cli.md)
- Workspace commit lifecycle (issue #35): [cli.md](docs/design/cli.md)
- `--dry-run` preview (issue #16): [cli.md](docs/design/cli.md)
- Shell completions (issue #19): [cli.md](docs/design/cli.md)
- `pact doctor` (issue #18): [cli.md](docs/design/cli.md)
- `find_repo_root` home-directory guard (issue #136): [cli.md](docs/design/cli.md)
- Fake-agent end-to-end harness (issue #157): [ci-release.md](docs/design/ci-release.md)
- Slow integration tier (issue #240): [ci-release.md](docs/design/ci-release.md)
- `spawn-many --dry-run --estimate-cost` (issue #222): [cli.md](docs/design/cli.md)
- `pact status` (issue #221): [cli.md](docs/design/cli.md)
- Test suite's own state-directory leak (issue #225): [ci-release.md](docs/design/ci-release.md)

### CI and release infrastructure

- Build footprint (issue #384): [ci-release.md](docs/design/ci-release.md)
- Rolling `edge` release: [ci-release.md](docs/design/ci-release.md)
- `edge` build version string (issue #86): [ci-release.md](docs/design/ci-release.md)
