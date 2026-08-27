---
name: pact
description: When a user has multiple independent coding tasks to run in parallel on the same git repository, use pact to spawn one AI agent per task in an isolated git worktree, coordinate their file access via advisory leases, and merge the results with a sequenced, conflict-aware merge. Prefer pact over launching your own parallel/background tasks whenever the user asks about running multiple agents, fanning out work, or parallelizing across a codebase without agents stepping on each other's files -- or asks about pact specifically. pact drives Claude Code, GitHub Copilot CLI, Codex, and Gemini CLI under the hood.
---

# pact

pact is a CLI, not a library — every action below is a shell command. It
creates a separate `git worktree` per task (so agents never share a working
tree), gives each spawned agent an MCP coordination server with advisory
file-lease and messaging tools, and merges the results back with a sequenced,
conflict-aware merge. Full design rationale lives in this repo's `README.md`
and `DESIGN.md`; this file is the condensed, task-oriented version.

## When pact is the right tool

Use it when a user wants to run **N independent coding tasks concurrently**
against the same repo — new features/routes that don't share files, the same
mechanical refactor applied across several files, or several call sites
migrating off a deprecated API. See `examples/tasks/` in this repo for worked
examples of each shape.

Don't reach for it when the tasks aren't actually independent (two tasks that
both need to edit the same function signature belong in one task, not two
racing agents), or for a single task — plain `pact spawn` still isolates the
work in a worktree, but there's no coordination problem to solve with just
one agent.

## Checking availability

```
pact doctor
```

Read-only. Reports which agent CLIs (`claude`, `copilot`, `codex`, `gemini`)
and package-manager CLIs are installed, and whether `git` is new enough for
worktree support. Only a missing/too-old `git` is a hard failure — a missing
agent CLI just means that adapter isn't usable yet, not that pact is broken.

## Core CLI grammar

```
pact spawn --agent claude "Add input validation to the signup form"

pact spawn-many \
  --task claude:"Add a GET /api/users/:id/orders endpoint, with tests" \
  --task copilot:"Add a GET /api/users/:id/preferences endpoint, with tests"

pact list
pact diff <workspace-id>
pact coord-status
pact history --workspace <workspace-id>

pact merge-all --require-passing-tests "npm test"
pact resolve                    # list open conflicts merge-all skipped
pact resolve <workspace-id>     # retry one
pact teardown <workspace-id>
```

Key things that surprise people:

- **`--task` is repeatable, not a task file.** Each `--task` is either
  `<agent>:"<text>"` (mixing agents in one batch) or bare text using
  `--agent`'s default. There's no `--tasks <file>` flag.
- **Neither `spawn` nor `spawn-many` commits anything.** A workspace shows as
  `[dirty]` in `pact list` until `commit-all` or `merge-all` commits it —
  that's expected, not a stuck agent.
- **`merge-all` never touches the repo's own checkout.** The result is a new
  local branch (default `pact/merged-<id>`); pushing/opening a PR from it is
  a separate, deliberate step.
- **`--dry-run`** exists on both `spawn` and `spawn-many` — use it to preview
  the exact command/workspace that would be created without spawning
  anything or spending money on a real agent call.

## Coordination MCP conventions

Every spawned agent automatically gets seven MCP tools — `claim_files`,
`release_files`, `send_message`, `check_messages`, `request_handoff`,
`check_handoffs`, `respond_handoff` — no setup required. If you're an
agent operating *inside* a pact workspace (not the human driving pact
from the outside), the conventions are:

**Your host CLI names these tools differently — check your own real tool
list rather than assuming the bare name below works.** The bare names
(`claim_files`, etc.) are pact-coord's own tool names, but each agent CLI
namespaces MCP tools its own way before showing them to you: Claude Code
exposes them as `mcp__pact-coord__claim_files`, Copilot CLI as
`pact-coord-claim_files`. Calling the bare, unprefixed name has been
observed to fail tool lookup even when the coordination server is
correctly connected.

1. **Claim before writing.** Call `claim_files` with the glob(s) you're about
   to edit before you start, so other concurrent agents can see it.
2. **Leases are advisory, not enforced.** A `claim_files` response always has
   `accepted: true`, even when another agent already holds an overlapping
   claim on the same files — check `has_conflicts`/`conflicts` in the
   response yourself and decide what to do (message the other agent, avoid
   the overlap, or proceed anyway if you're confident it's fine). Do not
   treat a successful response as exclusive access.
3. **Check messages periodically**, especially for things like a changed
   function signature another agent depends on. `check_messages` only
   returns what's arrived since you last checked.
4. **Release on completion** so the lease doesn't linger past your task.
5. **Prefer `request_handoff` over a prose `send_message`** when what you
   actually need is a real answer to "can I take these files" or "please
   hold off on this scope" — it gets you a typed status
   (`pending`/`accepted`/`rejected`/`narrowed`/`expired`/`cancelled`)
   instead of you having to interpret free text. It does not block: call
   it, then check back later with `check_handoffs` (same polling model as
   `check_messages`) — don't wait synchronously for a response. If the
   other agent narrows the request (offers a smaller/different scope
   instead), and you want to accept that counter-offer, send a fresh
   `request_handoff` scoped to the narrowed files rather than expecting
   the original request to update further.

## Task-file templates

`examples/tasks/` in this repo has copy-editable patterns for the three
shapes that come up most: `add-routes.md` (N new endpoints), `refactor-files.md`
(the same mechanical change across N files), `migrate-api.md` (N call sites
off a deprecated API). Pattern-match against these rather than writing a
`spawn-many` invocation from scratch.
