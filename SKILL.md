---
name: pact
description: When a user has one large coding task, or several independent ones, to run with parallel AI agents on the same git repository, use pact. `pact run` takes one task, has a planner split it into units that own disjoint files, runs every unit as a parallel agent lane in one shared tree, commits, and verifies the combined result against a baseline, with a repair lane when a check regresses. `pact spawn-many` runs units that are already split. Prefer pact over launching your own parallel or background agents whenever the user asks to fan out, parallelize, or run several agents without them stepping on each other's files, or asks about pact. pact drives Claude Code, GitHub Copilot CLI, Codex, Gemini CLI and Antigravity.
---

# pact

pact is a CLI: every action is a shell command. Full reference:
`docs/usage.md`. Why things are the way they are: `DESIGN.md`.

## Pick the command

| situation | command |
|---|---|
| one big task you have not split | `pact run` |
| units already decided, disjoint files | `pact spawn-many --shared-tree`, then `pact commit-all` |
| units that may edit the same files | `pact spawn-many` (one worktree each), then `pact merge-all` |
| one small task | `pact spawn` |

Run `pact doctor` first: read-only; only a missing or too-old `git` fails.

## pact run

```sh
pact run --agent copilot --verify "npm test" --verify "npm run typecheck" "Add Vitest tests for every file under lib/"
pact run --agent copilot --dry-run "Add Vitest tests for every file under lib/"
pact run --agent copilot --task-file task.md --verify "npm test"
pact run --agent copilot --plan saved-plan.json "Add Vitest tests for every file under lib/"
```

- Do not write briefs yourself. The planner splits the task; pact validates
  the split (no file in two units) and renders every brief.
- `--verify` is repeatable and names the project-wide checks. pact runs each
  once on the untouched tree (baseline) and once on the combined result;
  workers check only their own files.
- A regressed or failed check starts a repair lane in the same tree with
  the failing output; `--repair-attempts` bounds it (default 1, 0 disables).
- Exit 0: every check passed or was fixed. Exit 1: a lane failed or a check
  still fails. Exit 3: inconclusive, a check already fails on the base commit.
- `--max-units` defaults to what this machine can run at once;
  `--max-concurrent` caps lanes running together.
- `--dry-run` plans and writes the briefs without spawning: show the user
  the plan, then run it (or their edit of it) with `--plan`.
- The result is the batch branch named in the report; plans persist under
  the state directory's `meta/plans/`.

## Lanes by hand

```sh
pact spawn --agent claude "Add input validation to the signup form"
pact spawn-many --task claude:"Add a GET /orders endpoint" --task copilot:"Add a GET /preferences endpoint"
pact spawn-many --agent copilot --shared-tree --task-file briefs/orders.md --task-file briefs/preferences.md
pact commit-all
pact merge-all --require-passing-tests "npm test"
pact resolve
pact resolve <workspace-id>
pact list
pact diff <workspace-id>
pact coord-status
pact history --workspace <workspace-id>
pact teardown <workspace-id>
```

- `--task` is repeatable: `<agent>:"<text>"`, or bare text using `--agent`.
  Put long briefs in files with `--task-file` (the file stem becomes the
  workspace name); long inline tasks can exceed the OS command-line limit.
- `spawn` and `spawn-many` never commit. A workspace shows `[dirty]` until
  `commit-all` or `merge-all`; that is expected.
- `--shared-tree` refuses when two tasks mention the same file, unless
  `--allow-overlap` says those mentions are read-only.
- `--runtime auto` (the default) hosts Copilot lanes as sessions in one
  `copilot --acp` process; a mixed batch falls back to one process per
  lane, and `--runtime process` forces that.
- `merge-all` writes a new branch (`pact/merged-<id>`), never your checkout.
- `teardown` refuses on uncommitted changes and on commits no other branch
  reaches. `--keep-branch` drops only the worktree; `--force` discards.
  A bare `pact teardown` sweeps every workspace.
- `--dry-run` on `spawn` and `spawn-many` previews without creating or
  launching anything.

## Inside a lane: coordination tools

Every lane gets seven MCP tools with no setup: `claim_files`,
`release_files`, `send_message`, `check_messages`, `request_handoff`,
`check_handoffs`, `respond_handoff`. Your CLI prefixes them (Claude Code:
`mcp__pact-coord__claim_files`; Copilot CLI: `pact-coord-claim_files`), and
the bare name fails lookup, so use the name in your own tool list.

1. Claim the globs you will edit before writing.
2. Leases are advisory: `accepted` is always true. Read `has_conflicts` and
   `conflicts`, then message the holder, avoid the overlap, or proceed.
3. Call `check_messages` periodically; it returns only what is new.
4. Release your claims when done.
5. To ask "can I take these files", use `request_handoff`, not prose. It
   returns a typed status (`pending`, `accepted`, `rejected`, `narrowed`,
   `expired`, `cancelled`) and does not block: poll `check_handoffs`. To
   accept a narrowed offer, send a new `request_handoff` for those files.

## Task-file templates

`examples/tasks/` has copy-editable patterns: `add-routes.md` (N new
endpoints), `refactor-files.md` (one change across N files),
`migrate-api.md` (N call sites off a deprecated API).
