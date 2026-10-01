# Example task patterns

`pact spawn`/`pact spawn-many` take task text directly as an argument, or read
it from a file with `--task-file <path>` (its contents become the task text and
its file stem becomes the workspace name). These are copy-editable patterns for
the task *text* itself, for the three shapes that come up most: adding several
similar things, refactoring several similar files, and migrating off an old API.

Each file below shows the scenario, why it's a good `spawn-many` candidate
(the N units of work don't depend on each other), and the exact command.
Swap in your own file paths/names — the text after `--task <agent>:` is a
literal prompt handed to the agent CLI, not a pact-specific syntax. For real,
long briefs, put each unit's text in its own file and pass `--task-file` per
unit: a batch of long inline `--task` strings can exceed the OS command-line
length limit, and one file per unit is what an orchestrator naturally produces.

- [`add-routes.md`](./add-routes.md) — N new, independent endpoints/routes
- [`refactor-files.md`](./refactor-files.md) — the same mechanical change across N files
- [`migrate-api.md`](./migrate-api.md) — N call sites moving off a deprecated API

See the README's ["Spawn / teardown flow"](../../README.md#spawn--teardown-flow)
section for the full `--task <agent>:"<text>"` grammar.
