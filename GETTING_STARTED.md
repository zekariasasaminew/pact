# Getting started

From install to one task split across parallel agents and verified, in
about ten minutes. Every command is real.

## 1. Install

Download a binary from the [README](README.md#getting-started) (or
`brew install pact` after `brew tap zekariasasaminew/pact`) and put it on
your `PATH`. You also need one agent CLI installed and signed in:
[GitHub Copilot CLI](https://docs.github.com/en/copilot/how-tos/set-up/install-copilot-cli)
(fastest: its lanes share one process),
[Claude Code](https://docs.claude.com/en/docs/claude-code) or
[Codex](https://developers.openai.com/codex/cli/).

```sh
pact demo
pact doctor
```

`pact demo` runs a disposable walkthrough in a temp repo with no agent
calls and no cost. `pact doctor` lists the agent CLIs and package managers
it finds; only a missing or too-old `git` is an error.

## 2. Set defaults (optional)

Inside your repository:

```sh
pact init
```

This writes `pact.toml` with the agent it detected, so `--agent` becomes
optional. If generated files are missing from a fresh checkout (Next.js
route types, a Prisma client), add the command that makes them under
`[defaults] prepare = ["npx next typegen"]`.

## 3. See the plan before spending anything

```sh
pact run --agent copilot --dry-run "Add unit tests for every module under src/lib/"
```

A planner session reads the repository and splits the task into units
that own disjoint files. pact validates the split, writes one brief per
unit, prints the plan and stops. The plan is saved under the state
directory's `meta/plans/`; edit it if you disagree with the split.

## 4. Run it

```sh
pact run --agent copilot --verify "npm test" --verify "npm run lint" "Add unit tests for every module under src/lib/"
```

What happens, in order:

1. pact creates one shared working tree and runs each `--verify` command on
   it untouched, so it knows which checks already fail on your base commit.
2. The units run as parallel lanes, as many at once as memory allows. Each
   lane prints its progress prefixed `[copilot:<n>]`.
3. pact commits the combined result once and runs every check again.
4. A check that passed before and fails now gets one repair lane in the
   same tree, then the checks run again.

The last lines are the verdicts and where the result is:

```
result: branch pact/batch-<id> in <state dir>/workspaces/batch-<id> (committed)
verify `npm test`: passed (exit 0, 31.2s)
run: OK. Review with `pact diff batch-<id>`, land with `pact merge-all`, or push pact/batch-<id>
```

Exit code 0 means every check passed, 1 that a lane failed or a check
still fails, 3 that a check already failed on the base commit and cannot
judge the run. To rerun a plan you edited:
`pact run --plan <path printed by the dry run> "<same task>"`.

## 5. Review, land, clean up

```sh
pact diff <id>
pact list
pact teardown
```

Push the result branch and open a pull request as usual, or merge it
locally. `pact teardown` without an id removes every workspace; it refuses
to delete uncommitted work or commits no other branch reaches unless you
pass `--force`.

## 6. When you already know the split

```sh
pact spawn-many --agent copilot --shared-tree --task "Add tests for src/lib/a.ts" --task "Add tests for src/lib/b.ts"
pact commit-all
```

`--shared-tree` fits tasks that touch different files. Leave it off when
tasks may edit the same files: each then gets its own git worktree, and
`pact merge-all --require-passing-tests "npm test"` lands them on one new
branch.

## Next

- [docs/usage.md](docs/usage.md): every command and flag, the safety model,
  known limitations.
- [`examples/tasks/`](examples/tasks/): task patterns for adding N routes,
  refactoring N files, migrating N call sites.
- [CONTRIBUTING.md](CONTRIBUTING.md): building from source and adding an
  adapter.
