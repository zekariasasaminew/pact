# Getting started

From install to watching two agents work in parallel on the same repo, in
under 5 minutes. Every command below is real -- copy-pasteable against a
scratch repo, not illustrative pseudocode.

## 1. Install (30 seconds)

Download a prebuilt binary -- no Rust toolchain, no MSVC linker/Build
Tools install required. Pick your platform:

```sh
# macOS (Apple Silicon)
curl -L https://github.com/zekariasasaminew/pact/releases/latest/download/pact-aarch64-apple-darwin.tar.gz | tar xz

# macOS (Intel)
curl -L https://github.com/zekariasasaminew/pact/releases/latest/download/pact-x86_64-apple-darwin.tar.gz | tar xz

# Linux (x86_64)
curl -L https://github.com/zekariasasaminew/pact/releases/latest/download/pact-x86_64-unknown-linux-gnu.tar.gz | tar xz
```

```powershell
# Windows (x86_64)
Invoke-WebRequest https://github.com/zekariasasaminew/pact/releases/latest/download/pact-x86_64-pc-windows-msvc.zip -OutFile pact.zip
Expand-Archive pact.zip
```

You should now have a `pact` (or `pact.exe`) binary. Move it onto your
`PATH`, or just reference it by its full path in the commands below.

You'll also need at least one agent CLI installed and authenticated:
[Claude Code](https://docs.claude.com/en/docs/claude-code), [GitHub
Copilot CLI](https://docs.github.com/en/copilot/how-tos/set-up/install-copilot-cli),
or [Codex](https://developers.openai.com/codex/cli/). `pact` orchestrates
these -- it doesn't replace them.

## 2. A scratch repo (30 seconds)

```sh
mkdir pact-demo && cd pact-demo
git init
echo "# demo" > README.md
git add README.md && git commit -m "init"
```

Any real git repo works too -- a scratch repo just means nothing you care
about is at risk while you're trying this out.

## 3. Run one agent (1-2 minutes)

```sh
pact spawn "create a file named hello.txt containing the word hello, then stop"
```

You'll see a warning about the unattended-safety setting being used (every
agent CLI needs one in headless mode -- see the main README's Design
decisions for why), then live streamed output as the agent works, then a
final summary:

```
workspace <id> (pact/<id>)
  path: /path/to/.pact-pact-demo/workspaces/<id>
  done: Created hello.txt containing "hello".
```

Check `pact list` -- your workspace is there, in its own `git worktree`,
completely isolated from your actual repo.

## 4. Run two agents in parallel (1-2 minutes)

This is the actual point of `pact`. One command, two agents, running at
the same time, not one after another:

```sh
pact spawn-many \
  --task claude:"create a file named alpha.txt containing ALPHA" \
  --task claude:"create a file named beta.txt containing BETA"
```

Output from both agents streams live, each line prefixed `[claude:0]` /
`[claude:1]` so you can tell them apart even interleaved. Swap `claude`
for `copilot` or `codex` (or mix them: `--task copilot:"..."`) if you have
those installed instead.

Every task needs an agent, either via a `<agent>:` prefix on the task
itself or a batch-wide default:

```sh
pact spawn-many --agent claude \
  --task "create a file named alpha.txt containing ALPHA" \
  --task copilot:"create a file named beta.txt containing BETA"
```

Here `alpha.txt` runs on `claude` (the `--agent` default) and `beta.txt`
runs on `copilot` (its explicit prefix overrides the default). At least
one of `--agent` or a per-task prefix is required for every task.

## 5. Check the coordination layer (30 seconds)

While agents are running (or right after), `pact coord-status` shows
what the shared MCP server currently knows -- every active file lease and
each agent's unread message count:

```sh
pact coord-status
```

```
active leases:
  'src/*.ts' held by claude:0 (expires in 118s)
no pending messages
```

This is read-only and purely informational, the same way `pact
conflicts` is: leases are advisory, not enforced, so nothing here blocks
an agent from touching a file another agent has claimed. It's a window
into coordination state, not a lock you need to manage.

## 6. See what happened, then clean up (1 minute)

```sh
pact list                 # both workspaces, with a [dirty]/[clean] indicator
pact diff <id>             # what one workspace actually changed
pact conflicts             # any file touched by more than one workspace
pact teardown <id>         # refuses if there are uncommitted changes you haven't seen yet
pact teardown <id> --force # tear down anyway
```

That last safety behavior is deliberate, not a bug: `teardown` won't
silently discard uncommitted work. If a workspace is dirty, it tells you
exactly what would be lost and asks for `--force` before proceeding.

Once you're happy with what each workspace did, `pact merge-all` folds
every active workspace onto a fresh integration branch instead of tearing
them down individually -- see the main README's Usage section for the
full flag reference.

### A gotcha with `merge-all --union`

`--union <glob>` lets you name files (e.g. a barrel/plugin-registration
file) that are safe to resolve on conflict with a plain line-union merge:
your lines, then any of theirs not already present, *appended at the end
of the file*. That's fine for genuinely append-only content (logs,
CHANGELOG entries), but if the union-mergeable region sits above other
code -- a trailing `module.exports`, a file-final `start()`/`listen()`
call -- only the first workspace's addition lands where you'd expect;
every workspace after that gets appended past the trailing code instead
of inside the intended block. The file still parses, but the structure
is easy to be surprised by on first encounter. If you're using `--union`
on a barrel file, keep any finalization step in a separate file agents
don't touch, so there's nothing below the union-mergeable region to land
after.

## What just happened

Each `spawn`/`spawn-many` task got its own `git worktree` (so agents never
step on each other's uncommitted changes), a best-effort shared dependency
install (a second workspace with the same lockfile reuses the first
instead of reinstalling), and a coordination MCP server giving every agent
`claim_files`/`send_message`/`check_messages` tools automatically, no
setup needed. None of that required any configuration -- it's what
`pact spawn`/`spawn-many` do by default.

## Next steps

- The main [README](README.md) has the full command reference, every
  design decision (and why), and what's been verified against real
  installed agent CLIs vs. what hasn't.
- [CONTRIBUTING.md](CONTRIBUTING.md) if you want to add an adapter for
  another agent CLI or a package-manager detector.
