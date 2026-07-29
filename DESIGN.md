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

### PID reuse (issue #70)

A PID-only liveness check has a real gap: if the original holder died and
the OS later recycles that PID for an unrelated process before anyone
tries to steal the lock, the check sees a "live" process and refuses to
steal an actually-abandoned lock. Fixed by recording the holder's process
start time (`sysinfo::Process::start_time`, cross-platform) alongside the
PID in the lock file -- a live process whose start time doesn't match the
recorded one is a different process that happens to share the PID, not
the original holder, so the lock is stolen. A lock file written before
this field existed (bare PID) falls back to the old PID-only check rather
than erroring, so it's compatible with a lock held across an upgrade.

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

### Workspace id: task-derived slug + random suffix (issue #122)

`workspace_id` (was a bare `short_id()`, an 8-char random hex string with
no relationship to the task) now derives a readable slug from the task
text and always appends a `short_id()` suffix: `add-pagination-to-users-a1b2c3d4`.
Made `pact list`/branch names (`pact/<id>`) meaningfully more scannable
with real tasks, per an outside adoption/UX review that called the fully
opaque id out as unnecessary friction.

The random suffix is not decoration -- it's the actual collision-avoidance
guarantee, unchanged from before this issue. `slugify` alone is not unique
(two workspaces from identical or similarly-worded task text would slugify
to the same string), so `workspace_id` always appends it regardless of
whether the slug is empty, short, or long.

`slugify` is deliberately conservative:
- ASCII-alphanumeric only, lowercased, everything else collapsed to a
  single `-` (no runs of hyphens, no leading/trailing one).
- Capped at `MAX_SLUG_LEN` (32) -- this id becomes part of every path under
  the workspace (`state_dir/workspaces/<id>/...`), and Windows' MAX_PATH
  has already bitten this codebase once (the npm content store fallback);
  an unbounded slug from a long task description would make that worse,
  not better.
- Non-ASCII task text (most non-Latin scripts) slugifies to an empty
  string -- `workspace_id` falls back to the bare random suffix in that
  case, identical to every workspace's id before this issue existed. Not
  attempting transliteration (e.g. via `unidecode`) was a deliberate scope
  cut, not an oversight -- worth reconsidering only if real non-ASCII usage
  shows up.

`preview_workspace_location` (used by both `create_workspace` and
`spawn_preview`/`--dry-run`) takes `task: &str` now instead of nothing, so
`--dry-run`'s preview uses the exact same id a real spawn would generate --
confirmed by construction, not just by convention, since both call the
same function.

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

### A crashed `pact` orphans its agent process tree (issue #108)

Found during the 2026-07-23 Claude Code stress-testing campaign, then
isolated further afterward: forcibly killing the top-level `pact`
process mid-run does not clean up its child process tree (a real
`cmd.exe` -> `claude.exe` -> `pact mcp-serve` chain on Windows) -- all
three survived as orphans, confirmed via live process inspection.
`pact teardown --force` *does* correctly recover afterward (a real
tree-kill of whatever PID it has recorded, confirmed working), so this
isn't unrecoverable, just not automatic.

