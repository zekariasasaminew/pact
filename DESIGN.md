# pact — design notes

This document holds the *why* behind non-obvious decisions in pact's source:
empirical findings from manual testing, trial-report-driven fixes, tradeoffs
considered and rejected, and anything confirmed by hand rather than just
reasoned about. It exists so the code itself can stay comment-light — naming
and structure carry the *what*, this document carries the *why* and the
history. See `CLAUDE.md` for the convention this follows going forward.

Organized by crate, roughly in dependency order (`pact-vcs`/`pact-agents`/
`pact-coord`/`pact-deps` first, since `pact-core` and `pact-cli` build on
them).

## pact-vcs — git worktree lifecycle, merge-all

### PidLock origin

Originally built because git itself races on `.git/config.lock` when `git
worktree add`/`remove` run concurrently (see
anthropics/claude-code#34645) -- but the mechanism isn't git-specific,
it's just "serialize access to a resource, and don't leave it stuck locked
forever if the holder died." `pact-deps` reuses it verbatim to guard
concurrent population of a shared dependency store entry.

### Workspace lifecycle

`create_workspace` captures `base_commit` (`git rev-parse HEAD`) under the
same `PidLock` as the `git worktree add` call immediately after it, so
it's exactly the commit the new branch forks from -- not a value that
could race against a concurrent `pact spawn` moving HEAD in between the
two calls.

`workspace_diff` and `workspace_changes` both compute the merge-base
against the *repo root's* current HEAD, not a persisted value -- correct
as long as the repo's own branch hasn't been reset past the point the
workspace's branch forked from, the same assumption `git worktree`/`git
worktree remove` themselves make about a branch's relationship to its
origin. `workspace_changes` specifically exists to detect cross-workspace
file overlap (issue #8): two workspaces sharing the same merge-base forked
from a comparable point in history, so any file both of them touched is
worth surfacing, without needing semantic/AST-level analysis -- file-path
overlap is the same restriction the MCP lease layer already accepts.

### Workspace teardown

`remove_workspace` deletes a workspace's worktree and, unless
`keep_branch` is set, its `pact/<id>` branch. Confirmed via a real trial
run (an outside reviewer's report): `git worktree remove` does not delete
the branch it was created with -- that's standard git behavior, worktree
removal and branch deletion are independent -- so without this, every
torn-down workspace left a dead branch behind, accumulating over repeated
use. Force-deletes (`-D`, not `-d`) since an agent's throwaway workspace
branch is very often unmerged; `keep_branch` exists for anyone who wants
to inspect or rebase a workspace's commits after tearing it down.

It refuses on a workspace with uncommitted changes unless `force` is set.
This wasn't a real check before -- confirmed directly, by spawning a
workspace, adding an uncommitted file to it, and running the old
unconditional-`--force` teardown: the file was silently gone afterward,
with no warning at all. The underlying `git worktree remove` call already
has this exact protection built in (it refuses on a dirty worktree unless
*it's* passed `--force`); `remove_worktree_retrying` was defeating that
protection unconditionally on every call. This check restores it at
pact's own layer instead, so `--force` is something the caller chooses,
not something baked in silently.

`remove_worktree_retrying` tolerates two Windows failure modes confirmed
against a real killed agent process, not theoretical: (1) killing a
process doesn't mean its handles on its own `current_dir` are released the
instant `kill()` returns, so an immediate `git worktree remove` can fail
with "Permission denied" even though the process is already gone --
retrying briefly usually clears this; (2) git unregisters a worktree from
its own metadata *before* attempting to delete the directory, so if that
deletion fails, a later `git worktree remove` on the same path fails with
"is not a working tree" even though the directory (and whatever's in it)
is still sitting there orphaned. In that case it falls back to removing
the directory directly, also with retries, since it's the same underlying
handle-release race, just past the point where git itself can help.

### commit_all

Stages and commits everything in a workspace's working tree (staged,
unstaged, untracked) with a message derived from its task text, so `pact
diff`/`pact log` and `merge-all` always have a real commit to work with
instead of a permanently-dirty worktree -- see the trial report that
motivated this: every workspace in the trial ended `[dirty]` with nothing
to merge. `merge-all`'s first phase calls this unconditionally on every
selected workspace, which is why it's a no-op returning `Ok(false)` rather
than an error on an already-clean workspace.

`commit_message` builds the commit subject as `agent <id>: <first line of
task>`, matching the existing `pact/<id>` branch-naming convention so a
commit is traceable back to its workspace at a glance. The subject line is
capped around 72 chars (git convention); if the task is longer or spans
multiple lines, the full untruncated text follows in the commit body --
this is asserted directly by `commit_message`'s own unit tests, which is
the more reliable place to see the exact contract than a comment.

### merge_all

Closes the loop from "N workspaces are dirty" to "one clean integration
branch" -- see the trial report this is built against: 9 of 10 manual
merges failed on a shared barrel file, and strict-mode git blocked every
merge after the first conflict. Never touches the repo's own checkout --
everything happens in a throwaway worktree, same isolation model as agent
workspaces themselves, so this is safe to run regardless of what branch
(or branch-protection rules) the main checkout has.

Phases, all best-effort (one workspace's failure never blocks another's):

1. Auto-commit every selected workspace via `commit_all`.
2. Moving-base check -- refuse a workspace whose recorded `base_commit` is
   no longer an ancestor of current HEAD, so merging never silently
   assumes a fork point that isn't real anymore (e.g. HEAD was reset since
   the workspace was created). A workspace whose changes can't be sized in
   the next phase (e.g. `workspace_changes` failed) sorts last rather than
   being dropped, so a bug in sizing never silently excludes it.
3. Sequence the rest smallest-changeset-first, on the theory that landing
   small compatible changes before a large one reduces cascade conflicts.
4. Merge each into a fresh `target_branch` (default `pact/merged-<id>`)
   one at a time, skipping (not aborting the whole run on) a real
   conflict.

`dry_run` runs phases 1-3 (auto-commit still happens, since that's always
safe to call) but stops before touching git state for the actual merge,
returning the planned order instead.

`is_ancestor`'s `git merge-base --is-ancestor` exits non-zero for "not an
ancestor", which is a normal, expected outcome here, not a spawn/IO
failure -- so it returns `Ok(false)` for that case rather than treating a
non-zero exit as an error.

`pact merge-all`'s process exit code (issue #27) distinguishes three
outcomes: `0` every workspace merged, `2` one or more were skipped (a real
conflict, or the moving-base check) but nothing errored outright, `1`
reserved for a hard/unexpected failure. It used to always exit `1` on any
skip, so a CI wrapper around `merge-all` had no way to tell "half the work
landed, the rest needs a human" apart from a crash -- both looked
identical at the process level.

**Test scenario notes** (`crates/pact-vcs/tests/merge_all.rs`): the main
conflict test has workspace A append a new line at the end of `index.ts`,
well-separated (4 lines of untouched context) from anything C/D touch;
workspace B edits a completely different file. Both are genuinely
compatible with everything else and must always merge, regardless of
order. C and D both rewrite `index.ts`'s *first* line differently -- a
real, unavoidable conflict between exactly those two, confirmed by hand
against real git before writing the test: single-line-file appends turned
out to conflict far more readily than multi-line context does (see the
trial report this whole feature is built against). Since C and D touch the
same single file, they tie on the smallest-changeset-first heuristic, so
which one merges first (and therefore which one the *other* conflicts
against) isn't specified -- the test asserts that exactly one of them
merged, not which one.

### Semantic auto-resolution

`merge_branch_into` tries a plain `git merge` first. On a real conflict, it
tries the semantic-narrow auto-resolution rules in `try_auto_resolve` on
each conflicted file before giving up: never touch a generated/structural
file (`NEVER_AUTO_RESOLVE` -- lockfiles and similar, where a naive
line-level merge is very likely to silently produce a corrupt result, so a
real conflict there always stays a real conflict for a human or a
regenerate step); JSON-aware merge for `package.json`'s dependency blocks
(`PACKAGE_JSON_DEP_KEYS` -- the only part of the file this touches; a
conflict anywhere else in the file, e.g. scripts/version/name, is left as
a real conflict); a plain line-union merge for anything matching a
caller-supplied `--union` glob (nothing is union-merged unless the caller
explicitly named it -- pact does not guess which files are safe to blindly
concatenate). If *every* conflicted file resolves, the merge completes
with a commit instead of aborting. If any file is left over, the whole
merge is aborted (so the worktree is clean for the *next* workspace's
attempt -- one conflicted workspace must not poison the rest of the batch)
and reported as a real conflict, same as if none of this existed.

`try_resolve_package_json`: a dependency name added or changed on exactly
one side is taken as-is; changed to the *same* value on both sides is
fine; changed to *different* values on both sides is a real conflict this
does not try to guess at -- returns `Ok(None)` for the whole file in that
case, same as if anything *outside* the dependency keys differs between
the two sides. Re-serializing used to alphabetize every top-level key
(`serde_json::Value` is `BTreeMap`-backed without the `preserve_order`
feature), so a file that led with `name`/`version`/`description` came
back leading with `dependencies` -- fixed (issue #29) by enabling
`preserve_order` workspace-wide (`merged_obj` is built by cloning `ours`'s
object and updating in place, so key order already matches "ours" once
the map itself preserves insertion order) and by sniffing the input's own
indent width (`detect_json_indent`) instead of hardcoding 2 spaces.

`try_resolve_union`: the result is "ours" lines, in order, followed by any
of "theirs" lines not already present verbatim -- the same semantics as
git's own `merge=union` attribute driver, just applied here in Rust rather
than by mutating the repo's shared (cross-worktree)
`.gitattributes`/config to register a driver. Appropriate only for
genuinely order-independent, append-only content (barrel exports,
changelog entries).

This naive line-concat is exactly wrong for anything with "final
assignment/declaration wins" semantics, and it shipped that way: a real
Windows shakedown (issue #24) had two agents each append a disjoint export
to the same CommonJS barrel, and the union merge produced two
`module.exports =` statements (second silently wins, first is dropped)
plus, in the accompanying test file, two `const { ... } = require(...)`
declarations binding the same names -- a real `SyntaxError`. `merge-all
--union` reported `exit 0`/`auto-resolved` on output that either silently
broke or didn't even parse. `union_merge_is_safe` now runs a cheap
heuristic (not a real parser, deliberately -- no external tool dependency
for the 3-platform CI matrix to install) on JS/TS-extension output before
trusting it: rejects the result if it would contain two `module.exports
=`/`export default` statements, or two declarations binding the same
identifier in the same scope. On rejection the file falls through to a
real conflict instead of a false "auto-resolved". Non-JS/TS files and
other legitimate `--union` uses (logs, CHANGELOG, ignore files) are
unaffected -- false negatives are accepted by design (this is cheap, not
exhaustive); a false positive just means a file that would otherwise
silently break instead needs a human, which is the safe direction.

`read_conflict_stage` reads one side of a conflicted file from git's index
-- stage 1 is the common ancestor, 2 is "ours" (the integration branch,
before this merge), 3 is "theirs" (the branch being merged in). `Ok(None)`
if that stage doesn't exist for this path (e.g. the file was added fresh
on only one side) is treated as "don't understand this shape well enough
to auto-resolve," not an error.

### Arbiter resolver hook

`ArbiterResolver` is a hook `merge_all`'s caller can supply to attempt
further resolution of files the mechanical/semantic auto-resolution
couldn't handle. Deliberately a plain closure, not a concrete type:
`pact-vcs` has no dependency on `pact-agents` and shouldn't need one just
to leave a slot for "maybe spawn an AI agent here" -- the caller
(`pact-core`, which does depend on `pact-agents`) builds the actual
agent-invoking closure and is entirely responsible for what "resolved"
means, including any verification (e.g. running a test command) before it
reports a file as resolved. `pact-vcs` treats anything not in the returned
list as still conflicted and aborts the merge exactly as if this hook
didn't exist.

## pact-core — Orchestrator

### spawn / spawn_many concurrency

A separate, explicit `safety_override` per task in a `spawn_many` batch
(rather than one shared across the whole batch) is deliberately not
supported yet -- issue #3's acceptance criteria don't call for it, and
`--safety`'s existing single-spawn meaning (an adapter-vocabulary
override) already applies uniformly per invocation; extending it per-task
is a plausible follow-up, not something to speculatively build now.

`spawn_many` shares one `Supervisor` across N concurrent `std::thread`
calls so a single Ctrl-C kills every still-running child at once.
`workspaces: &WorkspaceManager` (via `self`) has no interior mutability
beyond what `create_workspace` already serializes with `PidLock` -- the
same concurrency Phase 0 verified against 6 simultaneous `spawn` calls --
so sharing `&self` across scoped threads doesn't need any new
synchronization of its own. Index and agent are captured outside each
task's closure return value specifically so a panic (which loses whatever
the closure would have returned) still leaves enough to attribute the
failure to the right task afterward.

### Coordination config wiring

`coord_config` builds the adapter-agnostic description of the
coordination server for the agent CLI to launch. What each adapter *does*
with this (a JSON file passed via a flag, or inline config overrides) is
up to it -- see `pact-agents::AgentAdapter::build_command`.
`coord_override`, if given (see `CoordServerOverride`, issue #10), points
at an alternative command/args instead of `pact mcp-serve` -- pact does no
protocol translation, it just tells the agent CLI to launch something else
instead of itself.

`coord_warning` (issue #28) decides whether to warn about the coord
connection based on the *last* `CoordStatus` reported over the whole run,
not the first. A real connection reliably goes through a transient
`pending` status before `connected` within a fraction of a second -- every
single spawn hit the old immediate-warn-on-any-non-connected-status logic,
even though the very next log line was `connected`. That trained users to
ignore pact WARNs, making the genuinely bad case (stuck on `pending`, or
`failed`, for the whole run -- e.g. the agent process dying before it
connects) read almost identically to normal. Extracted as a pure function
since `spawn_with_supervisor` itself spawns a real process and can't be
unit tested directly per this repo's testing conventions.

### Weaver — task overlap prediction

`PredictedOverlap`/`predict_task_overlap`: pure text analysis, no agent
spawned, run *before* anything is spawned at all, on the theory that
decomposition-time prevention is cheaper and more reliable than any amount
of post-hoc merge cleverness -- this is a heuristic prediction, not a
guarantee: it never blocks `spawn_many`, it only gives the caller
something to warn about (same "informational, nothing here blocks
anything" posture `Orchestrator::detect_conflicts` already established for
git-level overlap).

`predict_task_overlap` scans every task's text for file-path-like tokens
and reports any token mentioned by two or more tasks -- e.g. 5 of 10 tasks
each saying "export it from `src/index.ts`" predicts exactly the conflict
the pact v0.2 trial report hit. Deliberately conservative about false
negatives, not false positives: missing a real overlap just means this
specific prediction isn't caught (no worse than not running this at all),
while an occasional false-positive token (e.g. "next.js" read as a file)
costs nothing worse than one harmless extra line in a warning.

`looks_like_file_path` is a conservative, regex-free check: ends in a
short alphanumeric extension after the last `.`, with a non-empty stem
made of path-ish characters. Not a real path grammar -- see the false
positive/negative tradeoff above for why that's acceptable here.

### Arbiter — agent invocation

`ArbiterConfig` is the "verified" half of pact's conflict story: a
one-shot headless agent proposes a resolution for a file the
mechanical/semantic auto-resolution in `merge_all` couldn't handle, but
that resolution is only ever accepted if `test_cmd` then passes in the
same worktree. Entirely opt-in -- `Orchestrator::merge_all` with `arbiter:
None` never spawns an extra agent or spends anything beyond what
`spawn_many` already would. `test_cmd` is a shell command run (`cmd /C` on
Windows, `sh -c` elsewhere) in the worktree after the agent finishes; a
non-zero exit means the resolution is rejected and the merge falls back to
a reported conflict exactly as if Arbiter hadn't run. There is
deliberately no "skip verification if no test command is configured"
path: a resolution nothing verified isn't something `merge_all` will
accept.

`Orchestrator::merge_all` wires `arbiter` in as pact-vcs's
`ArbiterResolver` hook -- pact-vcs itself has no dependency on
`pact-agents`, so this is the one place that bridges "a file
mechanical/semantic resolution couldn't handle" to "actually spawn an
agent to look at it."

`run_arbiter` gives a one-shot headless agent the conflicting file(s)
(git's own `<<<<<<<`/`=======`/`>>>>>>>` markers still in place) and the
conflicting workspace's task text, asking it to resolve them in place. The
result is accepted only if (a) no conflict markers remain, (b) the files
stage cleanly, and (c) `config.test_cmd` then exits successfully in the
same worktree -- any failure at any step returns an empty list, and the
caller (pact-vcs) aborts the whole merge attempt exactly as if this were
never called. The agent's own reported success isn't trusted on its own --
conflict markers left behind mean it didn't actually finish, no matter
what it said.

`build_arbiter_prompt` gives the agent the conflicting workspace's own
task, the exact files it's being asked to edit (and nothing else), and an
explicit instruction not to run `git` itself -- pact stages and verifies
the result afterward, not the agent.

## pact-agents — adapters and process supervision

### AgentEvent normalization

`AgentEvent` is shared across every adapter (Claude Code, Copilot CLI,
Codex, Gemini), even though each CLI's actual output schema is different --
each adapter's own `parse_line` maps its specific shape onto this enum.
`Other` is a catch-all for anything not explicitly modeled, but it's still
surfaced to callers, never silently dropped: an unrecognized event is far
more likely to be a real message an adapter hasn't been taught about yet
than something safe to ignore.

`CoordStatus` is a separate variant, not bundled into `Init`, because Claude
Code reports every MCP server's status inside its one init event, but
Copilot CLI reports them as their own standalone events, and a line can
report several servers at once. Each adapter's `parse_line` emits zero or
more `CoordStatus` events per line as its own schema demands; the
connectivity check that consumes them (`pact-core`) doesn't need to know
which shape produced them.

### Process group kill

`Supervisor` (below) covers the Ctrl-C path; `kill_if_alive` in `pact-vcs`
(used by `teardown`) covers killing a specific workspace's agent process on
demand -- both need to reach an agent's *whole* process tree, not just the
directly-spawned PID, since a Bash tool call spawns a child shell that a
plain `Child::kill()` leaves running. On Windows, `taskkill /F /T /PID`
terminates the full descendant tree in one call. On Unix,
`pact_agents::run_and_stream` spawns every agent process via
`command_group`'s `group_spawn` (`process_group(0)`), making the child its
own process group leader, so its pgid equals its pid -- meaning the
already-recorded pid alone is enough to kill the whole group via
`kill(-pid, SIGKILL)`, without needing to persist a separate pgid. The Unix
path is implemented from documented POSIX process-group semantics and
`command_group`'s own source, and is CI-verified on real Linux/macOS
runners (`crates/pact-agents/tests/group_kill.rs`, issue #6) -- but a real
agent's own process tree on real Unix hardware remains unconfirmed, since
this project's primary dev environment is Windows.

The Unix test spawns `sh -c "sleep 60 & wait"` as the parent (direct child
of the test process), with the backgrounded `sleep 60` as the grandchild
whose survival is what's actually being checked; it counts survivors with
`pgrep -f "sleep 60"` (matching the full command line, so it finds that
specific backgrounded process, not unrelated `sleep` calls on a shared CI
runner).

### Supervisor

`Supervisor` tracks every live child process group across however many
concurrent `run_and_stream` calls share it, so one process-wide Ctrl-C
handler can kill all of them (registering the whole group, not just the
tracked child -- see "Process group kill" above for why) instead of the
single-shot, one-child assumption `run_and_stream`'s old self-installed
handler made. Single-`spawn` and `spawn-many` both go through a
`Supervisor` now: `spawn` just creates its own with exactly one registrant
for the duration of that one call, so its observable behavior (one
handler, killing one child, installed and torn down within a single
`run_and_stream` call) is unchanged; only the mechanism moved from a bare
function into this small object so `spawn-many` can share one across N
threads.

The Ctrl-C handler recovers a poisoned mutex guard (`unwrap_or_else(|p|
p.into_inner())`) rather than bailing out of the handler: a prior panic
while holding the lock (e.g. inside another thread's own cleanup) must not
make every other live child unkillable on Ctrl-C. A failure to install the
handler at all (e.g. an outer caller already installed one) is logged, not
fatal -- the agent process(es) just won't be killed on Ctrl-C in that case.

### run_and_stream

Every raw stdout line is appended to `log_path` as-is (not the
re-serialized `AgentEvent`) so schema drift or fields the parser doesn't
know about yet aren't lost -- then parsed and handed to `on_event`.
`on_pid` is called once, immediately after spawning, so the caller can
persist the PID before this function blocks -- that's what lets a
`teardown` invoked from a different process find and kill a still-running
agent.

stderr is drained on its own thread into the same log file (prefixed
`[stderr] `) rather than left inherited or piped-but-undrained -- either
of those risks interleaved garbage in the terminal or a full-pipe deadlock
if the child writes enough of it.

`parse_line` is adapter-supplied and returns zero or more events for one
raw line, not exactly one, because not every adapter's schema maps one
line to one event: confirmed necessary for Copilot CLI, whose
`assistant.message` events can carry both response text and one or more
tool calls in the same line. Claude Code's schema happens to be
one-event-per-line, but this function doesn't assume that of anyone.

Not every adapter emits an explicit `Result`-shaped event -- Codex's
`turn.completed`, confirmed directly, carries no success/failure signal at
all, so it never produces one. Falling back to `success: false`
unconditionally when none was seen would misreport every successful Codex
run as a failure; the process's own exit code is the honest fallback
signal instead.

### MCP config format confirmation

`write_mcp_json_config`'s JSON shape was confirmed to work for both Claude
Code's `--mcp-config` and Copilot CLI's `--additional-mcp-config @<path>`
by deliberately pointing both real CLIs at a broken command and observing
a loud, non-silent failure -- not just inferred from documentation.

### Adapter-specific quirks

### Claude Code safety default

`ClaudeCodeAdapter`'s default is `--allowedTools` (a curated safe-operation
allowlist covering file read/write/edit/search plus the VCS and
package-manager commands a coding task actually needs), not
`bypassPermissions` -- confirmed directly that an explicit `--allowedTools`
list makes Claude Code deny an out-of-scope tool call cleanly and
immediately in headless mode, rather than hang waiting for an approval
prompt no TTY can answer. `bypassPermissions` alone was the *documented*
fix for the hang; this is a real, verified safer alternative that isn't
all-or-nothing. The allowlist (`DEFAULT_ALLOWED_TOOLS`) isn't
user-configurable yet (see the README's Known limitations) -- the point
for now is proving the mechanism is genuinely safer than the old
bypass-everything default, not claiming this exact list is final.

`--allowedTools` is always passed, harmless alongside an explicit
`--permission-mode` override too (including `bypassPermissions` itself).
`safety_override`, when given, is passed as a raw `--permission-mode`
value; when absent, no `--permission-mode` flag is passed at all --
confirmed that Claude Code's own baseline default mode, combined with the
allowlist, is what produces the clean-deny-not-hang behavior this default
relies on. The MCP config is rendered to a `{"mcpServers": {...}}` JSON
file and passed via `--mcp-config` -- confirmed against the real CLI: a
malformed config is rejected with a loud error before the session starts,
so getting the file wrong is never a silent no-op.

### Claude Code output schema

`parse_line` is modeled directly against real output captured from
`claude -p --output-format stream-json --verbose` (see README), not
secondhand docs. One event in, one event out in every case observed so
far, but it returns a `Vec` to match the shared `AgentAdapter` interface
other adapters need.

`parse_assistant` reports the first recognized content block (text or
tool_use) rather than collecting all of them into a `Vec`, since in
practice Claude Code emits one block per line in stream-json mode.
Anything genuinely mixed falls back to `Other` with the full message
preserved.

### Codex adapter

`CodexAdapter` is live-verified against a real installed `codex`
(codex-cli 0.144.3) -- this was NOT true when the adapter was first
written (built from OpenAI's docs alone, on a machine without Codex
installed), and the docs turned out to be wrong on the exact safety flag.
Fixed and confirmed end-to-end, including a real MCP tool call through
this project's own coordination server, not just a bare launch.

**Safety flag**: the docs described a separate `--ask-for-approval` flag
with `never`/`on-request`/`untrusted` values -- that flag does not exist in
`codex exec --help` for the installed version. What actually works,
confirmed directly: `--sandbox workspace-write` alone still refuses to
write files in non-interactive mode (the agent reports back "approvals are
disabled" and gives up rather than hanging -- a good failure mode, but not
a working one). The only flag that produces a real, completed file write
is `--dangerously-bypass-approvals-and-sandbox`, which -- true to its name
-- skips both approval prompts and sandboxing in one flag, rather than two
independent axes as the docs implied. `safety_override`, if given, is
treated as a `--sandbox` value (`read-only`/`workspace-write`/
`danger-full-access`) instead of the bypass flag -- confirmed that a plain
sandbox mode without the bypass flag still won't let the agent actually
change anything in headless mode, so this is mainly useful for a
deliberately read-only/inspect-only run, not a safer "still gets work
done" middle ground the way Claude Code's `acceptEdits` is.

**MCP config**: passed via inline `-c mcp_servers.<id>.command=`/`-c
mcp_servers.<id>.args=` overrides (confirmed working end-to-end: a real
`claim_files` call through this project's own coordination server returned
the correct JSON) rather than `$CODEX_HOME/config.toml` -- that file also
holds Codex's auth/session state, not just config, so pointing
`CODEX_HOME` at a per-workspace directory would plausibly break headless
login.

**Output schema**: modeled against real output captured from `codex exec
--json` (see README), not secondhand docs -- including a real
tool-call-forcing task and a real MCP tool call, the same standard as the
Claude Code and Copilot CLI adapters. One real gap: unlike Claude Code's
`result.is_error` or Copilot's `result.exitCode`, Codex's `turn.completed`
event carries no success/failure signal at all -- a turn can "complete"
whether or not the requested task actually happened (confirmed: a
file-write task under a sandbox mode that refused the write still produced
a normal `turn.completed`). So this adapter never emits
`AgentEvent::Result` itself; success is determined from the process's
actual exit code instead (see "run_and_stream" above -- this finding is
why that fallback no longer assumes failure by default).

### Gemini adapter

`GeminiAdapter` is built from a real installed `gemini` CLI
(`@google/gemini-cli` 0.50.0, confirmed via `--help` and by actually
running `gemini mcp add` and inspecting the file it wrote), but **not
live-verified against a real authenticated session** -- this environment
has no Gemini API key or Google Cloud auth configured, and `gemini -p
"..."` fails immediately with "Please set an Auth method...". That means
the streaming JSON event schema is inferred from the CLI's own naming
conventions, not captured from real output the way every other adapter's
schema was -- treat it the same way this project treated Codex before it
was installed: real until proven otherwise, not real because it compiles.
See issue #9.

**Safety default**: no confirmed non-hanging alternative exists for this
adapter (unlike Claude Code) -- whether `--approval-mode default` denies
cleanly or hangs in headless mode couldn't be tested without real auth.
`yolo` (auto-accept everything) is the only thing that can be stated with
confidence won't hang, so -- same honest category as Copilot CLI and Codex
-- that's the default, not claimed as a verified safer option.
`safety_override`, if given, is passed as a raw `--approval-mode` value
(`default`/`auto_edit`/`yolo`/`plan`, confirmed from `gemini --help`).

**MCP config**: the one genuinely different mechanism among all four
adapters. Confirmed directly (by running `gemini mcp add --scope project`
and reading the file it produced) that Gemini CLI reads
`.gemini/settings.json`, relative to its *own working directory*,
automatically -- no CLI flag hands it over at all, unlike Claude
Code/Copilot CLI's `--mcp-config`/`--additional-mcp-config` or Codex's
inline `-c` overrides. The file's shape is identical to Claude Code and
Copilot CLI's `{"mcpServers": {...}}` (confirmed: the same
`write_mcp_json_config` helper works unchanged), just written to a fixed
path under `workspace_path` instead of wherever `coord.config_path` says.
No flag is needed to point Gemini at it -- it reads `.gemini/settings.json`
from its cwd automatically, which `run_and_stream` already sets to
`workspace_path` for every adapter.

**Output schema**: modeled on the shape common to the other three
streaming-NDJSON adapters (an init/session event, assistant text,
tool-call events, a final result), using field names guessed from Gemini
CLI's own vocabulary (`-o stream-json`'s wrapper type is unknown, so this
guesses a flat `{"type": ...}` shape like Claude Code's and Codex's).
Deliberately defensive: any line that doesn't parse as JSON, or whose
"type" isn't one of these guesses, surfaces as `Other` rather than being
silently dropped -- exactly because this schema is unverified and *will*
need correcting once run against a real session.

### Copilot CLI safety default

Unlike Claude Code, no confirmed non-hanging alternative to
`--allow-all-tools` exists yet (see the README and issue #2's
investigation): `--allow-tool` works for in-scope actions, but a task
needing a tool outside that list hangs (confirmed directly, 50s/zero
output) rather than denying cleanly the way Claude Code's `--allowedTools`
does. Until that's investigated further, `--allow-all-tools` stays the
only working default -- stated plainly in
`default_safety_description` rather than implying parity with Claude
Code's safer one. It also has no gradient (unlike Claude Code's six
permission modes): Copilot CLI's own `--help` states it's "required for
non-interactive mode", so `build_command`'s `safety_override` parameter
has nothing meaningful to override here.

### Copilot CLI output schema

`copilot.rs`'s `parse_line` is modeled directly against real output
captured from `copilot -p ... --output-format json` (see README),
including a real tool-call-forcing task to confirm `toolRequests`' field
names -- `name`/`arguments`, not Claude Code's `name`/`input`. Unlike
Claude Code (one content block per line), Copilot CLI can bundle response
text *and* one or more tool calls into a single `assistant.message` event
-- confirmed directly: a file-writing task produced one line with
non-empty `content` alongside a non-empty `toolRequests` array. Returning
a `Vec` from `parse_line` is what makes that safe to represent without
dropping either half. The MCP config passed via `--additional-mcp-config
@<path>` is the same `{"mcpServers": {...}}` shape Claude Code uses
(confirmed identical) -- the `@` prefix means "load from file" per Copilot
CLI's own docs; without it the argument would be parsed as an inline JSON
string instead.

## pact-coord — MCP coordination server

Advisory, glob-based, TTL-expiring file leases plus a threaded message log
between agents -- not enforcement, and deliberately not deep semantic
dependency analysis (see the README). Runs as its own process (`pact
mcp-serve`, launched by the agent CLI itself over stdio, not run in-process
by the orchestrator) speaking MCP via `rmcp`, backed by a SQLite database
shared across every agent in one repo's session.

### Database placement

The coordination database is *not* placed under `.pact-<repo>/` alongside
per-workspace bookkeeping (locks, metadata, logs). Those are
blast-radius-limited to the one agent whose workspace they belong to; this
database is depended on by *every* agent in the session. That directory
sits directly inside the same tree as each workspace (e.g.
`workspaces/<id>/../../state.db` is a trivially short relative path), and
headless launches default to `bypassPermissions`, so a careless broad shell
command in any one workspace could reach and corrupt state every other
agent depends on. Placing it under the platform's local data directory,
keyed by a hash of the repo root, isn't a hard security boundary (an
agent's Bash tool can still reach anywhere given an absolute or crafted
path) but removes it from being stumbled into by accident via
`../..`-style relative paths, which is the realistic risk.

### WAL mode

WAL is needed because the coordination database is opened concurrently by
a separate OS process per running agent (each `pact mcp-serve` is its own
process), not just separate threads in one process. `busy_timeout` means a
writer under real contention blocks briefly instead of immediately erroring
with `SQLITE_BUSY` -- prior art's "40-50 concurrent agents" claim implies
that contention is the normal case, not an edge case.

### Per-agent read cursors

A cursor per agent (rather than a shared `read_at` column on the message
itself) is what makes broadcasts work correctly: each recipient needs to
see a message once independently of whether other recipients have already
seen it, which a single mutable "read" flag on the row can't represent.

`check_messages` also excludes the caller's own broadcasts (issue #25) --
the original query only filtered `to_agent` against the caller, so a
broadcast (`to_agent IS NULL`) was never checked against `from_agent`,
meaning an agent got its own broadcasts echoed straight back. Real-world
effect: an agent polling `check_messages` in a loop and reacting to
broadcasts (the idiomatic pattern here) would react to its own, doubling
work or looping. The cursor advances over every recipient-matching row,
including the caller's own broadcasts, not just the ones actually
returned -- otherwise an agent that only ever broadcasts would never
advance past id 0 and would rescan the full `messages` table on every call.

### Lease system

`claim_files` is advisory, not enforced -- the response field is `accepted`
(not `granted`, renamed in issue #36), always `true`, alongside a
`has_conflicts` boolean and the `conflicts` array itself. `granted: true`
was the original name and was found to be actively misleading: an agent
LLM reading `{granted: true, conflicts: [...]}` is very likely to proceed
as though it holds the file exclusively, when the claim is recorded either
way regardless of what `conflicts` contains.

Two correctness gaps found via direct testing, both fixed:
- **No dedup (issue #31).** `claim_files` used to insert a fresh row on
  every call, even an identical repeat from the same holder -- confirmed
  at 8-agent stress-test scale (160 rows for what should have been at most
  8). Fixed via `ON CONFLICT(holder, pattern) DO UPDATE`, keyed on a
  `leases_holder_pattern` unique index added by a one-time migration in
  `db::open` that first collapses any pre-existing duplicates on an
  already-on-disk database (from before the index existed), so opening an
  older database doesn't fail outright.
- **`release_files` was exact-string-match only (issue #32).** Claiming
  `src/add.js` then releasing `src/*.js` returned "released 0 lease(s)".
  Now matches either an exact pattern-string match (kept as a fallback for
  a lease whose claimed files have since been deleted from disk, where
  glob expansion alone can't find anything to overlap against) or a real
  glob-overlap match against actual files on disk, the same expand-and-
  intersect approach `claim_files` already uses for conflict detection.

`ttl_seconds` is bounded (0, 24h] (issue #30) -- unvalidated before, a
negative TTL silently produced an already-expired lease and an unbounded
one produced an `expires_at` centuries out, both misleadingly returning
`accepted: true` either way.

## pact-deps — dependency materialization

Detects a workspace's package manager(s) and makes sure dependencies are
ready before the agent's first real command runs. Most ecosystems (pnpm,
yarn, uv, poetry, pipenv, Cargo, Go modules, Maven, Gradle) already have a
good global shared cache, so those just get their normal install/fetch
command run (`passthrough`). npm (flat, per-project `node_modules`, no
built-in sharing) is routed through a lockfile-hash-keyed content store
instead (`store`), materialized via reflink or read-only hardlink so a
second-plus workspace doesn't pay for a full reinstall. Plain pip/venv is
intentionally left as passthrough-only (see "Passthrough caching strategy"
below).

### The Windows MAX_PATH failure (issue #7)

A real failure mode found while verifying issue #7's fallback path, not a
synthetic test case: the store's key (platform/arch/libc/node/npm version
plus a 64-character lockfile hash) makes store-entry paths meaningfully
longer than a plain per-workspace `node_modules` would be. Confirmed
directly on Windows: `npm ci` populating a store entry for a package with a
postinstall step (`esbuild`) failed with `ENOENT` spawning `cmd.exe` -- not
because `cmd.exe` was missing, but because the fully-qualified path to the
file being installed exceeded Windows' legacy `MAX_PATH` (260 chars) once
nested under a long store-key directory name inside an already-long
temp/state-dir root.

`prepare_npm`'s populate-failure fallback (falling back to a plain,
unshared `npm install` for that one workspace) exists exactly for this
class of precondition-not-met failure -- it was hit for real, not
hypothetically, and the fallback (a shorter path) succeeded where store
population didn't. The same fallback also covers other real causes: a
network blip, a native build tool missing on that specific machine, a
registry issue -- none of which should leave a workspace with no
`node_modules` at all.

### Store key components

`platform_key` distinguishes store entries by OS, architecture, libc
flavor (Linux only), Node major version, and npm's own version -- see
issue #7's risk analysis for why each of these, beyond the original
os/arch/node-major set, turned out to matter: npm version because
different npm versions can lay out `node_modules` differently from an
identical lockfile, and libc flavor because packages that resolve a
platform-specific binary via `optionalDependencies` (esbuild, swc, sharp,
and others in that exact shape) pick a *different* one for musl (Alpine)
vs. glibc (Debian/Ubuntu) despite both reporting the same os/arch.

`libc_suffix` detects musl via the presence of a musl dynamic linker
(`ld-musl-*` in `/lib`), which is how musl libc (Alpine's default)
identifies itself; anything else on Linux is assumed glibc. Best-effort:
if detection is inconclusive, "glibc" is the safer assumption (the
overwhelming majority of non-Alpine Linux), not silently omitting the
dimension entirely.

### Windows `.cmd` shim resolution

`cmdutil::run` routes every spawned package-manager command through `cmd /C`
on Windows. npm/pnpm/yarn (and sometimes poetry/pipenv, depending on install
method) ship as `.cmd` shims, not `.exe`. `std::process::Command` doesn't
consult `PATHEXT` the way a real shell does, so `Command::new("npm")` fails
with "program not found" even though `npm` works fine typed interactively.
`cmd /C` restores that resolution; other platforms get a plain, direct spawn.

### Passthrough caching strategy

`passthrough::run` warms the package manager's own global cache instead of
building pact-specific sharing, for ecosystems that already cache well:
pnpm, yarn, uv, poetry, pipenv, Cargo, and Go modules all cache once and
reuse across projects by default, so the only job here is warming that
cache before the agent's first real command. Maven and Gradle need no
command at all -- `~/.m2` and `~/.gradle/caches` populate lazily on any
build invocation, so an explicit fetch step would only add time. A
non-zero exit is logged, not returned as an error: a transient network
failure here shouldn't fail the whole `spawn`, since the agent can still
retry the install itself once it starts working.

Plain pip/venv gets no custom store (a Phase 1 decision): pip already has
its own global download cache (`~/.cache/pip`) shared across projects,
covering the expensive part (network fetch). A hardlink-based store on top
of that would mean hardlinking into freshly created venvs, which risks
embedding absolute paths from the wrong venv (activation scripts, `.pth`
files, console script shebangs) -- a correctness risk, not just extra
engineering, so it's left as future work rather than shipped provisionally.

### ReadOnlyHardlink tradeoff

A hardlink shares the same underlying file record as its content-store
entry, so marking the destination read-only also freezes the canonical
store copy after first use -- intentional, not a side effect to work
around. The tradeoff: a package that writes into its own installed files
after materialization (a native-build step, a binary downloader, a
git-hook installer) fails loudly instead of silently corrupting every
other workspace sharing that store entry. That failure is the point.

### Package manager detection

### No committed lockfile (issue #26)

`prepare_npm`'s no-lockfile path used to run a plain `npm install`, which
writes a fresh `package-lock.json` into the workspace. Confirmed: two
agents on the same lockfile-less repo each independently generated a
different-content lockfile (different `npm install` runs can resolve
semver ranges to different exact versions), and `merge-all`'s conflict
detection then flagged `package-lock.json` as touched-by-multiple-
workspaces on every multi-agent Node run -- even when the two workspaces'
actual task changes touched entirely disjoint source files. Now runs
`npm install --no-package-lock` instead: the agent still gets a working
`node_modules` from the start, but no lockfile is generated, since there's
no stable content across workspaces to converge on in the first place.
The store-population-failure fallback path (where a real committed
lockfile *does* exist) is unaffected -- that install can still update the
existing lockfile in place, exactly as it would outside pact.

## pact-cli — command-line surface

`--help` output for every command comes directly from `///` doc comments
on the `Cli`/`Command` struct and enum definitions in `main.rs` -- those
are user-facing product documentation, not internal narrative, so they're
intentionally kept verbose and are not subject to the comment-reduction
pass the rest of the codebase got.

`mcp-serve` gets its own, self-contained tokio runtime rather than making
the whole CLI async -- it's the only command that needs one (`rmcp`
requires async), and every other command stays exactly as synchronous as
it already is. See the README for why that tradeoff was made deliberately,
not by default.

`print_event_labeled` needs no extra locking beyond what `println!`'s own
internal `Stdout` lock already gives per call: each event becomes one
complete line written in one call, so concurrent threads' (`spawn-many`)
lines interleave at line granularity, never mid-line.

### `--agent`/prefix precedence in `spawn-many` (issue #37)

`spawn` took a top-level `--agent`; `spawn-many` required every `--task`
to embed an `<agent>:...` prefix and had no `--agent` at all -- a
first-time user reasonably tried `--agent copilot --task "..."` and got an
unhelpful clap suggestion (`tip: a similar argument exists: '--safety'`).
`spawn-many` now also accepts `--agent` as a default for any `--task`
without a recognized prefix; a prefix still wins when present (mixing
agents in one batch is the original reason `spawn-many` required prefixes
at all). `parse_task_spec` falls back to the default even when a task's
colon isn't meant as an agent prefix at all (e.g. `--agent copilot --task
"fix the bug: handle empty array"` -- "fix the bug" isn't a real agent
name, so with a default set the whole string is the task text) rather
than surfacing an "unknown agent" error; without a default, that same
input still gets the specific "unknown agent" error, not a generic one.
Chosen over dropping `--agent` from `spawn` and always requiring a prefix
there, since that would break every existing `spawn` caller -- this is
purely additive to `spawn-many`.

### Streamed event filtering (issue #38)

The Copilot CLI adapter recognizes 4 event types and passes everything
else through as raw `[other]` JSON. Confirmed real noise, not a guess: a
single spawn produced 52 `[other]` lines to 1 real `[assistant]` line; a
2-agent `spawn-many` run's log ballooned to 695 KB, almost entirely
`session.background_tasks_changed`. `should_print_other` suppresses a
short, specifically-confirmed list of noisy raw `type` values
(`SUPPRESSED_OTHER_EVENT_TYPES`) from the live terminal view by default;
`--verbose`/`-v` (global flag) restores them. Anything not on that list
still prints unconditionally, same as always -- an unrecognized event is
still more likely to be a real message an adapter doesn't parse in detail
yet than something safe to drop silently, so only confirmed noise is ever
suppressed. Filtering happens at this presentation layer, not by dropping
anything from the normalized `AgentEvent::Other` stream itself, so the
full unfiltered stream is unaffected either way -- `run_and_stream`
already writes every raw line to the workspace's log file before any
filtering happens.

### Workspace commit lifecycle (issue #35)

Neither `spawn` nor `spawn-many` commits anything -- an agent's changes
land in its workspace's working tree, and `pact list` shows it as
`[dirty]` once the agent is done. That's expected, not a sign anything
needs attention, but it was undocumented anywhere (not `spawn --help`,
not `spawn-many --help`, not `merge-all --help`, not the README) until
now documented in all of those. `commit-all` (or `merge-all`, which runs
the same commit step automatically before merging) is what actually
creates a commit. A user checking a workspace's branch with `git log`
before merging, to sanity-check what the agent did, would otherwise see
an empty branch at the same commit it forked from and could reasonably
conclude the agent did nothing.