**Isolated with zero pact code involved:** a minimal standalone program
using `command-group` directly (spawn a grouped child via
`Command::group_spawn`, do nothing else, get killed externally) showed
the exact same thing -- the grouped child (a plain `ping`) survived a
`taskkill /F` of its parent. `command-group`'s own Windows
implementation does correctly set `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
(confirmed by reading its source directly), which is supposed to
guarantee exactly this cleanup at the OS level. That it didn't fire here
is either a real `command-group`/Windows nested-job-object limitation,
or specific to the fact that this testing happened from inside another
live Claude Code session (whose own process tree may itself already sit
inside a job object pact's new job then nests under) -- not
conclusively separated from pact's own code, since there's no clean way
to test "outside any other job object" from within this environment.

**Fix shipped: visibility, not a claimed cure.** Rather than chase the
exact Windows kernel mechanics further, `pact list` now reports a
workspace's recorded `agent_pid` liveness directly
(`pact_vcs::agent_process_alive`, the same `sysinfo`-based liveness
check `PidLock` already uses, minus the start-time disambiguation --
acceptable here since this is informational display, not lock-stealing
logic a false positive could break). Deliberately not claiming to
distinguish "orphaned" from "legitimately still running" -- pact
structurally can't tell those apart from a PID alone, so it surfaces the
raw fact (running / not running) and lets the user investigate, rather
than guessing at a classification it can't back up.

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

### Test-gated merge (issue #65)

Batty (a nearby competitor) has test gating without N-way merge; pact had
N-way merge without test gating. `merge-all --require-passing-tests <cmd>`
closes that gap. Design decisions, made with the user directly rather
than assumed (a real fork with real tradeoffs, not a default to pick
silently):

**Gating scope: per-workspace, not a single gate on the final integration
branch.** After each workspace merges cleanly (git-level, no conflict),
`merge_all` runs `<cmd>` in the integration worktree right there, before
moving to the next workspace. A failure resets the integration worktree
back to the commit it was at before that one merge (`git reset --hard`,
safe since this worktree is never shared with anything else) and treats
the workspace as skipped -- the exact same "skip and continue" shape
`merge_all` already uses for a real conflict, no new rollback concept
needed. The alternative -- one test run against the fully-merged branch
at the end -- would catch cross-workspace interaction bugs a per-workspace
run can't see, but raises a real, unsolved question this codebase has
never had to answer anywhere else: which of N already-merged workspaces
caused a failure discovered only after all of them landed, and how do you
undo just that one? Deliberately not attempted here; per-workspace
gating ships the well-scoped half of the idea now rather than blocking on
designing rollback-after-the-fact.

**A distinct flag, not a repurposed `--test-cmd`.** Arbiter's existing
`--test-cmd` means "verify an agent-proposed conflict *resolution*
worked" -- a fundamentally different question from "should this
workspace's own clean merge be *allowed to land at all*", even though
both run a test command. Reusing the name would have silently changed
`--test-cmd`'s existing meaning for anyone already using Arbiter.
`--require-passing-tests <cmd>` is the new, separate flag; `--test-cmd`
is untouched.

**Interaction with Arbiter:** if both are given, a workspace that Arbiter
resolves (auto-resolve, `--union`, or Arbiter itself accepting a
conflict resolution) *also* has to pass `--require-passing-tests` before
being accepted -- Arbiter's own `--test-cmd` verifies its proposed
resolution compiles/passes in isolation; `--require-passing-tests` is
this feature's separate, subsequent gate on the merge as a whole, run
after Arbiter's own verification succeeds. The two commands can be the
same string or different ones; nothing requires them to match.

**Cost:** running a real test suite once per accepted workspace multiplies
wall time for a large batch -- already opt-in via the flag (`merge_all`
behaves exactly as before when it's omitted, no extra cost or behavior
change), so this is a cost a caller explicitly chooses. No cheaper
"just check it compiles" tier below "run the full suite" in this first
cut -- `<cmd>` can already be an arbitrarily cheap command (`cargo check`
instead of `cargo test`) if a caller wants that tradeoff, so a separate
tier wasn't necessary to build.

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

As of issue #151, the two resolvers below live in `semantic_resolvers.rs`
behind a `SemanticResolver` trait (`can_handle(path) -> bool`,
`resolve(&ConflictStages) -> Result<Option<ResolvedFile>>`) instead of a
hardcoded `if is_package_json(file) { .. } else if union_globs.matches(..)
{ .. }` in `try_auto_resolve` -- a pure mechanical extraction (behavior,
including every existing test, is unchanged) meant to give a third
resolver (Cargo.toml, pyproject.toml, go.mod, a changelog's append-only
section, ...) somewhere to plug in later without `try_auto_resolve`
growing another arm. `try_auto_resolve` now: (1) picks the first resolver
whose `can_handle` matches the file, (2) reads all three conflict stages
once via the new `read_conflict_stages` (`Ok(None)` if "ours" or "theirs"
is missing; `base` alone may legitimately be absent and is left to each
resolver to require or not), (3) hands them to that resolver's `resolve`.
Deliberately left out of the trait: a `name()` method, since nothing
today consumes it -- easy to add once an actual observability/logging
consumer needs it, not before.

`PackageJsonResolver::resolve` (was `try_resolve_package_json`): a dependency name added or changed on exactly
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

`UnionResolver::resolve` (was `try_resolve_union`): the result is "ours" lines, in order, followed by any
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

### Persisted conflicts (issue #85)

From the same outside strategic-notes review that produced issue #84:
jj (Jujutsu) treats a conflict as a first-class object in the change
graph, resolvable later instead of a terminal error. `merge_all` today
skips a conflicted workspace and moves on (issue #27 made the skip vs.
hard-failure distinction visible at the exit-code level), but the
conflict itself didn't outlive that one `merge_all` call -- `abort_merge`
runs `git merge --abort` in the throwaway integration worktree, which
discards the three-way stage content entirely. What *is* worth
persisting isn't that stage content -- it's fully reconstructible on
demand by re-running the merge, since neither the conflicted workspace's
own branch nor its recorded base commit is ever deleted -- but a durable,
queryable record that the conflict happened and its current status
(open/resolved/abandoned), matching the issue's own "Proposed shape."

**`ConflictedWorkspace`** is the structured subset of `skipped` that was
specifically a real merge conflict (`MergeOutcome::Conflict`), not a
moving-base skip -- `merge_all` pushes to both `skipped` (existing,
freeform `reason` string, unchanged for backward compatibility) and
`conflicted` (new, structured: `id`, `branch`, `target_branch`, `files`)
at the same call site, from the same match arm, so there's no string
parsing anywhere to tell the two skip kinds apart. Only a real conflict
is resumable -- a moving-base skip means the workspace's base is no
longer part of history, which retrying the same merge wouldn't fix.

**`resolve_conflict`** retries a conflicted workspace's branch against
`target_branch` (which the caller already knows, from `ConflictedWorkspace`
or a persisted record built from it -- see DESIGN.md, "pact-coord >
Persisted conflicts / `pact resolve`"). It checks out the *existing*
`target_branch` directly (`git worktree add <path> <branch>`, no `-b`) --
deliberately different from `merge_all`'s own integration worktree, which
always creates a *new* branch. Checking out the real, existing branch
means a successful `merge_branch_into` call inside it commits directly
onto `target_branch`'s own history, with no separate "publish" step
needed the way `merge_all` doesn't need one for its own throwaway
integration branch either. Reuses `merge_branch_into` verbatim rather
than a separate resolve-specific merge implementation, so a retry
(auto-resolve, `--union`, Arbiter) behaves identically to the original
attempt -- there's exactly one merge-conflict-resolution code path in
this codebase, not two that could drift apart. Confirmed by hand against
a real repo, not just reasoned about: a genuine conflict (same line of a
multi-line file edited two ways) correctly stays `StillConflicted` on a
same-state retry, and correctly resolves once the conflicted workspace's
own branch is changed to no longer disagree with `target_branch`'s
current content -- see `crates/pact-vcs/tests/resolve_conflict.rs`.

Explicitly not attempted: two worktrees can't check out the same branch
at once, so `resolve_conflict` would fail loudly (a real `git worktree
add` error, not silent corruption) if `target_branch` somehow already had
a live worktree elsewhere -- not expected in practice, since `merge_all`
always removes its own integration worktree before returning, but worth
naming as the actual failure mode rather than assuming it can't happen.

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

The `spawn-many` CLI warning built from `predict_task_overlap` (issue
#150) says explicitly that it's a prompt-text heuristic and that no files
have been claimed yet, rather than implying real conflict prediction --
the original wording ("expect a merge conflict there") oversold a pure
text-token match as something closer to `Orchestrator::detect_conflicts`'
git-level analysis. The heuristic itself is unchanged; only the disclosure
in the printed warning changed.

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

### Arbiter diagnosability (issue #106)

Every early-return path in `run_arbiter_inner` used to delete
`.pact-arbiter.jsonl` unconditionally, win or lose -- a real Arbiter
failure left nothing to inspect afterward, not even a raw log. Fixed by
writing the log to the same stable `state_dir/logs/` location a normal
workspace's own log uses (`arbiter-<identifier>.jsonl`), not inside
`worktree_path` -- the throwaway integration/resolve worktree, which
`merge_all`/`resolve_conflict` tear down unconditionally once they
finish, so a log that merely survived Arbiter's own return paths would
still have been destroyed moments later by the *caller's* cleanup.
Deleted only on a genuinely accepted resolution; every failure path
leaves it in place, with the warning log line naming exactly where.

### Arbiter's real-world resolution rate was 0/6 before the Write-fresh redesign (issue #106)

With diagnosability restored, six real Arbiter attempts were run against
the same class of conflict (two workspaces each inserting one line/
function at the same point in a small file) -- Sonnet and Haiku, default
safety, `acceptEdits`, and `bypassPermissions`. **All six ended the same
way**: Arbiter's own sub-agent describes the correct resolution in
plain text, then says it needs permission to actually apply it, even
under `bypassPermissions` (the strongest override, meant to skip every
confirmation). This ruled out pact's own `--safety`/`--allowedTools`
plumbing as the cause -- there was no stronger override left to try.

### Arbiter Write-fresh redesign (issue #106)

The fix implemented and real-agent-verified this pass: instead of
leaving the conflicted file's raw `<<<<<<<`/`=======`/`>>>>>>>` text on
disk and asking the agent to `Edit` it in place, Arbiter now hands the
agent each conflicted file's clean three-way content (BASE/OURS/THEIRS,
read via the same `git show :N:path` machinery `pact-vcs`'s own semantic
resolvers use -- see `WorkspaceManager::conflict_stages`, now public)
directly in the prompt, and instructs it to compose the full resolved
file itself and `Write` it -- never `Edit` -- in one call.

**Confirmed by hand this alone wasn't enough.** The first real
verification attempt with a Write-based prompt still failed identically
to the old Edit-based one ("I don't have permission to edit `math.js`
yet") -- because the file was still sitting in git's actual unmerged
("UU") index state at the time, and a real agent (confirmed directly)
refuses to touch a file git reports as unmerged, regardless of which
tool is used or what permission override is set. `Edit` vs `Write` was
never the real mechanism; git's own conflict bookkeeping was.

**`WorkspaceManager::neutralize_conflict`/`restore_conflict`** (pact-vcs)
is the actual fix: before invoking Arbiter, each conflicted file's index
entry is temporarily collapsed from stages 1/2/3 down to a single plain
staged blob (`git add` of the "ours" stage's content, a valid,
marker-free placeholder), clearing the "UU" status a real agent
apparently checks for. `neutralize_conflict` returns a `ConflictSnapshot`
(the original on-disk bytes plus `git ls-files -u`'s raw index-info
lines) that `restore_conflict` uses to put the file back into its exact
original unmerged state on any rejection path -- a declined resolution
must leave the workspace exactly as conflicted as it was before the
attempt, for `pact resolve`/manual intervention. **Confirmed by hand,
non-obvious**: restoring isn't just "write the stage entries back" --
`neutralize_conflict`'s `git add` leaves a stage-0 entry behind that
`git update-index --index-info` alone doesn't clear, so a naive restore
left the index with stage 0 *and* stages 1/2/3 simultaneously, which
`git status` reports as `UM` (modified), not the real `UU` the file
actually was. `restore_conflict` explicitly `git update-index
--force-remove`s the path first to clear that stale stage-0 entry before
feeding the original index-info back in -- caught by
`crates/pact-vcs/tests/arbiter_conflict_prep.rs`, which asserts the
post-restore status is genuinely `UU`, not just that *some* status comes
back.

**Real-agent-verified, modest spend, not yet 100% reliable.** Four real
`claude`-as-Arbiter invocations against the same reproducible conflict
shape this pass: one full success (`Write` used correctly, real merged
content, test command passed -- Arbiter's first ever confirmed real
success); one rejected because the agent used `Edit` anyway despite the
prompt explicitly forbidding it (now worded more forcefully -- "never
Edit... Edit will be denied"); one rejected due to a since-separately-
fixed stdin issue (issue #184); one rejected where the agent correctly
understood the task and explicitly intended to `Write` the right
content, but the `Write` itself was still denied even though the file's
own conflict had already been neutralized. That last case is filed as
issue #185 -- leading hypothesis is that the *repository* is still
genuinely mid-merge (`.git/MERGE_HEAD` present) regardless of any one
file's own index state, and the agent may be checking for that
repo-wide signal too, not just the per-file one. Not chased further
this pass (a repo-wide `MERGE_HEAD` neutralize/restore would be a bigger
change than the per-file one already shipped, and "modest" real-agent
spend was the brief for this session).

Every rejection path -- old or new -- falls back to the same existing
safety net regardless: the workspace stays a normal skipped/persisted
conflict, resumable via `pact resolve`. This redesign makes Arbiter
*capable* of succeeding against a real agent for the first time, a real
and verified improvement over a 0% success rate -- it does not yet make
every attempt succeed.

### Arbiter scope enforcement (issue #146/#147)

From an outside code review (2026-07-24, full triage:
https://claude.ai/code/artifact/2cd644b9-b0e2-4533-9706-2034f798ff20):
prior to this, `run_arbiter_inner`'s only validation was that conflict
markers were gone from the *listed* files and the given test command
exited 0 -- confirmed by reading the actual pre-fix code before acting
on the report. Nothing checked whether the agent touched anything
*outside* that list, and nothing beyond "still has markers" caught an
agent that resolved a file by wiping it. The prompt tells the agent not
to touch anything outside the conflicted-file list, but a prompt
instruction is not enforcement -- the same "don't trust the agent's own
claim" reasoning that already applied to conflict-marker checking now
also applies to scope.

`validate_arbiter_scope` (pact-core) is the fix, extracted as a
standalone function specifically so it's testable against a real git
repo without spawning a real agent (unlike `run_arbiter_inner`, which
does). It checks, in order:

1. **Conflict markers gone** from every listed file (pre-existing check,
   unchanged).
2. **Not emptied** -- a listed file that had real content (at minimum
   its own conflict markers) before the agent ran can't come back empty
   or whitespace-only. Deliberately does *not* try to catch a merely
   *suspiciously large* shrink beyond "went to nothing": removing marker
   lines and one side's content is a normal, expected part of every
   correct resolution, so a size-based heuristic risks rejecting good
   resolutions along with bad ones. A missing file (the re-read itself
   fails) already rejects via the same code path, covering "arbiter
   deleted a conflicted file" without a separate check.
3. **Nothing changed outside the listed files** -- new public
   `pact_vcs::changed_paths(dir)` runs `git status --porcelain` and
   reuses the crate's own existing `parse_porcelain_path` (already
   relied on by `dirty_status`/`workspace_changes`) rather than
   reimplementing porcelain parsing in pact-core. Fails closed: if
   `git status` itself fails, that's treated as a violation, not "assume
   fine."

Verified for real, not just by inspection: 6 new pact-core tests build
an actual throwaway git repo with a real conflict-marker file, simulate
"the agent already ran" by writing/deleting files directly, and assert
accept/reject for each case -- clean resolution, leftover markers, an
emptied file, a deleted file, an out-of-scope change, and a
legitimately-new file created within scope (confirms `pre_run_lengths`
defaulting an unseen path to 0 doesn't itself trigger the emptied-file
check). Plus 2 new pact-vcs tests directly on `changed_paths` itself
(modified+untracked files reported, clean worktree reports empty).

**Remaining #147 item, closed out alongside the Write-fresh redesign**:
"no lockfile changed unless explicitly allowed." There's no existing
allow-mechanism for this, so the simplest correct behavior is the
strictest one -- `attempt_arbiter_resolution` now rejects up front,
*before* spawning a real agent at all, if any conflicted file is a
lockfile (reusing `pact_vcs::is_never_auto_resolve`'s existing list,
made `pub` for this). A lockfile needs the real package manager to
regenerate it correctly, not a "combine both sides' intent" hand-written
merge -- semantic auto-resolution already refuses to touch one for the
same reason; Arbiter now holds itself to the identical rule rather than
being a backdoor around it. Verified with a real fake-agent e2e test
(`merge_all_refuses_to_let_arbiter_touch_a_conflicted_lockfile`) that
also asserts no `arbiter-*.jsonl` log was ever written -- proof the real
agent process was never spawned at all for a lockfile, not just that
the outcome was rejected. Confirmed this is a genuine regression test,
not a coincidental pass: temporarily removed the guard and reran, which
failed exactly as expected.

### Arbiter decision records (issue #148)

Reviewer's proposed "durable audit artifact per attempt" -- a passing
test command doesn't prove semantic correctness, so a *successful*
Arbiter attempt needs the same durable record a rejected one already had
(the raw JSONL log, since issue #106). Reviewer's own proposed shape was
a new `.pact-<repo>/arbiter/<id>/<timestamp>/...` directory tree;
implemented instead as an extension of the *existing*
`state_dir/logs/arbiter-<id>.jsonl` convention -- a sibling
`arbiter-<id>.decision.json`, not a second competing directory
structure.

Required restructuring `run_arbiter_inner`, which previously logged and
`return`ed `Vec::new()` at five separate points scattered through the
function -- there was no single place to hang "write the decision record"
off of without repeating it five times. Split into `attempt_arbiter_resolution`
(the actual agent-spawn-and-validate logic, now returning one
`ArbiterOutcome` enum -- `Accepted { resolved_files }` or
`Rejected { reason, test_passed }` -- instead of scattered early
returns) and `build_arbiter_decision` (pure JSON-value construction from
an `ArbiterOutcome`, no I/O). `run_arbiter_inner` itself is now just:
run the attempt, build the decision from whatever it returned, write it
unconditionally, then act on accept/reject. Every exit path produces a
record now, not just the ones someone remembered to add a write call to.

`test_passed` is `Option<bool>`, not `bool` -- deliberately distinguishes
"the test command ran and failed" (`Some(false)`) from "rejected before
ever reaching that step" (`None`, e.g. leftover conflict markers or an
out-of-scope file change) -- collapsing those into one boolean would
lose real diagnostic information about *how far* an attempt got.

Verified with 3 new unit tests directly on `build_arbiter_decision` (no
agent spawn needed, pure function): an accepted attempt's full field
set, a rejected attempt's reason surfacing correctly, and the
`test_passed: Some(false)` vs. `None` distinction specifically.

### Structured run metadata (issue #15)

From an outside code review (2026-07-24), verified against source:
`RunOutcome` (pact-agents) is exactly `{ success: bool, summary: String }`
-- no start/end time, command/args/cwd, coordination status, or log
path. That context only ever lived in ephemeral terminal output and the
raw JSONL log file, never a queryable record.

New `RunMetadata` (pact-core) persists all of that to
`state_dir/meta/<id>-run.json`, sibling to the workspace's own
`meta/<id>.json` and the dependency-prep report (issue #12) -- the same
three-file-per-workspace convention now covers workspace identity,
dependency prep, and the actual agent run. Recorded regardless of
success or failure: `spawn_with_supervisor` used to propagate
`run_and_stream`'s `Err` straight up via `?` with nothing captured first,
meaning a run that failed to even start left no durable trace at all,
the exact case most worth one. `coord_status` is `Option<String>` --
`None` means no coordination config was attached to this run in the
first place, distinct from a status that was reported but never settled
on `connected` (a real, different failure mode already surfaced by the
existing `coord_warning` check).

Verified with 2 round-trip unit tests on the struct itself (serialize,
deserialize, field-for-field check) -- the write-to-disk mechanics
themselves follow the exact same `std::fs::write` + `serde_json::
to_vec_pretty` pattern already exercised for Arbiter's `decision.json`
(issue #148) and the dependency-prep report (issue #12), not
independently re-verified here. The actual `spawn_with_supervisor`
integration wasn't end-to-end tested against a real agent CLI -- doing
so would mean a real, billed agent spawn, which this project's test
conventions explicitly avoid (see CLAUDE.md).

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

### Agent subprocesses get an explicitly closed stdin (issue #184)

The `Command` built here never called `.stdin(...)` at all, meaning every
spawned agent process inherited pact's own stdin handle verbatim -- a
terminal, a pipe, whatever pact's own parent process happened to have.
pact only ever feeds an agent via its task/`-p` argument, never via
stdin, so an inherited, ambiguous handle serves no purpose and risks a
CLI behaving differently than a genuinely headless invocation would.
Found real-agent-verifying the Arbiter Write-fresh redesign (issue #106):
one real `claude -p ...` invocation printed "no stdin data received in
3s, proceeding without it" on stderr and the agent then reported
receiving an apparently-empty task, despite a real, non-empty `-p` value.
Not reproduced on every run, so likely dependent on the parent process's
own stdin state at spawn time rather than deterministic -- but the
underlying gap (stdin never explicitly closed) was real regardless.
`.stdin(Stdio::null())` now makes the "no stdin, ever" contract explicit
instead of leaving it to whatever the parent process happened to have.

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

**`--safety plan` isn't a strict workspace-isolation guarantee (issue
#103).** Confirmed by hand during the 2026-07-23 stress-testing campaign:
a real `pact spawn --safety plan` against an edit task correctly left
the target file untouched (plan mode really is read-only for the repo).
But Claude Code's own plan-mode feature separately wrote a real file to
the **host user's** `~/.claude/plans/<generated-slug>.md` -- outside the
isolated `.pact-<repo>/workspaces/<id>` worktree entirely, invisible to
`pact teardown`, never cleaned up. Not something pact's own code causes
or can prevent -- Claude Code CLI's own architecture decides where plan
documents go, apparently always this fixed global location regardless of
cwd -- so the fix here is a documented caveat (CLI help text, README),
not a code change: don't treat `--safety plan` as a guarantee that
*nothing* happens outside the workspace, only that the target repo isn't
edited.

**The coordination MCP tools need their own allowlist entry (issue
#104).** The 2026-07-23 Claude Code stress-testing campaign found that
`DEFAULT_ALLOWED_TOOLS` never included `mcp__pact-coord__*` -- meaning
`claim_files`/`release_files`/`send_message`/`check_messages` were
silently denied by Claude Code's own permission gate on every real,
default-safety spawn, even though the MCP server itself connects and
registers its tools correctly. Confirmed the fix directly, not just
reasoned about it: a real 2-agent `spawn-many` at plain default safety
now completes claim/broadcast/check/claim end-to-end with the correct
conflict detected, zero denials. `mcp__pact-coord__*`'s wildcard syntax
was itself confirmed against a direct `claude --allowedTools "...
mcp__pact-coord__*"` invocation in the exact same default permission
mode this adapter already uses (not `bypassPermissions` -- the
curated-allowlist safety posture didn't need weakening to fix this).

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

**A "wait for X, then Y" task can end its turn before Y happens (issue
#107).** Found incidentally during the 2026-07-23 stress-testing
campaign, while testing process-kill behavior: given a task phrased
exactly that way, Claude Code ran the wait as an async background bash
task and ended its own turn without ever actually waiting for it or
doing `Y` -- its final message honestly described the *plan* ("I'll be
notified when it finishes, and then I'll create done.txt"), not a
completed action. `pact` correctly reported this as `done`, matching the
same established principle as A5/A8 in the campaign's own findings (pact
reports the agent's own completion, not whether the user's goal was
satisfied) -- but there's no continuation mechanism in headless mode, so
`Y` never happens once the process exits. Not a pact bug -- a real,
non-obvious trap in how a headless agent can interact with "wait for X"
phrasing when its own CLI has an async-task capability. Documented as a
task-writing caveat (README's Known limitations), not fixed in code,
since there's no code-level lever to pull here.

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

**The untrusted-directory approval downgrade (found on a later
verification pass, issue #9):** `--approval-mode yolo` alone isn't
actually yolo mode -- confirmed directly, running it against a fresh
scratch repo Gemini CLI hadn't seen before printed `Approval mode
overridden to "default" because the current folder is not trusted.` to
stderr *before* even reaching the auth check, then would have hung
waiting for interactive confirmation in a real authenticated session
(the exact hang class this codebase already tracks carefully for Copilot
CLI's `--allow-tool`). `--skip-trust` (confirmed present and doing
exactly this in `gemini --help`'s own text: "Trust the current workspace
for this session") fixes it -- re-run with the flag added, the downgrade
message disappears, leaving only the expected, unrelated auth failure.
`build_command` now always includes `--skip-trust` alongside whatever
`--approval-mode` value is in effect, default or overridden -- an
unattended `pact spawn --agent gemini` has no human available to accept
a trust prompt any more than it has one available to accept a tool-call
confirmation, so both need to be preempted, not just the one that was
originally assumed to matter.

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

### `list_claims` MCP tool (issue #149)

From an outside code review (2026-07-24), verified against source: only
4 `#[tool]` handlers existed (`claim_files`/`release_files`/
`send_message`/`check_messages`) -- an agent had no way to ask "what's
currently claimed, by whom, until when" without inferring it indirectly
from a `claim_files` conflict response. Only the human-facing `pact
coord-status` CLI command exposed the full picture.

Added as a 5th tool, thin wrapper over the existing
`leases::list_active_leases` query (already backs `coord-status`, no new
query logic). No holder filter, same as `coord-status` -- a full
snapshot of coordination state, not scoped to "not me" the way
`check_messages` excludes an agent's own messages. `ActiveLease` gained
`Deserialize`/`PartialEq`/`Eq` derives (previously `Serialize`-only,
since nothing needed to parse it back) so both the server-side tests and
the pact-coord SDK bindings could round-trip it directly instead of
string-matching JSON output.

Shipped alongside matching `list_claims`/`listClaims` methods in both
pact-coord SDK bindings (issue #127) -- keeping the bindings in sync
with the actual tool surface as it grows is the default, not something
that needed a separate decision each time a tool gets added.

### Known scaling limit: `expand_glob` cost (issue #72)

`expand_glob` walks the entire workspace file tree (`WalkDir::new(root)`)
on every call, with no early pruning based on the glob's literal prefix.
`claim_files`' conflict-detection path calls it once per incoming pattern
*and* once per existing lease row being checked for overlap, so a single
`claim_files` call can trigger several full tree walks -- O(files in
workspace) per call, not O(1). Fine at the scale this has actually been
tested at; flagged here as a deliberate, known tradeoff rather than a
silent surprise, since the README cites MCP Agent Mail's 40-50-concurrent-
agent scale as prior art without pact itself having been tested anywhere
near that. Not optimizing preemptively -- revisit (glob-prefix-based
pruning, or caching the file list per workspace between calls) only if
real usage actually hits this as a bottleneck.

### Known limitation: intermittent MCP connection status under concurrency (issue #105)

Found during the 2026-07-23 Claude Code stress-testing campaign: under a
concurrent `spawn-many` batch, one or more agents can end up with their
`pact-coord` MCP connection status stuck at `pending` (or reported
`failed`) for the whole session -- silently losing all coordination
capability, no retry, no warning beyond a log line. Reproduced at 1/6
batches at 2-agent concurrency, 40% of agents at 5- and 10-agent
concurrency.

**Three concrete angles investigated, none of which reduced the rate:**

1. Switching `mcp-serve`'s tokio runtime from the default multi-threaded
   one to `new_current_thread()` (a real, worth-keeping improvement on
   its own merits -- one stdio server serving one client has no use for a
   worker thread pool -- but it didn't move the failure rate).
2. Staggering concurrent launches (400ms apart, individual `spawn` calls
   instead of one `spawn-many`) -- made it *worse* (7/10), not better.
3. Switching to a faster/cheaper model (Haiku) to keep investigation
   costs down -- made it *worse* too (6/10 vs. Sonnet's 4/10), which
   points at *why*: a faster model reaches Claude Code's own status
   snapshot sooner after subprocess launch, giving less runway either
   way -- consistent with a one-shot-snapshot explanation, not a
   pact-side slowness explanation.

**Decisive diagnostic: `pact mcp-serve` itself is confirmed fast and
100% reliable, entirely apart from Claude Code.** A small script sent a
real MCP `initialize` request directly to N concurrent `mcp-serve`
subprocesses over stdio, no agent CLI involved at all. Solo: 47ms. At
10-way concurrency: 140-203ms across all 10, zero failures. This rules
out pact's own subprocess -- database open/migration, the tokio runtime,
process startup -- as the bottleneck; it was never the slow part.

**Working theory, not confirmed further:** Claude Code reports an MCP
server's status exactly once, in its very first `system/init` event --
there's no follow-up event if the connection settles a moment later. The
actual failures are likely real OS-level CPU scheduling contention (many
concurrent, genuinely-inferencing `claude.exe` processes competing for
cores) delaying Claude Code's *own* process from promptly reading its
already-ready child's response within whatever internal timeout it
applies to that one-time snapshot -- a boundary pact's own process can't
see across or influence.

**Consequence for a real user:** running `spawn-many` with several
Claude Code agents (5+) carries a real, roughly 20-50%-per-agent (highly
environment-dependent) chance that one or more agents silently proceed
with zero coordination capability for their entire session -- no
`claim_files`/`send_message`/`check_messages` availability, no retry, no
visible signal beyond a `WARN`-level log line most users won't be
watching for. Every *other* safety net (worktree isolation, `merge-all`'s
real conflict detection, Weaver's pre-flight text-overlap warning) still
applies regardless -- this removes one layer, not all of them.

Left open, documented, not fixed -- revisit if a different angle
presents itself, or if Claude Code's own MCP client behavior changes.

### Coord status (issue #64)

`pact_coord::status` gives `pact coord-status` a read-only snapshot of the
coordination layer: every active (non-expired) lease, and a pending
(unread) message count per known agent. Landed because pact-coord was
otherwise a black box from outside an MCP client -- the only visibility
was indirect, via `pact conflicts`' per-file lease/message enrichment,
which only surfaces coordination context for files already flagged as
conflicting.

Two things worth knowing:

- **"Known agent" has no dedicated table.** Agent identity is implicit
  everywhere in this schema (a workspace id doubles as its MCP
  `agent_id`), so `known_agent_ids` unions every place an id can appear --
  lease holders, message senders/recipients, and `read_cursors` rows --
  rather than querying one canonical source.
- **Computing a pending count must not advance anyone's cursor.** Unlike
  `check_messages` (which is the caller *reading* its own messages, and
  correctly consumes them), a status view is a third party looking in --
  looking shouldn't change what a later real `check_messages` call from
  that agent would see. `pending_message_count` runs the identical
  recipient-matching query `check_messages` does, but only counts, never
  writes to `read_cursors`.

### Operation log / `pact history` (issue #84)

From an outside strategic-notes review surveying jj (Jujutsu)'s data
model: jj's operation log makes every operation a versioned, replayable
event. Pact's coord DB (leases + messages, both already timestamped rows)
was most of the way to this shape already, but there was no way to ask
"what happened in this session" as a whole -- only `coord-status`'s
current-state snapshot and `conflicts`' per-file enrichment.

**What "operation" means here, precisely:** one already-happened,
significant coordination-layer event -- `claim`, `release`, `broadcast`,
`message`, `merge_all`, `arbiter_decision`, `teardown`. Deliberately
excludes `check_messages`: it's a read, not an event that changed
anything, and the issue's own proposed shape didn't list it either.
`merge_all` is logged as **one row per invocation**, not one row per
workspace merged/skipped within it -- the per-workspace outcome (merged
ids, skipped ids + reasons) lives inside that one row's JSON `detail`
column. A `merge-all` run is one event from a user's perspective ("what
happened when I ran this"); splitting it into N rows would make
reconstructing "this was one call" require correlating rows back
together for no real benefit, since the detail blob already carries the
per-workspace breakdown the issue asked for.

**Storage: reuses the existing coord SQLite DB**, not a new one -- a new
`operations` table (`id`, `created_at`, `op_type`, `workspace_id`
nullable since `merge_all` spans multiple workspaces, `detail` as a JSON
text blob) alongside `leases`/`messages`/`read_cursors` in `db::open`'s
schema. This is exactly the "reuse what's already there" option the
issue itself favored, and it means every existing concurrency guarantee
(WAL mode, `busy_timeout`, one file per repo keyed by `db::db_path`)
applies to operations for free, no new infrastructure.

**Where logging happens, by process:** `claim`/`release`/`broadcast`/
`message` are logged inside `pact-coord`'s own MCP tool handlers
(`server.rs`), right where `leases::`/`messages::` are already called
with the connection in hand. `merge_all`/`arbiter_decision`/`teardown`
happen in the main `pact` process (`pact-core`), never inside an
`mcp-serve` subprocess, so they go through a new `pact_coord::log_operation`
entry point that opens its own short-lived connection against the same
`db::open(repo_root)` path -- the same pattern `pact_coord::status`/
`leases_matching`/`message_count_involving` already use for read access
from `pact-core`, just for a write instead.

**Query surface: `pact history`** (over `pact session-log`: shorter, a
familiar git-like mental model). Filters: `--workspace <id>`, `--since
<unix-seconds>`, `--type <op_type>`, `--limit <n>`. Human-readable output
by default (one line per operation: timestamp, type, workspace, a short
summary derived from `detail`); `--json` for the raw rows. No dedicated
`--outcome` filter in this first cut -- "outcome" (success/failure) only
means something for `merge_all`/`arbiter_decision`, and extracting it
generically would mean parsing type-specific fields out of an opaque
JSON blob; `--type merge_all` plus reading the printed detail covers the
same need without that complexity. Can be added later if it's actually
missed.

**Explicit non-goals**, per the issue's own scope: read-only query only,
no undo, no "fork from any past state," no replay-as-mutation -- pact is
a single-orchestrator-per-run tool, not jj's multi-user concurrency
model, so none of jj's distributed-operation-log machinery applies here.
Also explicitly not solved here: unbounded row growth over a very
long-lived repo. The issue didn't ask for a retention/cleanup policy, and
inventing one unprompted would be scope creep past what was asked --
noted here as a known limitation, not silently ignored, revisit if a
real repo's `operations` table actually becomes a problem in practice.

### Persisted conflicts / `pact resolve` (issue #85)

Companion to issue #84 above, and explicitly sequenced after it landed --
this builds on the same coord DB and the same `db::open`-per-call
pattern, rather than inventing separate storage. See DESIGN.md ("pact-vcs
> Persisted conflicts (issue #85)") for the git-level mechanics
(`ConflictedWorkspace`, `resolve_conflict`); this section covers what's
specific to persistence and the CLI surface.

**Storage:** a new `conflicts` table, alongside `operations`/`leases`/
`messages` in the same coord DB -- `workspace_id`, `target_branch`,
`files` (JSON array), `created_at`, `status` (`open`/`resolved`/
`abandoned`), `resolved_at`. `pact-core::merge_all` persists one row per
`ConflictedWorkspace` right after the merge attempt, best-effort (a
persistence failure warns, doesn't fail the whole `merge_all` call --
same posture as operation-log writes).

**Naming:** `PersistedConflict`, deliberately not `Conflict` -- that name
is already taken by `leases::Conflict` (an advisory lease-overlap
warning inside `claim_files`'s response), a completely different
concept. A third, also-unrelated "conflict" already exists in this
codebase too: `pact_core::FileConflict` (issue #8's cross-workspace
file-touch report, driving the pre-existing `pact conflicts` command,
which is informational-only and has nothing to do with `merge_all`).
Three genuinely different concepts sharing an English word is a real
source of confusion worth naming explicitly, not just in code -- this is
exactly why the issue itself insisted `pact resolve` be a new verb
distinct from the existing `pact conflicts`, and why this section spells
out the naming collision by name.

**What "resolved" means:** a persisted conflict becomes `resolved` only
when `pact resolve <id>` retries the merge and it succeeds (cleanly, via
auto-resolve, or via Arbiter) -- not when a user manually decides it
doesn't matter anymore (that's `abandoned`, a separate explicit action,
`pact resolve <id> --abandon`). Every retry, successful or not, is logged
as a `conflict_resolve` operation (reusing issue #84's log), so `pact
history` shows the attempt happened even when it didn't resolve anything.

**Retention:** no automatic expiry, matching issue #84's own precedent of
not inventing an unprompted cleanup policy -- `abandoned` is the manual
escape hatch for "not worth resolving," not a TTL.

**CLI surface:** `pact resolve` (no workspace id) lists every open
conflict; `pact resolve <id>` retries the most recent open one for that
workspace, taking the exact same `--union`/`--test-cmd`/`--arbiter-agent`/
`--arbiter-safety` flags as `merge-all` (extracted into a shared
`build_arbiter_config` helper in `pact-cli` so the two commands can't
drift in how they parse an equivalent flag set) -- this directly answers
the issue's own open question about Arbiter's relationship to a
persisted conflict: yes, standalone, outside a live `merge-all` run,
using the identical mechanism. Exit code 2 on a still-conflicted retry,
mirroring `merge-all`'s own "skipped, not a hard failure" exit-code
convention (issue #27) rather than a plain error.

### pact-coord SDK bindings v1 (issue #127)

User's explicit framing: this is a foundation, not a quick add -- "plan
really deeply... must consider every angle" before writing any client
code. Design questions resolved, in order:

**Transport: spawn `pact mcp-serve` and speak real MCP over stdio --
not a new server.** The critical fact that decides this whole design:
`pact mcp-serve` is *not* a persistent daemon a client could connect to
-- it's a fresh subprocess launched per agent session, over stdio, backed
by the shared SQLite DB (see "Database placement" above). There is no
standing network-facing coordination server today, and inventing one for
this SDK would be new server-side surface, a new attack surface (a real
listener needs real access control; a stdio pipe implicitly scoped to
whoever spawned it doesn't), and a new maintenance burden -- none of
which the issue actually asked for. The right model: a Python/TS client
using pact-coord *is* an MCP client, exactly like Claude Code or Copilot
CLI already are, just driven by a script instead of an LLM. This also
directly reuses the existing `--coord-command`/`--coord-arg` override
(issue #10) -- pointing an agent at a custom coordination command is
already a supported extension point; the SDK is simply the first
non-agent-CLI consumer of it.

**Build on the official MCP client SDKs, don't hand-roll the wire
protocol.** `pip install mcp` (Anthropic's own Python MCP SDK) and
`@modelcontextprotocol/sdk` (npm) both install cleanly and are the
correct, protocol-compliant way to speak MCP's stdio transport and
initialize handshake -- confirmed by installing both for real and
inspecting their actual client APIs (`ClientSession`/`StdioServerParameters`
in Python; the equivalent `Client`/`StdioClientTransport` in the TS SDK),
not assumed from documentation. Re-implementing JSON-RPC framing and the
MCP handshake by hand in two more languages would be real, avoidable
protocol-compliance risk for zero benefit -- pact-coord's bindings are a
*thin, opinionated wrapper* around each language's official SDK, mirroring
how `pact-agents`' own adapters wrap each agent CLI's conventions rather
than reimplementing them.

**API shape.** One class per language (`PactCoordClient` /
`PactCoordClient`), four methods matching the four MCP tools exactly:
`claim_files`, `release_files`, `send_message`, `check_messages` --
parameters and return shapes match `pact-coord/src/server.rs`'s actual
`ClaimFilesParams`/`ReleaseFilesParams`/`SendMessageParams` and the
`ClaimResult`/`Message` types field-for-field (`accepted`/`has_conflicts`/
`conflicts`, not `granted`, matching the issue #52 rename), not a
redesigned API. Two ways to construct a client: spawn `pact mcp-serve`
itself (the common case, mirrors what pact-core's own orchestrator does
for a real agent), or accept an already-open MCP session (for a caller
that manages the subprocess itself) -- the latter is needed internally
regardless, so exposing it is free.

**Error semantics: match `isError`, don't invent a new taxonomy.** Every
pact-coord tool result already carries `isError`/text content on failure.
The binding raises a typed exception (Python: `PactCoordError`; TS: same
name) carrying that exact text, rather than a generic MCP protocol error
-- callers get the same failure message a human reading raw MCP traffic
would see.

**Auth/trust model: unchanged, explicitly a v1 non-goal to extend it.**
Since transport is a locally-spawned subprocess over stdio -- identical to
today's real agent-CLI usage -- there is no new threat model to design
for. The binding does **not** support connecting to a remote/networked
pact-coord instance, because no such thing exists; adding one would be a
materially larger feature (a real daemon, real access control) that
nothing in the original issue asked for. Worth its own design
conversation if real demand shows up, not built ahead of it.

**Packaging: repo-local for v1, not published to PyPI/npm.** Registry
publishing adds real, permanent surface (namespace squatting risk,
security-disclosure process, a release process to keep in sync with
pact's own versioning) for a v1 with no external users yet. `bindings/
python`/`bindings/ts` are installable directly (`pip install
./bindings/python`, or a git URL; `npm install <git-url>` or a local
path) -- publishing to real registries is a reasonable follow-up once
there's actual demand, not a default to reach for immediately.

**Versioning:** the binding targets whatever `pact mcp-serve` binary is
on `PATH` (or an explicit path) at call time -- same "trust the installed
CLI" model every other pact-adjacent tool already has, no separate
protocol-version negotiation invented. A minimum-compatible-pact-version
note lives in each binding's own README rather than an enforced runtime
check, matching how CLI flags document their own minimum requirements
elsewhere in this project.

**Testing:** unlike agent-CLI-invoking code, spawning `pact mcp-serve`
costs nothing and involves no LLM -- it's a deterministic Rust binary.
Both bindings' test suites spawn a real `pact mcp-serve` subprocess
against a real throwaway repo and exercise claim/release/send/check
end-to-end for real, the same "real repo, no mocking" standard this
project already holds git-interaction tests to, not stubbed out the way
a real agent-CLI call would need to be.

### VS Code extension v1 (issue #128)

From an outside adoption/UX review: `pact history`'s operation log
(issue #84) is genuinely novel among parallel-agent tools, but most CLI
users won't appreciate it as text output the way they'd immediately grok
it as a timeline in their editor. `editors/vscode/` is a minimal webview
that shells out to `pact history --json` (reusing the exact flag already
built for scripting, not a new pact-cli surface) and renders it -- one
command (`pact.showHistory`), one webview panel, a `pact.binaryPath`
setting for when `pact` isn't on `PATH`. Not published to the
Marketplace -- v1 scope per the issue itself.

**Split for testability, not just tidiness:** `render.ts` (HTML
generation -- `escapeHtml`/`formatTimestamp`/`renderHtml`) has zero
dependency on the `vscode` module; `extension.ts` is the thin glue
(command registration, `execFile`, webview lifecycle) that does. This
means the actual rendering logic has real unit tests
(`tests/render.test.ts`) that run with plain `vitest`, no VS Code
Extension Host required -- including a real captured `pact history
--json` payload (two agents claiming/releasing/messaging via the
pact-coord Python binding against a real throwaway repo) as fixture
data, not fabricated JSON, plus an explicit HTML-injection test (a
message body containing `</pre><script>...` must not survive
`escapeHtml` unescaped).

**What's verified vs. not, honestly:** TypeScript compiles clean against
real `@types/vscode`; the real `pact history --json` shape this code
depends on was captured from an actual run and matches what `render.ts`'s
tests assert against; the extension activates in a real Extension
Development Host (`code --extensionDevelopmentPath=...`) without an
immediate crash. **Not verified:** actually seeing the rendered webview
render correctly in a live VS Code window -- this environment can drive
a VS Code process but has no way to inspect a GUI window's visual output,
so the "does it look right" question is unconfirmed here, same
"implemented, not fully live-verified" honesty this project already
applies to the Gemini adapter, the Homebrew tap, and the winget
manifest. Worth a real look on a machine with an actual display before
calling this fully done.

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

### Structured prep reporting (issue #12)

From an outside code review (2026-07-24), verified against source:
`prepare` returned bare `Result<()>`, and every real per-manager failure
was a `tracing::warn!` and nothing else -- no way to know which managers
were detected, which strategy ran, whether the npm content store was hit
or freshly populated, or which materialization method (reflink/
read-only-hardlink/copy) was used, without reading logs.

`prepare` now returns `Vec<ManagerPrepReport>` (one per detected
manager: `manager`, `strategy`, `store_key`, `store_hit`,
`materialization`, `success`, `warnings`) instead of a bare `Result<()>`
-- callers get real data, not just a side effect. `pact-core`'s
`spawn_with_supervisor` persists it to `state_dir/meta/<id>-deps.json`,
sibling to the workspace's own `meta/<id>.json`, feeding `pact inspect`
(issue #16).

Two real, previously-invisible gaps this surfaced while making the
report honest, not by design intent: `passthrough::run` and
`run_plain_npm_install` both used to swallow a real command failure into
`Ok(())` -- only `cmdutil::run`'s own spawn failure would ever surface
as `Err`, meaning a passthrough install that ran and genuinely failed
(bad exit code) was indistinguishable from one that succeeded, in the
return type. Both now return `Result<bool>` (spawn failure is still
`Err`; the command's own exit code becomes the `bool`), which the report
now actually reflects in `success`.

`store_hit` is captured by a new `ContentStore::entry_exists` check
*before* `populate_if_absent` -- calling `populate_if_absent` first and
then checking existence would always report `true` (it just got
populated), telling a hit from a miss requires looking before, not after.

Verified for real, not mocked: 3 new pact-deps tests exercise the actual
`prepare_npm`/`prepare_passthrough` functions against real scratch
workspaces -- a real `npm ci` with a zero-dependency `package.json`/
`package-lock.json` pair (fast, effectively no network needed) confirms
`store_hit: false` on the first call and `store_hit: true` on a second
call with the identical lockfile; a real `cargo fetch` against this
crate's own `Cargo.toml` (cargo is guaranteed present in this workspace's
own build environment) confirms the passthrough success path; a
lockfile-less `package.json` confirms the `plain-install-no-lockfile`
strategy and its warning.

**Deliberately not surfaced in `spawn --dry-run`** -- `--dry-run` only
ever calls the side-effect-free `pact_deps::detect` today, never
`prepare` itself (which does real installs); making dry-run show
`store_hit`/`materialization` would mean either running a real install
during a dry run (defeats the point) or building a separate
preview-only path that predicts what `prepare` would do without doing
it (a real, separate feature, not implied by this finding). Scoped down
from the original issue's broader ask for exactly this reason.

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

### Bun detector (issue #17)

Bun's own lockfile changed format between versions -- older Bun writes a
binary `bun.lockb`, current Bun (confirmed against a real installed 1.3.14
CLI, not assumed) defaults to a text `bun.lock` instead. `detect()` checks
for either, ahead of the pnpm/yarn/npm chain, so a Bun-managed project
(which always also has a `package.json`) is never misreported as npm.
Bun goes through `passthrough::run` like pnpm/yarn -- no custom content
store, since Bun already has its own global cache, the same reasoning
that keeps everything except npm on the passthrough path. Confirmed by
hand: `bun install` defaults to `bun.lock`, not `bun.lockb`, on a fresh
project; `bun install --frozen-lockfile` (verified against a real `bun
install --help`, not assumed) against a project with a committed
`bun.lock` correctly resolves `node_modules` without modifying the
lockfile, mirroring `npm ci`'s reproducibility guarantee -- pnpm/yarn use
`--prefer-offline` instead, a different (caching, not lockfile-strictness)
guarantee, so this isn't an inconsistency, just a different real flag for
a different real semantic gap Bun doesn't otherwise cover.

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

### `--union` renamed to `--append-only` (issue #11)

From an outside code review (2026-07-24): "union" implies something more
general/safer than the actual implementation -- a naive append-only line
concat with limited JS/TS guardrails (rejects duplicate
`module.exports`/`export default`/redeclared bindings, nothing else --
CSS cascade, config keys set twice, non-JS/TS languages aren't checked).
The barrel-file position-loss gotcha was already documented in
`GETTING_STARTED.md`; the flag's own name didn't reflect the limitation.

`--append-only` is now the primary CLI flag name on both `merge-all` and
`resolve` (`#[arg(long = "append-only", visible_alias = "union")]`) --
`--union` still works identically, a visible alias (shown in `--help`'s
`[aliases: --union]`, not hidden), not a silent deprecation. The
underlying Rust field was renamed from `union` to `append_only` too
(not just the CLI-facing flag string) -- otherwise `--help` would have
shown a mismatched `<UNION>` value placeholder under the new
`--append-only` flag name.

Verified for real, not just that both names parse: 2 new integration
tests resolve the identical real barrel-export conflict (same shape
pact-vcs's own `merge_all.rs` tests use) once via `--append-only` and
once via `--union`, confirming both produce the same merged content, not
just that clap accepts both spellings.

### `pact doctor --json` (issue #17)

From an outside code review (2026-07-24): the human-readable `doctor`
output is fine for a person running it directly, but CI diagnostics and
bug reports need machine-readable output. Added `--json`, computing the
same data the human-readable path already does (`git_version`/
`worktree_ok` moved out of the `match` and shared by both output
branches, so they can't disagree) plus `os`/`arch` from
`std::env::consts`.

**Deliberately doesn't include** "pact state directory"/"coordination DB
path"/"config path," despite the original finding listing them --
`pact doctor` resolves *before* `--repo` is ever looked at (same early
dispatch as `Init`/`Demo`/`Completions`, ahead of `Orchestrator::open`),
by design: it's meant to work from anywhere, checking the environment,
not a specific repo. Adding repo-scoped fields would mean either forcing
doctor to resolve a repo (changing its "works from anywhere" character)
or making those fields silently absent/misleading when run outside one.
Scoped down to what's genuinely repo-independent.

Verified with 4 integration tests, including one that runs both output
modes back to back and asserts they report identical
`worktree_supported`, so a future change to one path can't silently
diverge from the other.

### `pact inspect` (issue #16)

From an outside code review (2026-07-24): users need one command that
answers "what is going on with this workspace" -- today that meant
combining `list`/`diff`/`coord-status`/`history` mentally, none of which
alone shows the full picture.

`pact inspect <id>` is pure aggregation, not new computation -- every
data point comes from an existing accessor: `get_workspace` (new, thin
wrapper over `WorkspaceManager::get_workspace`, previously only
reachable indirectly through `list`), `is_dirty`, `agent_process_alive`,
the new `dependency_prep_report`/`run_metadata` readers (issues #12/#15,
reading back the `state_dir/meta/<id>-{deps,run}.json` files those
issues started writing), `coord_status` (filtered to leases/pending
messages belonging to this workspace's own id), `open_conflict_for`
(new, thin wrapper over the existing `pact_coord::open_conflict_for_workspace`
-- the same lookup `pact resolve <id>` itself already uses, not a new
query), and `history` (filtered to this workspace, capped at 20).

`dependency_prep_report`/`run_metadata` treat a missing or unreadable
file as `None`, not an error -- a workspace that was created directly
(e.g. by a test, or one that predates these two issues) legitimately has
no such record, and that's informational, not broken.

Verified for real: 3 new integration tests build a real throwaway repo,
create a real workspace via `WorkspaceManager::create_workspace`
directly (no agent CLI involved), and drive the real `pact` binary --
one against a freshly-created workspace with nothing recorded yet (every
section correctly says so), one with the dependency-prep/run-metadata
files written directly to simulate what a real spawn would have
produced (confirms the aggregation renders real persisted data
correctly), and one confirming an unknown workspace id fails cleanly.

### Error messages suggest the next command (issue #123)

From an outside adoption/UX review: `git` suggests the closest command on
a typo, `cargo` links to docs on error, `gh` prompts login when
unauthenticated -- pact's own failure paths didn't chain back to itself.
Added a `-- try: pact doctor` (or `pact init`/`pact demo`, whichever fits)
suffix to the handful of error messages where the next useful command is
unambiguous from the failure itself: every "unknown agent" error
(`spawn`/`spawn-many`/Arbiter's `--arbiter-agent`), `spawn-many`'s
"no --agent and no prefix" error, and `find_repo_root`'s "no git
repository found" error.

Deliberately narrow -- only added a hint where there's a single, obvious
next step, not to every error path in the CLI. `pact init`'s own
"pact.toml already exists" error already had this pattern (`pass --force`)
before this issue; the ones added here follow that same shape.

While manually verifying this, incidentally found and filed issue #136
(`find_repo_root` can silently walk into an unrelated ancestor git repo) --
real, but a distinct concern from this issue's scope, not fixed here.

### winget manifest (issue #126)

Unlike Homebrew, winget has no "personal tap" equivalent -- the only
distribution path is a manifest submitted as a PR to the single, official,
Microsoft-owned `microsoft/winget-pkgs` repo. Submitted directly (user's
explicit authorization), not held for review first: PR
[microsoft/winget-pkgs#407420](https://github.com/microsoft/winget-pkgs/pull/407420).

Package identifier `zekariasasaminew.pact`, manifest schema 1.12.0 (the
version several real, recently-merged manifests in that repo are
currently on -- checked directly rather than assuming an older schema
version was still current). `InstallerType: zip` +
`NestedInstallerType: portable` matches how pact's own release workflow
actually packages the Windows binary (a bare `pact.exe` inside a `.zip`,
no real installer) -- modeled closely on `ajeetdsouza/zoxide`'s manifest,
a real merged package with the same zip+portable-exe distribution shape,
rather than guessing the schema from documentation alone.

**Not live-verified with a real `winget validate`/`winget install`** -- no
Windows Package Manager client available to run either command in this
environment. Verified instead by: validating the YAML parses cleanly
with Python's `yaml.safe_load`, and comparing field-by-field against
`ajeetdsouza.zoxide`'s real manifest (fetched directly from
`microsoft/winget-pkgs` via the GitHub API, not from memory/assumption).
Disclosed honestly in the PR's own checklist rather than checking boxes
that weren't actually done.

**The CLA check is the one step genuinely outside this session's
control** -- `microsoft/winget-pkgs` requires a signed Contributor
License Agreement before a PR can be reviewed/merged, and that's a legal
agreement only the account owner can sign (via the CLA bot's own comment
on the PR), not something completable on the user's behalf.

Same manual-per-release-bump caveat as the Homebrew tap: this manifest is
pinned to v0.3.0 and needs a new PR (or an update to this one) for every
future release -- an automated bump (winget has its own "WinGet Releaser"
GitHub Action several other projects use, visible in `ajeetdsouza/zoxide`'s
own manifest commit messages) is a reasonable follow-up, not built here.

### Homebrew tap (issue #125)

From an outside adoption/UX review: package-manager distribution turns a
5-step manual download into a 1-command install. Published as a
**personal tap** (`zekariasasaminew/homebrew-pact`, a separate repo, not
this one) rather than submitting to homebrew-core -- core has real
notability requirements and a review process; a personal tap needs
neither and is fully within this project's own control to publish and
update.

The formula (`Formula/pact.rb` in that repo) pins a specific tagged
release (`v0.3.0` at time of writing) with per-platform `url`/`sha256`
pairs -- the `sha256` values came directly from `gh release view
v0.3.0 --json assets --jq '.assets[].digest'`, GitHub's own
server-computed digest of each uploaded asset, not a locally-recomputed
hash, since that's the authoritative value and avoids a transcription
error in a hash that would otherwise just fail installs silently for
real users. No Linux `aarch64` asset exists yet (matches
`release.yml`'s current build matrix), so the formula only covers
`x86_64` Linux plus both macOS architectures and doesn't claim more
platform support than pact's own releases actually build for.

**Not live-verified against a real `brew install`** -- this development
environment has no macOS/Linux Homebrew install available. Ruby itself
also isn't installed here (no admin rights to add it via Chocolatey in
this sandbox), so the formula's syntax was verified only by hand (brace/
`do`/`end` balance) against the extremely standard, mechanical shape
every Homebrew formula follows -- not by an actual `ruby -c` parse or
`brew audit`/`brew install --build-from-source`. Worth a real
verification pass on real Mac/Linux hardware before treating this as
fully confirmed working, same "implemented, not live-verified" posture
this project already applies elsewhere (Gemini adapter, Arbiter's live
path) when a real dependency isn't available in this environment.

Per-release maintenance is a manual step for now (bump `version`/`url`/
`sha256` in the tap repo after cutting a new pact release) -- an
automated bump (e.g. a workflow in the tap repo triggered by pact's own
`release.yml`) is a reasonable follow-up, not built as part of this pass.

### Demo GIF re-recording (issue #124)

From an outside adoption/UX review: a real terminal recording near the
top of the README is the single artifact most likely to convert a reader
into a stargazer, and the existing `docs/demo.gif` (Phase 11) was known
stale -- predates the noise-suppression fixes, and was itself a Pillow
hand-render (real captured content, drawn frame-by-frame with a custom
script) rather than a genuine terminal-session recording, because the one
recording tool tried at the time (`terminalizer`) failed outright on this
Windows/Git-Bash setup.

Re-investigated the tooling gap directly rather than assuming it's still
unfixable:
- **`pip install asciinema` installs cleanly, but the CLI cannot even
  start on native Windows Python** -- `asciinema/recorder.py` does a
  top-level `import fcntl` (a Unix-only module) with no platform guard,
  so every invocation fails at import time before reaching any actual
  recording logic. Confirmed directly, not assumed from documentation --
  this is a hard blocker specific to native Windows, not something a flag
  or workaround fixes.
- **`agg`** (https://github.com/asciinema/agg, `cargo install --git`;
  not the unrelated crates.io package of the same name) **works fine
  standalone.** It only consumes an existing `.cast` (asciicast v2) file
  and renders it to GIF -- no pty involved at all, so the Windows-only
  blocker above doesn't apply to it.

This means the actual gap was narrower than "no recording tooling at
all": specifically, asciinema's own *recorder* is what's Windows-broken,
not the whole pipeline. `docs/record_cast.py` closes that gap by
constructing the `.cast` file directly from a real `pact demo` subprocess
run (real stdout content, real per-line wall-clock timestamps) instead of
going through asciinema's recorder -- `agg` doesn't care how the `.cast`
was produced, only that it's valid.

Content choice: `pact demo`'s own output (issue #119), not a hand-scripted
multi-agent session. This is a strict improvement over the original
GIF's approach for a docs asset specifically: `pact demo` is free,
deterministic, and exercises real `pact` code (`WorkspaceManager`,
`merge_all`) end-to-end, so the GIF can never go stale relative to a real
spawn/merge in the way a baked-in real-agent-output transcript could --
regenerating it after a future change is just re-running the script,
not manually re-transcribing a new session. The `render_demo.py` Pillow
script (Phase 11) is deleted, fully superseded.

One deliberate liberty: `pact demo` finishes in about 1.5 real seconds,
so genuine gaps between lines are mostly single-digit milliseconds -- too
fast to be watchable. `record_cast.py` applies a minimum per-line hold
(`MIN_HOLD_SECONDS`) that only *raises* an unreadably-short real gap,
never shortens a real one -- the two real `git worktree add` calls (the
two workspace-creation lines) still show as genuinely longer pauses than
the rest, since their real gap already exceeds the floor. Disclosed
explicitly in the README's Known limitations, same as the previous
version's own tradeoff was.

### `pact.toml` / `pact init` (issue #118)

pact was 100% CLI-flag driven until this -- no persisted config file at
all, confirmed by grepping for any `Config`/`.toml` reference in the
codebase before starting. From an outside adoption/UX review: reaching a
successful `spawn-many` from a fresh install required knowing 7 concepts
before anything worked, `--agent`/`--safety` being two of them repeated on
every invocation.

Precedence, deliberately in this order: **CLI flag > `pact.toml`'s
`defaults.*` > built-in hardcoded default.** A flag always wins even when
`pact.toml` sets something, so nothing about existing scripted/CI usage
changes unless a `pact.toml` is added *and* the flag is dropped. Applies to
`spawn`/`spawn-many`'s `--agent`/`--safety` and `merge-all`/`resolve`'s
`--arbiter-agent`/`--arbiter-safety` -- all four flags changed from a
clap `default_value`/no-default `String`/`Option<String>` to a plain
`Option<String>`, resolved against `PactConfig` before use. The pre-existing
hardcoded `"claude"` fallback (`spawn`'s old `default_value`,
`--arbiter-agent`'s old `default_value`) is preserved as the last resort,
so a repo with no `pact.toml` behaves byte-for-byte as before.

`PactConfig::load` treats a **missing** `pact.toml` as `Self::default()`
(no error -- this is opt-in, pact must work fully without one) but a
**present, malformed** one as a hard error, not a silent fall-back to "no
config" -- a typo in `defaults.agent` should be loud, not quietly ignored.

`pact init` writes the file, refusing to overwrite an existing one without
`--force`. Agent detection reuses `pact doctor`'s own `AGENT_CHECKS`/
`doctor_check_version` (`detect_installed_agents` in `main.rs`) so the two
commands' view of "what's installed" can never drift apart. Only sets
`defaults.agent` uncommented when *exactly one* agent CLI is detected --
zero or multiple both leave it commented with an explanatory note, since
guessing between multiple installed CLIs would be worse than asking.

`defaults.safety` is never auto-populated by `pact init` (left commented
with an example) -- there's no reasonable per-machine guess for a safety
override the way there is for "which agent CLI is installed."

### `pact demo` (issue #119)

Deliberately fakes exactly one step: the agent CLI call itself. Everything
else -- the temp repo, the two `WorkspaceManager::create_workspace` calls
(via `pact-vcs` directly, not through `Orchestrator::spawn`, since spawning
means launching a real adapter/agent subprocess), the real `merge_all` --
is real pact code on a real, disposable repo.

The reasoning for faking only that one step: `pact demo` needs to work for
a brand-new user who hasn't installed or authenticated any agent CLI yet,
on any machine, with zero cost and zero chance of hanging on an auth
prompt. A real agent call fails all three of those. Each demo "workspace"
instead gets a canned, deterministic file write standing in for what an
agent would have done -- clearly labeled as simulated in the command's own
output, never presented as a real agent run.

Runs from anywhere, not just inside a git repo -- handled the same way as
`doctor`/`completions`/`init`, before `repo_root` resolution, since `pact
demo` creates and owns its own throwaway repo entirely under
`std::env::temp_dir()` and has no use for `--repo` at all.

Cleanup (`remove_dir_all` on the whole temp repo) happens after the run
regardless of success or failure (via a captured `Result` in `demo::run`,
not a `?` that would skip cleanup on an error). Integration test
`demo_leaves_no_leftover_temp_directory` checks only the exact path this
invocation's own output names -- an earlier version swept
`std::env::temp_dir()` for any `pact-demo-*` entry before/after, which is a
real, reproducible false failure whenever this test happens to run
concurrently with `demo_succeeds_from_outside_any_git_repo` in the same
test binary (cargo parallelizes tests within one binary by default), since
the sibling test's own still-running `pact demo` shows up in the sweep.

### `--agent` auto-default (issue #121)

Extends the `pact.toml` precedence chain (issue #118, above) with one more
fallback: **CLI flag > `pact.toml`'s `defaults.agent` > sole detected
installed agent CLI > hardcoded `"claude"`.** `resolve_default_agent` in
`main.rs` implements this and is shared by both `spawn` and `spawn-many`,
reusing the same `detect_installed_agents` (and by extension `pact
doctor`'s own `AGENT_CHECKS`) as `pact init`.

Deliberately only auto-selects when *exactly one* supported agent CLI is
detected -- zero or multiple both fall through to the next fallback rather
than guessing which of several installed CLIs the user meant. When
auto-detection does fire, it prints an explicit note naming which agent
was chosen and how to override it (`--agent` or `pact.toml`) -- never a
silent guess, since a wrong guess here means launching the wrong (real,
billed) agent CLI.

Note this only ever *adds* a new fallback in front of the pre-existing
hardcoded `"claude"` default (`spawn`) / real per-task error (`spawn-many`)
-- neither of those was removed, so a machine with zero or multiple agent
CLIs installed behaves exactly as before this issue.

Verified via a real cross-platform integration test
(`tests/agent_auto_default.rs`) rather than a fake/mocked detection layer:
the test writes an actual runnable `claude` shim (`.cmd` on Windows, a
`chmod +x` shell script on Unix) into a fresh temp directory and overrides
the child `pact` process's `PATH` to exactly that directory, so `pact
doctor`'s real detection logic genuinely finds exactly one agent CLI --
the same code path a real machine with only Claude Code installed would
exercise, not a substitute for it.

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

Every entry up through `session.skills_loaded` came from Copilot CLI
shakedowns specifically -- nobody had looked at Claude Code's *own* raw
`[other]` stream for its own noise until a real `pact spawn-many
--agent claude` run (capturing fresh output for a `docs/demo.gif`
refresh) turned up two more (issue #100): `rate_limit_event` (account
rate-limit metadata, not agent output) and `user` -- in headless mode
there's no real interactive user turn, so every `"type":"user"` event is
the SDK echoing a tool result back to itself, already covered by the
`[tool]`/`[assistant]` events. Smaller scale than issue #58's 75%+
finding (4 lines in one small 2-agent run), but the same category of
confirmed, not-agent-output noise, added to the same list.

The 2026-07-23 Claude Code stress-testing campaign found two more
(issue #102), and this time a plain string in
`SUPPRESSED_OTHER_EVENT_TYPES` wasn't the right shape for the fix:
`system` events with a `subtype` other than `init` (`thinking_tokens`,
`task_started`/`task_notification` from background bash tasks, and
presumably more not yet observed), and `assistant` turns with only a
`thinking` content block (extended thinking, `thinking` empty/redacted
by the API in every capture so far, just a large opaque `signature`
blob). Both are structurally guaranteed to reach `should_print_other`
*only* via their respective noise case -- Claude Code's real `system`/
`init` and real `assistant` text/tool-use events are already consumed
into `AgentEvent::Init`/`AssistantText`/`ToolUse` before the generic
`Other` fallback ever runs, so a blanket `Some("system") => false` /
`Some("assistant") => false` in `should_print_other` is safe, not an
overly broad suppression -- confirmed no other adapter uses either bare
string as a type discriminator at all. Re-verified against a real spawn
after the fix: zero `[other]` lines in an otherwise simple task's
default output.

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

### `--dry-run` preview (issue #16)

`spawn`/`spawn-many` immediately create a real workspace and can launch a
real, billed agent session -- a user isn't always sure what an
agent/task/safety combination will actually do before committing to it.
`Orchestrator::spawn_preview` builds the same id/branch/path
`create_workspace` would (via `WorkspaceManager::preview_workspace_location`,
refactored out of `create_workspace` so both paths generate an id the
same way), detects package managers against the *repo root*, not a
not-yet-created workspace path (a fresh worktree starts as a clean
checkout of `HEAD`, so this is a fair approximation unless the repo root's
own working tree has uncommitted package-manager-file changes that
wouldn't carry over), and calls the real `AgentAdapter::build_command` so
the printed command can never drift from what a real spawn would launch.

That last part has a side effect to account for: `build_command` for the
Claude Code, Copilot, and Gemini adapters (not Codex, which inlines its
MCP config as `-c` flags instead) unconditionally writes the MCP
coordination config to `coord.config_path` as part of building the
command, so the printed `--mcp-config <path>`/`--additional-mcp-config
<path>` argument is real. `spawn_preview` deletes that file immediately
after building the command, rather than changing `build_command`'s
signature across every adapter just for this -- the alternative (a
`write_config: bool` on the trait) was rejected as a bigger surface
change than a single `remove_file` call justified. `state_dir`'s
subdirectories (`workspaces/`, `mcp/`, `meta/`, `locks/`) still get
created by `WorkspaceManager::open` regardless of `--dry-run` -- that's
existing `open` behavior, not something this issue introduced, and
they're confirmed empty afterward (no real workspace, no lingering MCP
config) by `crates/pact-cli/tests/spawn_dry_run.rs`.

### Shell completions (issue #19)

`pact completions <shell>` calls `clap_complete::generate` directly
against `Cli`'s own `#[derive(Parser)]` definition (via
`<Cli as clap::CommandFactory>::command()`), so the generated script can
never drift out of sync with the real flag/subcommand set the way a
hand-maintained completion script would. Handled as an early return in
`main`, before `repo_root`/`Orchestrator::open` -- same reasoning as
`McpServe`, but for a different reason: completions must work from
anywhere, not just inside a git repo, since a user configuring their
shell's completion path has no reason to be standing in one. Confirmed
by hand, not just "the script generates without error": sourced the real
generated bash script and called its completion function with the exact
positional arguments (`$1`/`$2`/`$3`) bash's own completion machinery
passes, and `pact spawn --ag<TAB>` correctly completed to `--agent`.

### `pact doctor` (issue #18)

Reuses `pact_deps::run_shimmed` (`pact-deps`'s existing `cmd /C`
Windows-shim-resolution helper, re-exported for this rather than
re-derived) to run each tool's real version-check invocation and reports
found/not-found per item. Every check but `git` uses `--version`; `go` is
the one deliberate exception (`go version`, a subcommand, not a flag --
confirmed by hand: `go --version` actually fails with `flag provided but
not defined: -version`, so assuming a uniform `--version` convention
across every tool would have been wrong for at least this one). A
program not on `PATH` was confirmed, not assumed, to make
`run_shimmed`/`cmd /C` return a failed exit status with an "is not
recognized" stderr message rather than erroring the Rust call itself, so
"not found" is a normal `Ok` result, not a caught error.

`git`'s check additionally parses `X.Y` out of `git version X.Y.Z...` and
requires `>= 2.5` (when `git worktree` was introduced) to report
worktree support -- unparseable version strings are treated as "can't
confirm, assume fine" rather than a false failure, since a `git` that
responds to `--version` at all is already almost certainly new enough in
real use. `git` is the only check that can make the command exit
non-zero; every agent CLI and package manager is purely informational,
per the issue's own acceptance criteria -- not everyone needs all of
them, so a missing `copilot` or `poetry` isn't a failure the way a
missing `git` is.

### `find_repo_root` home-directory guard (issue #136)

Found incidentally during #123's error-hint work: `find_repo_root` walks
up from the current directory looking for the first `.git`, with no
upper bound but the filesystem root. A user running `pact` from a
directory nested under an *unrelated* git repo -- most plausibly a
dotfiles-manager home-directory repo (chezmoi, yadm, and similar all
work exactly this way) -- silently got that repo's `.pact` state
instead of an error, with no signal anything unexpected happened.
Reproduced by accident in this project's own dev environment: the home
directory happens to be an unrelated git repo, and running `pact list`
from a plain temp subdirectory under it walked all the way up and
picked it.

`unexpected_repo_root_warning` warns (never blocks -- an intentional
home-directory-as-repo setup is plausible, just unusual) on either of
two cheap signals: the found root is exactly `dirs::home_dir()`, or the
walk went up `>= 4` directories to find it. The home-directory check is
precise and covers the concrete failure mode that was actually hit; the
levels-up check is a broader, fuzzier catch-all for the same class of
mistake (an unrelated ancestor repo further from home). `--repo`
explicit is the documented way to silence the warning by being
unambiguous.

### Fake-agent end-to-end harness (issue #157)

Every existing agent-invoking test stubs the agent out entirely --
`ArbiterResolver`'s closure in `pact-vcs`'s tests never spawns a process,
and `pact-core`'s own tests inject a fake resolver the same way. That
leaves the real `spawn -> stream stdout -> AgentAdapter::parse_line ->
commit -> merge/conflict -> teardown` loop, including the actual
process-spawning code in `pact_agents::run_and_stream`, unexercised by
anything short of a real (paid, potentially flaky) agent CLI call --
which `CLAUDE.md` rules out for tests outright.

`crates/pact-cli/src/bin/fake_agent.rs` is a second `[[bin]]` in the
`pact-cli` package (auto-discovered from `src/bin/`, no `Cargo.toml`
change needed -- and the only way `CARGO_BIN_EXE_fake_agent` is reliably
available to `pact-cli`'s own integration tests, since Cargo only
guarantees that env var to tests in the *same* package as the binary).
It impersonates the Claude Code adapter specifically (the only one of
the four this binary stands in for -- "1-2 fake agents, not all four
adapters," matching the issue's v1 scope): it reads its `-p` argument as
a JSON `Script` (`{"writes": {...}, "sleep_ms": _, "summary": _,
"success": _, "exit_code": _}`, all fields optional) instead of a
natural-language instruction, performs the scripted file writes relative
to its CWD (the real worktree `run_and_stream` launches it in), sleeps
if asked, and prints Claude Code's real `stream-json` schema (`system`/
`init` then `result`) so `claude_code::parse_line` parses it exactly as
it would genuine output. `parse_script` also tries extracting the first
`{...}` substring before giving up to a no-op default -- Arbiter wraps a
workspace's original task text inside a larger natural-language prompt,
so the raw `-p` value isn't pure JSON in that path.

Tests in `crates/pact-cli/tests/fake_agent_e2e.rs` copy the compiled
`fake_agent` binary onto a scratch `PATH` entry as `claude`/`claude.exe`
(`shim_dir`), then run the real `pact` binary
(`env!("CARGO_BIN_EXE_pact")`) with `--agent claude` against a real
throwaway git repo -- so `spawn`/`spawn-many`/`merge-all`/`teardown` all
run through their actual code paths, just with a scripted, free,
instant "agent" on the other end of the pipe. This is the harness that
caught issue #178 (`list_workspaces` crashing on `-deps.json`/`-run.json`
sidecar files) -- the first test to drive a real `spawn -> list` round
trip, something no closure-stubbed test could have exercised.

**Deliberately out of scope for this pass**: Arbiter-specific e2e
scenarios (a fake agent invoked *as* the conflict resolver, verifying
Arbiter's own prompt-wrapping and scope-enforcement against a real
subprocess round trip). Scripting a fake agent to produce a distinct
"correct resolution" versus the original conflicting edit, through
Arbiter's prompt-wrapping of the task text, adds enough complexity to
warrant its own follow-up rather than folding it into this harness's
first pass -- `ArbiterResolver`'s closure-based unit/integration tests
already cover Arbiter's decision logic; what's missing here is only the
real-subprocess round trip, which is lower-value for Arbiter
specifically since its prompt construction (not its process-spawning) is
where most of its bugs have actually been.

## CI and release infrastructure

### Rolling `edge` release

`release.yml` only builds on a pushed `v*` tag -- deliberately manual and
infrequent, matching the "cut a release when a headline feature merges"
cadence. The gap that leaves: real behavioral work lands on `main`
between tags (16 commits' worth, at one point, all real fixes) with no
installable build for anyone without a Rust toolchain to test against,
which is exactly the audience the prebuilt-binary release path exists
for in the first place.

`edge-release.yml` closes that gap without touching `release.yml` at
all -- a second, additive workflow, same build matrix, triggered on every
push to `main` (plus manual `workflow_dispatch`) instead of a tag push.
Named `edge`, not `nightly`: it fires on every push, not on a daily cron,
so "nightly" would misdescribe the actual cadence. The `edge` git tag is
force-moved to the new commit each run (`git tag -f edge && git push
origin edge --force`) before `softprops/action-gh-release` republishes
the release at that tag with `prerelease: true` -- that action updates an
existing release in place (replacing same-named assets) rather than
requiring a new tag per run, which is what makes a single rolling release
possible instead of accumulating one release per push. `concurrency:
cancel-in-progress` on the workflow avoids overlapping runs stepping on
each other if pushes land in quick succession.

Considered and rejected: adopting `cargo-dist` wholesale (the ecosystem-
standard tool for this, with mature prerelease-version handling) --
real value for a project wanting installer scripts, checksums, a
Homebrew tap, but it replaces `release.yml` with its own generated
workflow and config surface, a bigger lift than this problem justified
at pact's current size. Also considered: pointing users at raw CI
artifacts from the latest `main` run instead of a release -- no new
workflow needed at all, but artifact downloads require GitHub auth even
on a public repo, and have a 90-day retention window, making the
Releases page a meaningfully better discovery path for the same
information.

### `edge` build version string (issue #86)

An `edge` binary's `--version` used to print the plain `Cargo.toml`
version (e.g. `pact 0.2.0`) -- identical to the last tagged release,
since `CARGO_PKG_VERSION` isn't bumped between tags. Found during an
outside R3 shakedown: no way to tell an `edge` download apart from a
real release, or recover which commit it was built from, after the fact.

`pact-cli/build.rs` reads `PACT_EDGE_SHA` (an env var, unset for normal
builds) and emits `cargo:rustc-env=PACT_VERSION=<version>[-edge.<short
sha>]`; `Cli`'s `#[command(version = env!("PACT_VERSION"))]` uses that
instead of clap's default `CARGO_PKG_VERSION` wiring. `edge-release.yml`
sets `PACT_EDGE_SHA: ${{ github.sha }}` on the build step; `release.yml`
sets nothing, so a tagged build's `PACT_VERSION` falls straight through
to the plain `CARGO_PKG_VERSION` with no behavior change. Confirmed by
hand: building locally with `PACT_EDGE_SHA` set produces `pact
0.3.0-edge.e4ef6a0`; building without it produces the unchanged `pact
0.3.0`.

A build script over a `const fn`/`option_env!` match was necessary, not
just convenient -- `option_env!` alone can't format a runtime string (no
owned-`String` concatenation in a `const` context without a crate like
`const_format`), so computing the final string at build time and handing
it to the binary via `env!` was the simplest path that needed no new
dependency.
