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
forever if the holder died." `pact-deps` used to reuse it verbatim to
guard concurrent population of a shared dependency store entry -- gone
along with that store (issue #233); `PidLock` today guards git worktree
operations only.

### Lock timeout: 30s -> 30min (issue #230)

The 30s `LOCK_TIMEOUT` used to guard `git worktree add`/`remove` was tuned
as an arbitrary "don't hang forever" ceiling, not derived from anything
real. A real production `spawn-many` run (497-file repo, 5 concurrent
tasks) hit it: `git worktree add` took ~9s per checkout on that repo, so
the 5th task's wait for its turn (~4 waiters * 9s = 36s) exceeded 30s, the
lock acquisition failed, and the task silently never became a workspace --
no error text reached the report's captured output (see issue #8, the
same run's orphaned-process terminal wedge, for why). The failure
condition is `(N-1) * checkout_seconds > 30`, which is a cliff that gets
easier to hit as either N or repo size grows, on a codebase where nothing
was actually stuck.

Re-verified empirically before changing anything: 60 concurrent, unlocked
`git worktree add -b` calls across 3 rounds (12-20 way) against a 3000-file
repo on git 2.46/Windows produced zero failures and `git fsck` came back
clean -- modern git does not appear to need this serialization for
distinct paths/branches on this platform. Removing the lock entirely
(rather than just fixing the timeout) was considered and rejected for this
pass: the original anthropics/claude-code#34645 citation above predates
this verification, was not re-checked against current git on macOS/Linux
(unavailable in this environment, same standing limitation as issue #6),
and the actual reported harm was the timeout misfiring on ordinary
contention, not that the lock itself was slow or wrong. Widening the
timeout fixes the reported bug without reversing a documented safety
decision on unverified ground.

`LOCK_TIMEOUT` is now 30 minutes. The reasoning: `PidLock`'s stale-lock
stealing (liveness + start-time check, see below) already handles the
"holder crashed" case before this timeout is ever consulted -- the only
scenario left for this timeout to guard is a holder that is alive but
genuinely stuck, which is rare and deserves a generous window, not a
budget sized to one worktree checkout. When it does fire, the error is
still a hard, attributed, non-swallowable failure (`create_workspace`
returns `Err`, `spawn_many` surfaces it per-task, the CLI prints `task #N:
failed before/during launch: ...` and exits 1) -- see issue #231 for
making that reconciliation a structural invariant instead of relying on
this loop being correct.

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
`--dry-run`'s preview calls the exact same `workspace_id` function a real
spawn does. **This claim turned out to be misleading, not false but not
sufficient either** -- see issue #234 directly below: calling the *same*
function twice is not the same as it returning the *same value* twice,
and `workspace_id`'s random suffix meant it never did.

### Workspace names: `--name` (issue #234)

A real production report found the #122 scheme above unusable in
practice for two compounding reasons, both traced to the random suffix:
(1) `--dry-run`'s preview id and the real run's id could never agree,
since `short_id()` returns a fresh `Uuid::new_v4()` on every call -- the
same function, called twice, is not the same as it returning the same
value twice; (2) a batch of tasks sharing a long common preamble (a house
style, a templated instruction block) slugify to an identical 32-char
prefix, so every workspace in the batch looked the same except for its
opaque suffix -- exactly the scenario `slugify` was already known not to
solve on its own (see above), now confirmed in the wild, not just reasoned
about.

`workspace_id(task, name: Option<&str>)` takes an explicit name now.
When given, it drives the id **directly**: `slugify(name)`, no random
suffix at all. This is deliberately not "slug from name plus a random
suffix" -- an explicit name is exactly what the caller asked for, and a
suffix would reintroduce the same dry-run/real-run mismatch #234 exists
to fix. The tradeoff: two tasks given the same `--name` within one batch
collide on id/branch, which fails loudly at `git worktree add -b`
("branch already exists"), not silently -- accepted deliberately, and the
CLI additionally checks for a duplicate `--name` up front (before ever
spawning a thread) so the failure is immediate and clear rather than a
git error surfacing from inside a background task.

`spawn --name <name>` (one task) and `spawn-many --name <name>`
(repeatable, positionally matched to `--task` in the same order) both
require *no* name or *exactly one name per task* -- a partial count is
rejected rather than guessed at. `--name` must contain at least one ASCII
alphanumeric character (validated before `slugify` ever runs), so a
caller can't accidentally end up with the confusing empty/opaque id
`slugify` would otherwise produce.

**Deliberately not implemented**, per the original report's own
prioritization ("(c) ... I would not bother if (a) lands"): a
deterministic hash-derived suffix for the *default*, no-`--name` path.
That would only partially fix the parity problem (two separate process
invocations -- a `--dry-run` now, a real run later -- still can't
coordinate without a shared run id neither one currently has reason to
generate), and an explicit name is the actual fix for the case where
parity or readability actually matters. The default scheme (task-slug
plus random suffix) is unchanged for every caller not using `--name`.

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

1. Auto-commit every selected workspace via `commit_all` -- a workspace
   whose auto-commit fails is removed from the batch here and recorded
   as `skipped` (issue #194, see below), not just logged and left in.
2. Moving-base check -- refuse a workspace whose recorded `base_commit` is
   no longer an ancestor of current HEAD, so merging never silently
   assumes a fork point that isn't real anymore (e.g. HEAD was reset since
   the workspace was created). A workspace whose changes can't be sized in
   the next phase (e.g. `workspace_changes` failed) sorts last rather than
   being dropped, so a bug in sizing never silently excludes it.
3. Sequence the rest by risk score (`merge_risk_score`, issue #159) --
   changed-file count plus a penalty for touching a central file or
   lockfile, ascending -- on the theory that landing small, low-risk
   changes before a bigger or more central one reduces cascade
   conflicts. Plain changed-file count alone (the original heuristic)
   still anchors the score; the penalty only pulls a *smaller* but
   riskier change later than a same-or-larger plain one, not the reverse.
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

**A real, serious bug in phase 1 found and fixed (issue #194, outside R4
regression report, 2026-07-29): a false-green summary when auto-commit
fails.** The phase-1 loop logged `"...leaving it out"` on a `commit_all`
failure but never actually removed the workspace from the batch. Since
the intended changes never landed as a commit, that workspace's branch
had nothing new for `git merge` to do -- a trivial, conflict-free
no-op that `merge_all` reported as a normal `merged <id>`, exit `0`, on
a branch silently byte-identical to base. The original report hit this
across all 10 agents in one real 10-agent Windows run (`git add`
failing on a filename-too-long error, `node_modules` not in
`.gitignore`) -- a real data-loss shape: a green CI-friendly summary
that actually merged nothing. Fixed by recording the failure as a
`SkippedWorkspace` (the same structured pattern the moving-base check
already used) before removing it from `selected`, so it shows up in
the CLI's "skipped -- needs a human" section and flips the exit code
to `2`. Verified with a real integration test that forces a genuine
`git commit` failure via a `pre-commit` hook that always exits
non-zero (portable, not relying on a Windows-specific error), then
asserts the target branch's tree is byte-identical to base *and* the
workspace shows up in `report.skipped` -- confirmed as a real
regression test by temporarily reverting the fix and watching it fail.

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
same single file, they tie on the risk-score heuristic, so which one
merges first (and therefore which one the *other* conflicts against)
isn't specified -- the test asserts that exactly one of them merged,
not which one.

### Risk-aware merge ordering (issue #159)

From an outside code review (2026-07-24, triage discussion): plain
changed-file count treats a one-file change to `package.json` the same
as a ten-file isolated feature, when the former is often the riskier
change to land in the middle of a batch. Marked "design first" at the
time -- the weighting scheme needed a decision the review flagged but
didn't resolve (what counts as "central," how much weight per factor).
Picked sensible defaults this pass rather than leaving it unbuilt
further, on the reasoning that none of it is a public API commitment
and is easy to retune later if the weights turn out wrong in practice.

`merge_risk_score(files) -> usize`: base score is still the plain
changed-file count (the original heuristic was directionally right,
just too coarse) -- `+3` per file whose basename is a common package
manifest, schema-adjacent, or barrel/router entry point
(`CENTRAL_FILE_BASENAMES`: `package.json`, `Cargo.toml`,
`pyproject.toml`, `go.mod`, `requirements.txt`, `Gemfile`,
`composer.json`, `index.{ts,tsx,js}`, `router.ts`, `routes.ts`), `+5`
per lockfile (reusing `is_never_auto_resolve`'s existing list -- lockfiles
already get the strongest "don't touch casually" treatment elsewhere in
this codebase, for the same underlying reason). Deliberately
basename-based, not path-aware or content-aware: this can't know a
repo's own conventions (a project's real barrel file could be named
anything), so it only catches the common, cross-ecosystem cases -- a
first-cut heuristic, not an attempt to model real conflict probability.

**Deliberately deferred, not half-built**: the triage's other proposed
factors --`overlap_with_other_workspaces`, `prior_conflict_history`,
`active_claim_overlap` -- would require `pact-vcs` to depend on
`pact-core`'s Weaver prediction and/or `pact-coord`'s operation log and
live lease state, a real cross-crate architectural change, not a weight
tweak. Left out of this pass rather than bolted on awkwardly just to
check the box; the four-factor file-content-only version above is a
complete, self-contained improvement on its own.

`--dry-run` now shows each workspace's computed risk score alongside
the planned order (`MergeReport::planned` changed from `Vec<String>` to
`Vec<PlannedWorkspace { id, risk_score }>`), so the reasoning behind the
order is inspectable, not just the order itself -- confirmed for real
via a `--dry-run` preview showing a workspace touching three plain
files sequenced *before* one touching only `package.json`, despite the
`package.json` workspace changing fewer files overall.

### Overlap-aware risk scoring (issue #236)

A real production run's own `--dry-run` output showed the same limitation
this document's "deliberately deferred" note above already anticipated,
confirmed in practice: four workspaces each touching one key of the same
`package.json` all scored `4` (`1` file `+3` central-file penalty),
identically -- "least risky first" had degenerated to plain creation
order, and the `--dry-run` output presented that as if it were a real
decision.

**The prior deferral reasoning turns out to have been more conservative
than necessary.** The note above assumed `overlap_with_other_workspaces`
would need `pact-vcs` to depend on `pact-core`'s Weaver prediction and/or
`pact-coord`'s live lease state -- a real cross-crate change. It doesn't:
`merge_all` already computes every selected workspace's own
`workspace_changes` locally, within `pact-vcs`, before sequencing them.
Computing each workspace's overlap against the *union of every other
selected workspace's changed files in the same batch* needs nothing
beyond data `merge_all` was already gathering, just gathered once up
front instead of one workspace at a time.

`merge_risk_score(files, other_files: &HashSet<String>)` gained a third
term: `+10` (`OVERLAP_PENALTY`) per file `files` shares with
`other_files` -- deliberately dominant over the existing `+3`/`+5`
central-file/lockfile penalties and any realistic plain file count, so a
workspace sharing anything with the rest of the batch reliably outranks
one that doesn't, matching the report's own proposed model ("merge the
workspace with fewest overlapping files against all others first").

**Still file-level, not hunk-level** (the report's proposed direction
(b), not attempted here): four workspaces editing disjoint regions of
one file (genuinely low risk) and two editing the same function
(genuinely high risk) both score as "shares this file with N others"
identically -- overlap-aware scoring narrows the gap `merge_risk_score`
can't close (four *different* files scoring identically is now fixed;
four edits to the *same* file needing hunk-level analysis to
differentiate is not, and would need real diff/hunk data this heuristic
deliberately doesn't compute). Because ties are therefore still possible
for a realistic batch (several workspaces genuinely sharing the exact
same one file), `merge-all --dry-run` now detects the all-scores-equal
case explicitly and says the order shown is creation order, not a risk
ranking -- honest about the heuristic's limits instead of presenting a
meaningless order as a decision.

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

### Gate diagnosability, and the environment-vs-code distinction (issue #232)

A real production run found `--require-passing-tests` reporting "merged
cleanly but failed the required test command" for every workspace in a
batch, in 27 seconds for a suite that normally takes 155s and passes.
Root cause: the integration worktree the gate runs in has no dependencies
installed (no `pact_deps::prepare()` call anywhere in that path -- issue
#233 covers actually fixing that, coupled to this one), so the command
couldn't run at all -- and `run_shell` returned a bare `Result<bool>`
that collapsed "your code failed the gate" and "the environment couldn't
even run the command" into the identical false signal, while also
capturing and then discarding the one thing (stdout/stderr) that would
have told a real user the truth in seconds.

Two changes, deliberately independent of actually fixing #233's
dependency-availability problem:

**Pre-flight, before merging anything.** `merge_all` now runs the gate
command once against the freshly-created integration worktree at the
unmodified base commit (`head`, before any workspace's branch is
touched), before entering the per-workspace merge loop. If it fails
*there*, no workspace's changes are on trial -- the whole `merge_all`
call aborts (`bail!`, integration worktree and scaffolding branch cleaned
up first) with a diagnosis explaining the environment, not any code
change, is the problem, and that no workspaces were merged. This is
cheap, general, and language-agnostic: it doesn't know or care *why* the
environment is broken (missing deps, wrong working directory, whatever),
it just refuses to blame workspace code for a command that was never
going to pass regardless of what any workspace did.

**`run_shell` returns `GateOutcome`, not `Result<bool>`.** Carries
`success`, `exit_code`, `duration`, and `output_tail` (last 20 lines of
combined stdout+stderr -- deliberately combined rather than
stderr-only, since most real test runners write failure detail to
stdout). Both the pre-flight check and the existing per-workspace gate
use `GateOutcome::diagnosis()` to put this in the abort message /
`SkippedWorkspace.reason` respectively, instead of silently discarding
the one artifact that explains the failure.

**Test fixtures fixed alongside this, per issue #11's own critique:** the
existing `require_passing_tests` tests used `always_fail_cmd()`
(`exit 1`/`false`) to represent "your code broke the gate" -- but an
unconditionally-failing command is now, correctly, indistinguishable
from a broken environment (both fail on the unmodified base), so it
would trip the new pre-flight abort instead of the per-workspace skip
path the tests expected. Replaced with a content-based gate (fails only
once a specific file the workspace under test introduces actually
exists -- passes cleanly on the untouched base) that can genuinely tell
the two cases apart, matching what a real gate command does. `PidLock`'s
same pattern from issue #71: a fixture shaped to make the test pass isn't
the same as a fixture shaped to catch the real bug.

Deliberately not done in this pass: actually running `pact_deps::prepare`
(or a hardlinked/shared equivalent) on the integration worktree before
the gate, which would make `--require-passing-tests` genuinely usable
for a dependency-needing project rather than just correctly diagnosing
why it can't run yet. That's issue #233's shared-content-store fix --
wiring dependency prep into `merge_all` before #233 lands risks
reintroducing #233's own lock-contention stall directly into the merge
path. Sequenced as its own follow-up once #233 ships.

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

### Weaver: negation and brand-name false positives (issue #239)

The "an occasional false positive costs nothing worse than one harmless
extra line" reasoning above turned out wrong in practice, confirmed by a
real production run, not just a hypothetical: **every** task's prompt
said "do NOT modify any `package-lock.json`" (careful, well-written
prompts naming the files they're avoiding), and the heuristic -- blind to
polarity -- flagged `package-lock.json` as a possible overlap across all
five tasks anyway. Combined with every task's shared "this is a Next.js
app" preamble also getting flagged (the exact `next.js`-as-a-file case
this document already predicted), the one true positive
(`package.json`) landed in the middle of two false ones. A heuristic
that fires hardest on the best-written prompts, and buries its one real
finding in noise, is worse than the "costs nothing" framing assumed.

Two fixes, both in `extract_file_tokens`:

- **Negation.** `split_into_clauses` splits task text on `,`/`;` and a
  *sentence-final* `.`/`!`/`?` (one followed by whitespace or
  end-of-string -- deliberately not a plain char split, which would also
  cut a filename's own internal dot, turning `package-lock.json` into
  `package-lock` and `json`). Any clause containing a negation cue
  (`not`, `never`, `avoid`, `without`, `except`, `no`, or a `n't`
  contraction) is skipped entirely for token extraction. Coarser than
  "strip only the text after the cue within the clause" (the issue's
  literal suggestion), but the negation cue is reliably near the start
  of its own clause in practice ("do NOT modify X"), and skipping the
  whole clause is simpler and can't under-strip.
- **Brand names.** `looks_like_brand_name`: a candidate with no `/` whose
  stem is Title Case (capital first letter, all-lowercase rest --
  `Next`, `Node`, `React`) is treated as a product name, not a file.
  Deliberately narrow: a path with a `/` is never mistaken for a bare
  brand name regardless of case, and a fully-uppercase conventional
  filename (`README.md`, `LICENSE.md`) doesn't match the Title Case
  shape, so it still gets caught correctly.

Both are still text heuristics with real, known blind spots --
`clause_is_negated` triggers on the word "no" appearing anywhere in a
clause for any reason, not just as syntactic negation of the file
mention, and `looks_like_brand_name` would just as happily reject a
real, deliberately Title-Case-named file if a repo used that
convention. Accepted for the same reason the original heuristic's
false-positive tolerance was accepted: `predict_task_overlap` is
advisory only, never blocks anything, and `pact conflicts`' real
git-diff-based detection remains the mechanism that actually matters.

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
spend was the brief for this session). **Resolved in a later pass** --
see "Arbiter merge-state neutralization (issue #185)" below for the fix
and its own live re-verification (4/4 real successes, zero denials).

Every rejection path -- old or new -- falls back to the same existing
safety net regardless: the workspace stays a normal skipped/persisted
conflict, resumable via `pact resolve`. This redesign makes Arbiter
*capable* of succeeding against a real agent for the first time, a real
and verified improvement over a 0% success rate -- it does not yet make
every attempt succeed.

### Arbiter merge-state neutralization (issue #185)

Follow-up to the previous section's leading hypothesis, not chased
further at the time: `neutralize_conflict`'s per-file index fix cleared
one file's own "UU" status, but the *repository as a whole* was still
genuinely mid-merge (`.git/MERGE_HEAD`/`MERGE_MSG`/`MERGE_MODE` present)
for the duration of the attempt, since the merge as a whole hasn't been
committed or aborted yet -- by design, Arbiter needs to still be
"inside" the conflict to resolve it. `WorkspaceManager::
neutralize_merge_state`/`restore_merge_state` extends the same
snapshot-then-restore pattern to that repo-level state: reads and
removes `MERGE_HEAD` (and `MERGE_MSG`/`MERGE_MODE` if present) from the
worktree's *own* git-dir -- resolved via `git rev-parse --git-dir`
rather than assumed as `.git/worktrees/<name>` by convention, since a
non-worktree checkout has no `worktrees/` subdirectory at all -- and
`Ok(None)` when the worktree isn't actually mid-merge.

**Restored unconditionally, not just on rejection** -- unlike
`neutralize_conflict`/`restore_conflict`, which stay neutralized on
acceptance (the file is genuinely no longer conflicted, that's the
desired end state). `MergeStateSnapshot` is different: `merge_branch_into`
always runs `git commit --no-edit` (accepted) or `git merge --abort`
(rejected) immediately after Arbiter returns, and both need `MERGE_HEAD`
present to behave as a real merge conclusion rather than a plain commit.
`attempt_arbiter_resolution` in pact-core wraps the entire attempt (the
pre-existing function, renamed to `attempt_arbiter_resolution_inner`) so
every exit path -- accepted or rejected -- restores the merge state
before returning control to `merge_branch_into`.

Covered by `crates/pact-vcs/tests/arbiter_conflict_prep.rs` against a
real conflicted repo: `MERGE_HEAD` actually disappears and the
mid-merge banner clears once both the per-file and repo-level state are
neutralized together; restoring puts the exact original `MERGE_HEAD`
bytes back and a real `git commit --no-edit` still succeeds afterward
(the same sequence `merge_branch_into` performs); a no-op when the
worktree isn't mid-merge at all.

**Live re-verified, real spend, resolved.** 4 independent real
`claude`-as-Arbiter attempts against the same reproducible conflict
shape as the original investigation (two branches each adding a
different function -- `add`/`multiply` -- to the same `math.js`, right
after an existing `subtract`, forcing a conflict at both the function
insertion point and the `module.exports` line): **4/4 full successes**,
zero `Write` denials. Each was a genuinely fresh repo/conflict (not the
same one retried), spawned via real `claude` agent calls for both
conflicting edits, then a real `merge-all --arbiter-agent claude
--test-cmd "node -e \"require('./math.js')\""` invocation. Every
resulting merge was inspected directly (`git show`), not just trusted
from exit code: all four contained both functions, correct syntax, and
a correctly merged `module.exports` in every case. Compare to the
pre-fix baseline this issue itself established: 1 success, 3 rejections
in 4 attempts, one of which was exactly this repo-level-state denial.
Zero for four against three-for-four is a real, verified improvement,
not a coincidence at this sample size given the fix's mechanism directly
addresses the hypothesis that was tested and confirmed.

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

**False-positive on the canonical fan-out workflow, fixed (issue #199, outside R5 report):**
`validate_arbiter_scope`'s out-of-scope check compared post-run `git
status --porcelain` only against the conflicted-files list -- it never
accounted for files git's own 3-way merge had *already* auto-carried
into the working tree from THEIRS before the arbiter agent ran at all
(an add-only file with no OURS counterpart merges cleanly, no conflict,
no arbiter involvement). Real repro from the report: 4 fan-out agents
each add their own `plugins/<name>.js` + `tests/<name>.test.js` and
touch a shared `app.js` registration block -- exactly the primary
README workflow. `merge-all` correctly lands one clean, leaves three
conflicted on `app.js` only; every `pact resolve` attempt on those three
rejected with "changed files outside the conflicted-file list", listing
the *other* workspaces' own `plugins/*.js`/`tests/*.js` files that git's
merge had auto-added, not anything the arbiter agent touched. Under the
Write-fresh redesign this meant `pact resolve` rejected on every attempt
against the canonical workflow -- the pre-#187 arbiter was the one
actually landing conflicts in that report's own testing.

Fix: `attempt_arbiter_resolution` now snapshots `changed_paths` *before*
`neutralize_conflict`/the agent run (`baseline_changed`), and
`validate_arbiter_scope` excludes that baseline from the post-run diff
alongside the conflicted-files list itself -- isolating "the arbiter's
model changed this" from "git's merge already left this dirty." Two new
tests cover both directions: a pre-existing baseline change is ignored,
and a change that only appears *after* the agent ran (not in the
baseline) is still correctly rejected -- the fix narrows the check, it
doesn't disable it.

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

**`coord_status` can go stale mid-session, confirmed by design, not a bug
(issue #201, outside R5 report):** an outside tester killed the
`mcp-serve` sidecar 8s into a 66s run and found `coord_status:
"connected"` still reported at the end, expecting something like
`"disconnected"`. Investigated and confirmed this is the field's actual,
already-documented semantics working as intended, not broken wiring:
`coord_status` is a passive relay of the *last* `AgentEvent::CoordStatus`
the agent CLI itself chose to report via its own event stream -- pact
never independently polls the sidecar's liveness because pact doesn't
own that process at all. The agent CLI spawns `mcp-serve` itself as its
own MCP client (per `--additional-mcp-config`/equivalent), so pact has
no process handle on it to check. Whether the field ever updates past
its first "connected" depends entirely on whether the agent notices the
pipe died -- which it only has occasion to do on its *next* coordination
tool call. A task that makes no further coordination call after a
mid-session sidecar death, like the report's repro, gives the agent
nothing to notice or report. Doc comment on the field tightened to spell
this out explicitly. A true independent liveness check would mean pact
supervising the sidecar itself instead of the agent CLI owning it -- a
real architectural change, not attempted here.

**`files_touched`: a ground-truth signal for "did the run actually do
anything," independent of the agent's own success claim (issue #212,
outside Windows Copilot report):** a task told an agent its target file
must already exist, and to exit non-zero if it didn't -- the file was
missing, Copilot's own prose said "I will exit with a non-zero code,"
but its `type: "result"` event reported `exitCode: 0` anyway. Root cause
isn't Copilot-specific: its contract (like Codex's `turn.completed`,
already documented above) has no task-semantic success channel at all,
only "did the CLI process exit cleanly." Rather than trying to override
`exit_success` with a heuristic (the reporter's own report flags this as
unreliable -- a legitimate read-only/inspect task also touches zero
files, so "zero files touched" alone can't safely demote a run to
failure without risking false negatives on correct runs), added a
separate, adapter-agnostic `files_touched: bool` computed from a real
`pact_vcs::changed_paths` check on the workspace right after the run --
ground truth, not any agent's self-report. Deliberately kept out of
`exit_success` entirely; `pact list` surfaces `[clean, no files
touched]` distinctly from plain `[clean]` (which still covers the
completely normal case of a workspace that's clean because it was
already committed/merged), and `pact inspect` prints an explicit note
alongside a "last run" that succeeded without touching anything.
Verified end-to-end via the fake-agent harness (issue #157): a
scripted no-op-but-successful spawn shows the distinct annotation, a
real scripted file write does not.

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

### `Phase` events and the end-of-run summary (issue #241)

A real production `spawn-many` run produced **zero output for ~22
minutes** during dependency prep -- indistinguishable, from the
outside, from a hang. On top of that, the run's final output was just
each workspace's raw "done: <summary>" line, no indication of how long
anything took or whether the shared npm content store had actually
helped.

**`AgentEvent::Phase(String)`** is the one variant in this enum that
doesn't come from an agent CLI's own output at all -- it's synthetic,
emitted directly by `pact-core`'s `spawn_with_supervisor` around each
phase of a single spawn: right before `create_workspace` ("creating
workspace"), right before and after `pact_deps::prepare` ("preparing
dependencies", then a summary of what happened), and right before
`run_and_stream` launches the agent ("running agent"). Printed
unconditionally by the CLI (`[phase] <text>`, or `[label:index] [phase]
<text>` under `spawn-many`) -- deliberately not gated behind
`--verbose`/`should_print_other` the way adapter chatter is, since the
entire point is a visible heartbeat during a phase that would otherwise
produce nothing at all.

This doesn't turn the 22-minute stall itself into fast progress -- that
was issue #233's job (the content-store lock timeout, fixed separately)
-- it turns 22 minutes of silence into "preparing dependencies" staying
on screen the whole time, which is enough to tell a stall from a hang.
A finer-grained live progress bar (bytes downloaded, percent complete)
was considered and left out: `pact_deps::prepare` has no natural
sub-phase hooks to report through without a much larger refactor of
`prepare_npm`'s own internals, and a static "still working on this"
marker already answers the actual complaint (no signal at all) without
that cost.

**The end-of-run summary** (`spawn-many`'s per-workspace listing) now
prints two more lines per successful workspace, both from data that
already existed but was never surfaced here: `duration: Ns` (from
`RunMetadata.started_at`/`ended_at`, issue #12) and `dependencies:
<manager>: <outcome>` (from `ManagerPrepReport`, issue #12/#160's
`store_hit`, via the new `dependency_summary_line` -- distinct from
`dependency_phase_summary` in `pact-core`, which renders the same data
for the *live* phase marker instead of the final listing, slightly
different wording for each context). Both come from data files that
already existed (`pact inspect <id>` could always show this) -- the
fix is presenting them by default at the point a real user is actually
looking, not requiring a second command per workspace after the fact.

**Deliberately not done, per the issue's own proposed direction (d)**:
writing the summary to `meta/` as JSON so it survives a wedged
terminal. `RunMetadata`/`ManagerPrepReport` already persist per-workspace
(exactly this data, just not aggregated into one file) and survive a
wedged terminal today via `pact inspect`/`pact status --json` after the
fact -- a *separate* aggregate summary file would duplicate that
persistence for marginal benefit over querying the existing sidecars,
not attempted here.

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

### Orphaned grandchild cleanup, and a deeper pipe-inheritance root cause (issue #237)

A real production `spawn-many` run found 7 `copilot` processes still
alive after `spawn-many` itself finished and `pact`'s own process had
already exited -- holding the terminal's stdout pipe open, so the
caller's shell never got its prompt back. `Supervisor` covered the
Ctrl-C path (above); there was no equivalent cleanup on the *normal*
exit path at all.

**What shipped:** `run_and_stream` now calls `kill()` on the tracked
`GroupChild` again immediately after `wait()` returns for the direct
agent process -- a no-op in the common case (nothing left to kill), and
a real sweep of any surviving group member when there is. On Windows,
the spawn also opts into `command_group`'s `kill_on_drop(true)` (a Job
Object flag, only exposed on Windows without the crate's `with-tokio`
feature, which pact doesn't enable): the OS itself kills every process
still in the job when the job handle closes, which happens automatically
even if pact's own process exits abruptly (crash, `SIGKILL`) rather than
normally -- a stronger guarantee than anything a userspace `Drop` could
give. Verified fast and real:
`crates/pact-agents/tests/orphan_cleanup.rs`'s
`killing_a_process_group_after_its_primary_already_exited_still_reaches_a_grandchild`
proves the exact call `run_and_stream` makes (`kill()` on a group whose
tracked primary has already exited) still reaches a real grandchild
process, reusing `group_kill.rs`'s proven parent/grandchild command
construction.

**What this does *not* fix, found and verified while building the test
above.** An adversarial test (`fake_parent_with_grandchild`, this
package's second `[[bin]]`) spawns a grandchild with its own stdio
explicitly set to `Stdio::null()`, then exits almost immediately --
modeling a real agent CLI's MCP-server sidecar. On Windows, that
grandchild still inherited pact's own piped stdout **write handle**: 
`std::process::Command`'s Windows implementation sets
`bInheritHandles=true` and duplicates *every* currently-inheritable
handle in the spawning process' table into the child, not just the three
explicitly configured stdio handles -- so the pipe's write end stayed
open in the grandchild's process table even though the grandchild's own
configured stdout was `Stdio::null()`. `run_and_stream`'s read loop
cannot see EOF on that pipe until *every* handle to its write end is
closed, so it blocked for the grandchild's entire remaining lifetime
(confirmed: `run_and_stream` took 120.57s to return against a 119s-
remaining `ping -n 120` grandchild -- not an approximation, the measured
number). By the time the post-wait `kill()` above runs, the grandchild
is already gone on its own; there is nothing left to sweep. This is
**worse** than "an orphan survives after pact exits" (the fix above
targets that) -- it's "pact's own process cannot finish running at all
until the grandchild does," which matches the original report's exact
symptom (the shell never returned a prompt) more precisely than the
orphan-survival framing the issue was originally filed under.

This is a real, previously-undocumented root cause, not a narrow edge
case: it reproduces with the *simplest possible* grandchild (a single
`ping`/`sleep` invocation via plain `std::process::Command`, no shell
tricks), meaning it's the **default** Windows behavior for any
subprocess an agent CLI spawns normally, not something specific to
Copilot's own implementation. `command_group`'s job-object wrapping
isn't the cause -- it spawns via ordinary `std::process::Command::spawn`
(confirmed by reading `command-group`'s own source) and assigns the
result to a job afterward; the handle leak happens one level up, in how
Rust's std itself creates the piped `Stdio` and hands its write end to
the direct child.

**Fixed in issue #253, and not the way this section originally proposed.**
The first idea considered -- replacing the anonymous pipe with a named
pipe opened non-inheritable, or scoping what the direct child inherits via
`PROC_THREAD_ATTRIBUTE_HANDLE_LIST` -- turned out not to actually solve
this on its own: Windows preserves a handle's inheritable flag across
inheritance, so a handle that reaches the agent process still arrives
marked inheritable *inside* the agent's own handle table, and the agent's
own subsequent `CreateProcess` calls (however it spawns its sidecar) can
still hand it down to a grandchild regardless of how carefully pact
scoped the original grant. Actually preventing that would mean pact
performing handle surgery on a process it doesn't own the code of --
spawning the agent suspended, remotely duplicating its stdout handle to a
non-inheritable copy via `DuplicateHandle` across process boundaries, and
patching the still-suspended process's own cached handle value before
resuming its first thread. That's real, documented Win32 capability, but
it leans on internal-layout assumptions (`RTL_USER_PROCESS_PARAMETERS`)
beyond what a small Rust CLI should take on for this.

Instead, the fix accepts that the handle may leak and stops caring: OS
handle security can restrict what the *direct* child receives, but can't
stop that child from re-exposing an already-inherited handle to its own
children, so `run_and_stream` no longer treats "the pipe hit EOF" as the
signal that the run is over. Both stdout and stderr are read on detached
background threads (not joined -- joining would just move the hang from
the main loop into the join call); stdout lines are pushed through an
`mpsc` channel, and the main loop calls `recv_timeout` in a
`CHILD_EXIT_POLL_INTERVAL` (50ms) loop, racing each line against a
non-blocking `Supervisor::try_wait` poll of the *direct* agent process.
The loop exits as soon as that direct child exits, whether or not the
pipe has actually seen EOF -- a lingering grandchild's copy of the write
handle is no longer this process's problem to wait out. The existing
post-wait `kill()` sweep (above) still runs afterward and still reaches
the grandchild in the common case (it's still in the same job/group), so
nothing is left orphaned either.

Verified by turning the adversarial test from a documented limitation
into real regression coverage:
`run_and_stream_returns_promptly_despite_a_windows_grandchild_that_
inherited_the_stdout_pipe` asserts `run_and_stream` now returns in well
under 10 seconds against the same `fake_parent_with_grandchild` (previously
measured at 120.57s), *and* that the grandchild marker process is gone by
the time the function returns -- both the hang and the orphan are covered
by one test.

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

### Windows multi-line prompt truncation (issue #210)

From an outside Windows Copilot stress-test report: every agent spawn on
Windows used to be wrapped in `cmd /C <program> <args...>` (the same
rationale as `cmdutil::run`'s Windows `.cmd` shim resolution, see
`pact-deps`'s own section on this) -- but `cmd.exe`'s own command-line
reader treats a raw embedded newline as ending the current line, **no
matter how the argument is quoted**. Confirmed by hand with a harmless
local `.cmd` shim (not a real agent call): a 2-line task prompt through
`cmd /C echoargs -p "<2-line-text>" --output-format json --allow-all-tools`
arrived at the child as exactly `["-p", "<line 1 only>"]` -- both the
rest of the prompt *and every flag after it* silently vanished. Since a
multi-line task prompt is a very common shape (numbered steps,
multi-sentence instructions), this silently dropped `--allow-all-tools`/
`--additional-mcp-config`/`--output-format json` on a real fraction of
Windows spawns, running the child in each CLI's interactive default
instead (burning real API credits) while `cmd /C`'s own exit code 0 still
made pact report `exit_success: true`. Not Copilot-specific: `codex.cmd`
and `gemini.cmd` are the byte-identical npm `cmd-shim` template as
`copilot.cmd` (confirmed by reading all three installed shims directly --
only the final script path differs), and `claude.exe` was wrapped in the
same `cmd /C` unconditionally too -- all 4 adapters were exposed.

**The report's own suggested fix ("modern Rust resolves PATHEXT natively,
just drop the wrapper") is empirically wrong for this project's actual
environment -- verified by hand before considering it, not taken on
faith.** Tested directly on rustc 1.96.1 (well past the claimed Rust
1.60 cutoff): a bare `Command::new("<name>")` for a real `.cmd`-shimmed
program fails outright with `program not found` -- matching this
project's own already-documented finding (`pact-deps` > Windows `.cmd`
shim resolution). Dropping the wrapper as suggested would have been a
straight regression, breaking every Windows spawn rather than fixing the
multi-line case. Also tested and ruled out: pointing `Command::new`
directly at a resolved `.cmd` file path (Rust's own "BatBadBut"
mitigation rejects it: `batch file arguments are invalid`), the
interactive-`cmd.exe` caret-newline line-continuation escape (`^` +
`\n`, doesn't survive `cmd /C`'s re-tokenization), and `%VAR%`-expansion
indirection (same truncation, plus loses quoting).

**What actually works, confirmed by hand:** a genuine native `.exe`
target spawned with no `cmd.exe` in the chain at all preserves an
embedded newline in an argument perfectly, every subsequent flag intact
-- the truncation is 100% specific to `cmd.exe`'s own line-oriented
reparsing, not a fundamental Windows/Rust argv-passing limit. All 3
current `.cmd`-shimmed agents are npm's own standard `cmd-shim` template
verbatim (`... & "%_prog%" "%dp0%\<relative-script>.js" %*`, `%_prog%`
being the shim's own sibling `node.exe` if present, else bare `node` from
PATH) -- the same technique Node's own `cross-spawn` library uses for
this exact class of problem on Windows.

New `pact-agents::windows_shim` (windows-only, `#[cfg(windows)]`)
resolves `program` to a directly-executable target before `cmd.exe` is
ever involved: a real `.exe` found on PATH needs no wrapper at all (fixes
`claude.exe` with zero added complexity); a `.cmd`/`.bat` shim matching
the standard npm template gets parsed for its real interpreter + script,
spawned directly (fixes Copilot/Codex/Gemini). Anything that resolves
neither way falls back to the old `cmd /C` wrapper exactly as before, so
nothing regresses for an unrecognized shape. `process::build_agent_command`
is the single call site, `#[cfg(windows)]`/`#[cfg(not(windows))]` split so
non-Windows platforms are unaffected (unchanged direct `Command::new`).

Verified for real at every layer, not just unit-tested against
synthetic fixtures: the shim parser's tests use the byte-for-byte real
template captured from the actual installed `copilot.cmd`/`codex.cmd`/
`gemini.cmd`; a `#[ignore]`d manual-verification test (`cargo test --
--ignored`) resolves all 4 real installed agent CLIs on this machine and
confirms the resolved interpreter/script actually exist; and a full-stack
proof spawned the real `node.exe` this project's `copilot` resolves to
(via PATH, since this machine's copilot install has no sibling
`node.exe`) against a harmless echo script with a real multi-line
argument plus trailing flags -- all arrived at the child completely
intact, with zero `cmd.exe` involved.

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

**Partial live verification (2026-07-30, issue #9)**: real auth now
available (Google discontinued the old "Sign in with Google" / Gemini
Code Assist OAuth path for individuals in favor of its new Antigravity
product line -- confirmed directly from the CLI's own failure message,
`This client is no longer supported for Gemini Code Assist for
individuals`; the still-maintained path for headless `gemini` is a plain
AI Studio API key via `GEMINI_API_KEY`), which made a first real
`pact spawn --agent gemini` possible. Confirmed for real: the adapter's
mechanics all work end to end against actual `gemini` output --
`--skip-trust`/`--approval-mode yolo` launch cleanly, a real `{"type":
"init", ...}` event arrives and parses as `AgentEvent::Init` exactly as
coded, and the coordination "never reported a status" warning fires
correctly (expected, since the task never called an MCP tool). One real
schema gap found: `gemini`'s actual output includes a
`{"type":"message","role":"user","content":...}` event (an echo of the
prompt back on the stream) that `parse_line` doesn't explicitly match --
harmless today since the catch-all correctly routes it to `Other`, not a
bug, but worth naming as confirmed real shape rather than still-guessed.

**Still unconfirmed**: every real spawn attempt (three, across two
sessions) hit the AI Studio free-tier key's request quota before
producing a real assistant/tool-call/result event -- `TerminalQuotaError:
You have exhausted your daily quota on this model` with `limit: 20`,
confirming this key's cap is a low fixed per-day count, not the ~1000/day
figure associated with the (now Antigravity-only) OAuth path. That means
the `assistant_message`/`tool_call`/`result` branches of `parse_line` are
still guesses, exactly as before -- only `init` (and the newly-seen,
already-correctly-ignored `message` echo) moved from guessed to
confirmed. Also noted in passing, not yet investigated: `-m
gemini-2.5-flash` was silently not honored -- the quota error reported
`model: gemini-3.5-flash` instead, a model never requested. Re-attempt
once the daily quota resets (or with a paid/billed key) to close the
remaining gap in issue #9.

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

### Safety profiles (issue #161)

From an outside code review (2026-07-24, triage discussion): `--safety`
was a raw pass-through to whatever vocabulary the specific agent CLI
uses -- Claude Code's `--permission-mode`, Codex's `--sandbox`, Gemini's
`--approval-mode`, and Copilot ignoring it entirely (no gradient). A user
wanting "the safest sane setting for unattended use" across agents
needed to know all four vocabularies. Resolved direction from that
discussion: add three pact-level profile names as **aliases** layered on
top of the existing raw pass-through, never a replacement -- any other
value (`acceptEdits`, `read-only`, ...) keeps flowing through to
`build_command` completely unchanged.

`pact_agents::resolve_safety_profile(agent, safety)` is the single
resolution point, called right before each of the three places that
already call `adapter.build_command` (`spawn_preview`,
`spawn_with_supervisor`, Arbiter's `attempt_arbiter_resolution`) --
deliberately *not* resolved once at the CLI layer, since `spawn-many`
shares one `--safety` string across a batch that can mix agent kinds;
resolving per-adapter right where the agent is already known handles a
mixed-agent batch correctly for free, with no CLI-layer change needed at
all beyond documenting the three names in `--help`.

Mapping, confirmed by hand via free `--dry-run` previews across all four
adapters (no real agent spend needed to verify a resolved command
string):

| profile | Claude Code | Codex | Gemini | Copilot |
|---|---|---|---|---|
| `strict` | `--permission-mode plan` | `--sandbox read-only` | `--approval-mode plan` | `--allow-all-tools` (unchanged) |
| `workspace-write` | *(no flag -- pact's existing curated-allowlist default)* | `--sandbox workspace-write` | `--approval-mode auto_edit` | `--allow-all-tools` (unchanged) |
| `unrestricted` | `--permission-mode bypassPermissions` | *(no flag -- pact's existing full-bypass default)* | *(no flag -- pact's existing yolo default)* | `--allow-all-tools` (unchanged) |

Two deliberate non-uniformities, not oversights:

- **Codex's `workspace-write` doesn't currently get real work done.**
  Confirmed by hand (see "Codex adapter" above): even a plain `--sandbox
  workspace-write`, without the bypass flag, still refuses to write
  files in headless mode -- the agent reports "approvals are disabled"
  and gives up. `workspace-write` here is faithfully a deliberately
  read-only/inspect-only run for Codex today, not a safer "still gets
  work done" middle ground -- that's Codex's own current CLI
  limitation, not something this aliasing layer can paper over.
- **Codex's and Gemini's `unrestricted` is the same as their existing
  default**, and **Copilot's three profiles all resolve to the same
  no-op.** Both are honest, not lazy: pact's existing defaults for
  Codex/Gemini are already the only mode confirmed to complete real
  headless work at all (no safer alias exists to offer instead, per each
  adapter's own DESIGN.md section), and Copilot's CLI genuinely offers
  no distinct restricted mode through this adapter to alias in the first
  place.

The "explicit safety override" warning (`spawn`/`spawn-many`) shows what
a profile name actually resolves to for the chosen agent (e.g. `strict
-> plan`), since that mapping isn't obvious from the profile name alone
-- a raw, non-profile value still just echoes back unchanged.

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

### `pact clear-leases` (issue #209)

Leases persist in SQLite across `mcp-serve` process restarts by design --
coordination state needs to outlive any single sidecar process (see
"Database placement" above). Combined with the 15-minute default TTL,
this meant a fresh dev/test run's very first claim could hit a false
conflict against a *prior* run's still-live lease from minutes earlier --
a real report: `fail_on_conflict=True` raised before the reporter's own
script had claimed anything at all.

**Deliberately not fixed by shortening the default TTL or building a
"stale" detector.** Both were considered and rejected in a real design
conversation before implementing anything (this project's standing
convention for genuine forks -- see #65, #84/#85, #159-162): a shorter
default trades off against real agent sessions that legitimately run
longer than the new default without re-claiming, silently weakening real
conflict detection to fix a dev-loop annoyance; and "stale" has no clean
definition here (by age? by workspace existence? by repo?) that can't
also misclassify a real, still-relevant lease from a genuinely slow
agent.

**What shipped instead: `pact clear-leases`**, an explicit,
unconditional wipe of every lease row (active or already expired) for
the repo's coordination database -- no heuristic, no scope decision to
get wrong, purely "you, the caller, are asserting nothing real is in
flight, so nothing is." The same posture leases themselves already take
(advisory, not enforced) applied to their own lifecycle management.
Messages and history are untouched -- this is scoped to the one table
issue #209 was actually about.

### Opt-in strict claim response (issue #162)

pact has no mechanism to physically stop an agent from writing a file --
it never sits between an agent and its own file-write tool calls, by
design (doing so would require CLI-specific integration that breaks
agent-agnosticism). The `granted`->`accepted` rename (issue #36, above)
exists precisely because pretending to lock something it can't enforce
was found actively misleading. A real queueing/blocking primitive
(`claim_files` actually waiting until a conflicting lease frees) was
considered and explicitly rejected in the same triage discussion this
issue came from -- a bigger build, real UX risk (a hung MCP tool call
needs a mandatory max-wait), not justified by current evidence of need.

Chosen direction instead: an opt-in `fail_on_conflict` boolean param on
`claim_files`. Default `false` (unchanged advisory behavior, the only
mode that existed before this). When `true` and the claim overlaps
another holder's active lease, `claim_files` now returns `Err` *before*
touching the `leases` table at all -- checked ahead of the `INSERT`
loop, not after -- so a rejected claim leaves no trace: a caller
retrying after resolving the overlap won't find its own earlier,
rejected attempt still on record. The MCP layer needed no new plumbing
for this: `leases::claim_files` returning `Err` already flows through
the exact same `error_result`/`isError: true` path a malformed glob or
invalid `ttl_seconds` already used, since it's the same `Result`
being handled. Same mechanism throughout, no new state, no
blocking/queueing.

Verified for real across the whole surface, not just the Rust layer:
3 new `pact-coord` unit tests (rejects an overlapping claim, doesn't
record it, still accepts a non-conflicting one), plus a matching real
end-to-end test in both `bindings/python` and `bindings/ts` -- two real
`pact mcp-serve` sessions, one claiming a real file first, the second
retrying with `fail_on_conflict` and asserting the SDK's own
`PactCoordError` is raised, not just that the Rust-level function
returns `Err`.

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

**2026-07-29 update: a different angle did present itself, from an
unrelated investigation, and it measurably helped.** Real-agent-
verifying the Arbiter Write-fresh redesign (issue #106) the same night
surfaced issue #184: `run_and_stream` never explicitly closed a spawned
agent's stdin, leaving it inherited/ambiguous, and one real Claude Code
invocation was directly observed stalling for several seconds ("no
stdin data received in 3s, proceeding without it") before apparently
misreading an empty task. An unexpected multi-second stall in an
agent's own startup path is exactly the kind of thing that could throw
off a race that's specifically timing-sensitive under concurrency --
plausible, not confirmed, as a *contributing* factor to this issue,
distinct from (and possibly compounding) the OS-scheduling-contention
theory above.

After #184 shipped, re-ran the exact same reproduction this issue's own
text describes (2-agent concurrent `spawn-many`, 6 back-to-back
batches, Haiku for cost control) against real `main`: **12/12 real
agent connections reached `connected`, zero failures** -- versus the
original 1/6 (and Haiku specifically making it *worse*, 6/10, before
#184). Not proof the underlying OS-scheduling race is gone -- a
12-for-12 run doesn't rule out a race that was already intermittent
at a ~1-in-6 rate, and the sample here is small, matched to this
session's "modest real-agent spend" brief rather than an exhaustive
re-validation. Documented honestly as a real, measured improvement
worth noting, not a confirmed fix -- left open rather than closed,
since the original OS-contention theory was never ruled out, only
found to have a real compounding factor that's now removed.

Left open, documented -- revisit with a larger real-agent sample if a
regression report comes in, or if Claude Code's own MCP client behavior
changes.

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

### Coordination reachability: "no leases" vs. "no server" (issue #235)

A real production run with GitHub Copilot CLI found `coord-status`
reporting "no active leases" / "no pending messages" for an entire
session where every task's prompt explicitly instructed the agent to
call `claim_files`. Root cause of the *ambiguity* (not necessarily of why
the agent didn't call the tool, which is a separate, larger question --
see the "strategic criticism" discussion on issue #227): pact writes an
MCP config and hands it to the agent CLI, which is responsible for
launching `pact mcp-serve` and connecting to it as an MCP client. If
that never happens -- wrong config, the agent CLI not invoking it, the
agent choosing not to call any coordination tool for the whole run --
`coord-status` reads an empty `leases`/`messages` state and prints
exactly what it prints when everything is working correctly and nobody
has claimed anything yet. Those two states were indistinguishable, and
only one of them means the coordination layer is functioning.

Fixed with a real connection signal, not a guess: `CoordServer::get_info`
(the `ServerHandler` method rmcp calls while answering the client's
`initialize` request -- the one point in the MCP protocol every real
client reaches before any tool call, whether or not it ever calls one)
now logs a `coord_connect` operation for its `agent_id`, reusing the
existing operation log (issue #84) rather than a new table.
`CoordStatus` gained `connected_agent_ids`, and both `pact coord-status`
and `pact status` cross-reference it against `pact list`'s actual active
workspaces: a workspace that's *finished* and never connected gets an
explicit warning/hint naming it, distinct from ordinary "nothing claimed
yet" silence. A still-*running* workspace is deliberately not flagged --
it may simply not have launched `mcp-serve` yet, and flagging every
in-progress run would be noise, not signal.

**Separately, `expand_glob`'s blindest spot.** `leases::expand_glob`
only ever walked the real filesystem, so a claim on a path that doesn't
exist yet -- the "I am about to create this file" case, the single most
important one for conflict avoidance between two agents -- expanded to
the empty set and could never show an overlap. Fixed narrowly: a
*literal* pattern (no glob metacharacters) that matches nothing on disk
now resolves to itself, normalized, so two agents both claiming the same
not-yet-created path see the conflict. A pattern *with* real
metacharacters (e.g. `src/generated/*.ts`) that matches nothing on disk
still can't be resolved this way -- there's no way to know what a
wildcard would match against files that don't exist -- left as a known,
narrower limitation, not attempted here.

**Explicitly not decided here:** whether the advisory-lease model's
future is worth continued investment at all, versus leaning primarily on
`pact conflicts`' mechanical, no-cooperation-required filesystem diffing
(which worked correctly on the same real run that found leases doing
nothing). That's a real strategic fork, not a bug -- see issue #227's
tracking comment for the two options as posed to the user; this pass
only fixes what makes the *current* mechanism honest and correctly
functioning, without deciding whether to keep betting on it.

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
ready before the agent's first real command runs. Every ecosystem this
detects (pnpm, yarn, uv, poetry, pipenv, Cargo, Go modules, Maven, Gradle,
and npm) already has a good global shared cache, so `prepare` just runs
each one's normal install/fetch command (`passthrough` for most; `npm ci`
directly for npm) and lets that cache do the sharing. Plain pip/venv is
intentionally left as passthrough-only (see "Passthrough caching strategy"
below).

### Content store removed in favor of npm's own cache (issue #233)

npm didn't always get this treatment. Through issue #233, it was the one
ecosystem pact built its own sharing for: a lockfile-hash-keyed content
store (`ContentStore` in `store.rs`), populated once per (platform,
lockfile) pair via a real `npm ci` under a `PidLock`, then materialized
into each workspace via reflink or read-only hardlink. Every section
below through "npm store manifest, verification, cleanup" documents that
subsystem's real history -- issues found, fixes shipped, all verified at
the time. It's kept for that history, not as current documentation:
**that entire subsystem is deleted.**

The removal decision itself (not a new problem found, a re-evaluation of
whether purpose-built sharing was worth its maintenance cost) landed once
`prepare_npm` already had a documented open question along exactly these
lines (see "Content store lock timeout, and --no-deps" below: "whether
the content-store subsystem should exist at all... left for a real
conversation rather than decided here"). That conversation happened: npm
already has a global cache (`~/.npm`, or wherever `npm config get cache`
points) shared automatically across every concurrent `npm ci` call on the
machine, with npm's own locking, no pact-side coordination needed --
which is exactly the property the custom store existed to provide, built
from scratch instead of reused. Verified by hand before deleting
anything (this project's standing discipline for every git/OS-level
assumption): 5 concurrent `npm ci` calls into 5 separate workspace
directories, sharing one global cache, run twice (a cold cache, then a
second pass with a warm one) -- both runs succeeded for all 5, no
corruption, no errors, comparable wall time to what the custom store's
own Phase 1 verification measured.

What this trades away, honestly: reflink/hardlink materialization meant a
second-plus workspace's `node_modules` came from an already-extracted
copy rather than npm re-extracting from its cache archive per workspace.
`npm ci` against a warm cache still avoids the network, just not the
per-workspace extraction step. What it gains: ~250 lines of custom
lock/manifest/materialization machinery deleted, one less subsystem
carrying its own bug class (the Windows `MAX_PATH` failure below is a
direct consequence of the store's own directory-nesting depth -- a
workspace-local `node_modules` doesn't have that problem to have), and
symmetry with how every other ecosystem in this crate is already
handled: pass through to the tool's own cache, don't rebuild it.

This is a real breaking change to the CLI surface: `pact store list`,
`pact store verify`, and `pact store clean` no longer exist. Shipped
with a minor version bump and called out in that release's notes, not
silently.

### Structured prep reporting (issue #12) -- historical, superseded above

Everything from here through "npm store manifest, verification, cleanup"
documents the deleted content-store subsystem's real history. Kept for
that reason, not as current behavior -- see "Content store removed"
above.

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

### Content store lock timeout, and --no-deps (issue #233)

A real production `spawn-many` run (5 tasks, cold npm content store) hit
~22 minutes of dead time before any agent touched a file. Root cause:
`POPULATE_LOCK_TIMEOUT` was 600s, held for the *entire* population --
one workspace won the lock and ran a real `npm ci`; the other N-1 waited,
but the install took longer than 600s, so every waiter timed out and
fell back to `run_plain_npm_install` -- a full, independent, redundant
install each. The store's own fast path (a waiter that acquires *after*
the winner finishes sees `entry.exists()` and reuses it for free) never
executed, because the timeout was shorter than the thing it was waiting
for. Net effect: the cache made this run strictly *worse* than having no
cache at all (dead waiting, plus N installs instead of N starting
immediately).

Same root cause and same fix shape as issue #230's `pact-vcs::LOCK_TIMEOUT`:
`PidLock`'s stale-lock stealing (liveness + start-time check) already
handles a populator that crashed, so a fixed short timeout was only ever
guarding against a populator that's alive but stuck -- a rare case that
deserves a generous window, not a budget shorter than a real cold-cache
install. `POPULATE_LOCK_TIMEOUT`: 600s -> 1 hour. Verified directly at
the lock level (not via a real slow `npm ci`, which would make the test
suite itself slow and flaky): `populate_if_absent_reuses_a_slow_populate_
instead_of_duplicating_it` races 4 threads against one key with an
artificially slow populate closure and asserts the closure runs exactly
once -- proving reuse, not duplication, without needing a real install.

**Separately, `--no-deps` (`spawn`/`spawn-many`).** The same report noted
every task paid dependency prep's full cost even for tasks that
explicitly said not to touch dependencies (pure version-string edits in
a manifest file) -- prep ran unconditionally, with no way to opt out.
`--no-deps` skips `pact_deps::prepare` entirely for that invocation; no
`-deps.json` sidecar is written either (its absence now means "never
attempted", distinct from an empty array meaning "ran, detected zero
package managers").

**Deliberately not done in this pass (the issue's own "deep research"
item, left for a real conversation rather than decided here):** whether
the content-store subsystem should exist at all, versus delegating
entirely to npm's own local cache (`npm ci` against a warm `~/.npm` is
already reasonably fast) plus a lighter hardlink layer, or adopting
`pnpm`'s/`cacache`'s content-addressable design wholesale. The two fixes
above remove the "worse than no cache" failure mode and the unconditional
cost, which was the concrete, reported harm -- but they don't settle
whether this ~250-line subsystem is worth its own maintenance cost
relative to deleting it and leaning on the package manager's own
tooling. Real tradeoffs on both sides (a from-scratch reimplementation
carries real bugs, like issue #7's MAX_PATH failure below and #57's BOM
handling, that a mature tool like pnpm has already hardened against; but
a from-scratch store is also the only way to get reflink/hardlink
materialization across arbitrary npm projects that never opted into
pnpm themselves) -- not a default to pick silently.

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

`platform_info` (renamed from `platform_key` when issue #160 needed its
`node_major`/`npm_version` components as their own structured fields,
not just baked into the key string -- returning both avoids a second
`node --version`/`npm --version` subprocess spawn just to get what this
function already asked them for) distinguishes store entries by OS,
architecture, libc
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

### npm store manifest, verification, cleanup (issue #160)

From an outside code review (2026-07-24, triage discussion): the shared
npm content store had no manifest, no way to verify an entry wasn't
corrupt, and no cleanup command -- it could only grow. Marked "design
first" at the time, since the cleanup policy specifically needed
thought (LRU vs TTL, safety against a concurrently-populating entry).
Picked sensible defaults this pass rather than leaving it unbuilt
further, matching the same reasoning as issue #159 (below): none of
this is a public API commitment, easy to retune later.

**Manifest** (`ContentStore::write_manifest`/`touch_manifest`/
`read_manifest`, a sibling `<key>.manifest.json` next to each `<key>/`
entry directory, matching the existing `<key>.lock` convention): `key`,
`created_at`, `last_used_at`, `node_major`, `npm_version`,
`lockfile_hash`, `file_count`, `byte_size` -- the exact fields the
review proposed. `file_count`/`byte_size` are computed by walking the
entry directory once, but only on a real populate (a cache miss) --
a cache hit only calls `touch_manifest` (updates `last_used_at`,
no walk), since the entry's content is never expected to change again
after population. An entry populated before this feature existed, or
whose manifest write failed, simply has no manifest -- `list`/`clean`
treat it as invisible rather than erroring; the entry itself still
works fine for materialization regardless.

**`pact store list`** -- every entry with a manifest: key, node/npm
version, file count, human-readable size, and how long ago it was last
used.

**`pact store verify [key]`** (all entries with a manifest if `key` is
omitted) -- confirms the entry's current file count and total byte size
still match what its manifest recorded. Deliberately not a
byte-for-byte content hash: re-hashing a potentially large
`node_modules` tree on every verify would defeat the point of caching
it. A mismatch here means something changed since population, which
should never legitimately happen to a shared store entry (nothing is
supposed to write into one afterward), so this is a reliable-enough
corruption signal without that cost. Exits non-zero if any entry fails.

**`pact store clean --older-than-days <N> | --all [--dry-run]`** --
removes entries by `last_used_at` age or unconditionally. Chose
`last_used_at` over `created_at` for the age check specifically because
an entry that's still being hit regularly shouldn't be evicted just for
being old -- LRU-style, not pure TTL, on the theory that "still useful"
is what actually matters for a cache. Safe against a
concurrently-populating entry: `remove_entry` acquires the exact same
per-key `PidLock` `populate_if_absent` already uses before deleting
anything, so a `clean` racing a real `spawn` for the same lockfile hash
either waits for that population to finish first or the population
waits for `clean` to finish removing a *stale* entry first -- never a
torn read of a half-written `node_modules`. Removing an entry never
affects a workspace that already materialized from it (copied or
hardlinked files are independent copies at that point) -- only future
materializations from that key.

Verified for real, not synthetically: a real `pact spawn` against a
real npm workspace (zero-dependency lockfile, so `npm ci` runs
instantly with no network access) populates a real entry, then `pact
store list`/`verify`/`clean --dry-run`/`clean --all` are driven against
what was actually populated, confirming the full list -> verify ->
dry-run -> real-removal -> empty round trip end to end
(`crates/pact-cli/tests/store.rs`).

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

### ReadOnlyHardlink tradeoff -- historical, superseded (issue #233)

Documents the deleted content store's materialization strategy -- see
"Content store removed in favor of npm's own cache" above. A hardlink
shares the same underlying file record as its content-store
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

### Doc/CLI grammar drift check (issue #238)

A real user following `SKILL.md` -- the file written specifically for an
agent to read and act on literally -- hit `pact diff --id <id>`
rejected: the real grammar is positional (`pact diff <id>`). Worse than
a typo: `merge-all`/`commit-all` genuinely *do* take `--id`, so the
error read as a bug in pact, not a bug in the doc. `SKILL.md` had the
same wrong shape for `resolve --id`/`teardown --id` too; `README.md` and
`GETTING_STARTED.md` already had the correct positional form in both
cases -- only `SKILL.md` had drifted.

Fixed the three wrong lines, then built the structural fix the issue
actually asked for: `crates/pact-cli/tests/docs_cli_grammar.rs` extracts
every fenced `pact`/`./pact` command from all three docs (joining `\`-
continued lines, handling double-quoted task text) and runs each against
a real scratch repo, asserting it isn't rejected as a clap usage error
(exit code 2 -- clap's own convention, distinct from `main`'s runtime
errors, which exit 1). `spawn`/`spawn-many` examples get `--dry-run`
force-appended if the doc example doesn't already have it, so a real
agent is never launched just to check argument parsing. This makes the
exact class of bug that caused this issue structurally hard to reintroduce
without noticing -- not proofread by a CI check that runs `--help` once,
but every real example, run for real, on every push.

**This check caught a second, previously-undiscovered real bug on its
first run**, not just the three known ones: `README.md`'s own
`--coord-command /path/to/alt-coord --coord-arg --some-flag` example
failed to parse -- `--coord-arg` had no `allow_hyphen_values`, so clap
rejected any flag-shaped value (`--some-flag`) as an unrecognized flag of
pact's own, even though `--coord-arg`'s whole purpose is forwarding
arbitrary arguments to an alternative coordination command that may
itself take flags. Fixed on both `spawn`/`spawn-many`'s `coord_args`
definitions, not just the doc.

`mcp-serve` gets its own, self-contained tokio runtime rather than making
the whole CLI async -- it's the only command that needs one (`rmcp`
requires async), and every other command stays exactly as synchronous as
it already is. See the README for why that tradeoff was made deliberately,
not by default.

### SKILL.md routing re-verified against a real Copilot session (issue #203)

The reworded `SKILL.md` description (leading with the user's problem
instead of the tool list, plus explicit "prefer pact over your own
built-in task tool" language) was flagged as *plausible but unconfirmed*
when it shipped -- the original fix explicitly said re-testing would
need another real, billed Copilot call, and didn't spend one
unilaterally.

**Live re-verified.** A real `copilot skill add`, in a plain scratch repo
with no prior pact context, against the exact near-verbatim trigger
prompt from the original report ("I have several independent coding
tasks I want to run concurrently on this repo, without them stepping on
each other's files. How should I do that?"): `session.skills_loaded`
confirmed the `pact` skill loaded, the assistant's very first message
explicitly invoked it ("Running the \"pact\" skill to show how to run
multiple independent coding tasks in parallel using isolated worktrees
and advisory leases"), and the follow-up response recommended concrete
`pact doctor`/`pact spawn-many --dry-run --task` commands -- a complete
reversal from the original bug (Copilot recommending `git worktree add`
plus its own built-in task tool instead). Confirmed fixed, not just
plausible.

### pact init auto-registers SKILL.md with detected agent CLIs (issue #219)

Follow-up to #203: registering `SKILL.md` used to require a manual step
per agent CLI (`copilot skill add <pact-repo>`). `pact init` now does
this automatically for every agent CLI it detects installed, reusing the
same detection `--agent` auto-default (#121) already built.

**Per-adapter mechanism, confirmed by hand, not assumed:**
- **Copilot**: shells out to `copilot skill add <repo-root>` (confirmed
  the real registration mechanism while re-verifying #203 above) --
  Copilot owns its own skills directory and registration bookkeeping, so
  pact defers to the CLI's own command rather than writing into
  `~/.copilot/skills/` directly and risking format drift from whatever
  internal manifest Copilot itself maintains alongside the raw files.
- **Claude Code, Codex, Gemini**: no confirmed, real skill-directory
  convention for any of the three as of this pass (unlike Copilot's
  `copilot skill add`, none has a discovered equivalent CLI subcommand or
  documented fixed skills path verified by hand) -- rather than guess at
  an undocumented path and silently write into the wrong place, these
  three are a no-op for now, each logged as a single `tracing::debug!`
  line naming the adapter and "no confirmed skill-registration mechanism"
  rather than failing or pretending success.

**Opt-in, not default-on**: `pact init --register-skill` -- the safer
default for a command that writes into a directory *outside* the repo it
was invoked in (a real CLI's own global config), matching this project's
general bias toward additive, no-surprise-behavior-change defaults (how
`--name`/`--no-deps` shipped, both opt-in flags rather than changed
defaults). Absence of the flag is a silent no-op, not a warning --
running `pact init` shouldn't get noisier for users who don't want this.

Failure handling: a `copilot skill add` that fails (Copilot CLI not
actually on `PATH` despite being detected via a different check, a
permissions error) is logged as a warning and does not fail `pact init`
itself -- the same "best-effort, never block the primary operation"
posture dependency prep and passthrough installs already use.

**Caught a real bug live-verifying this against the actual pact repo**:
the first implementation spawned `copilot` via a direct
`std::process::Command::new("copilot")`, which failed with "program not
found" despite `copilot` working fine typed interactively and being
correctly detected as installed moments earlier -- the same Windows
`.cmd`-shim gap `cmdutil::run` already exists to work around
(`std::process::Command` doesn't consult `PATHEXT` the way a real shell
does). Fixed by routing through `pact_deps::run_shimmed` instead of a
raw spawn, confirmed by re-running `pact init --register-skill` against
this repo's own real Copilot installation afterward (`registered
SKILL.md with copilot (copilot skill add)`, and `copilot skill list`
showed it).

`print_event_labeled` needs no extra locking beyond what `println!`'s own
internal `Stdout` lock already gives per call: each event becomes one
complete line written in one call, so concurrent threads' (`spawn-many`)
lines interleave at line granularity, never mid-line.

### `teardown` bulk mode (issue #214, outside Windows Copilot report)

`teardown` required a workspace `<id>` -- no way to tear down every active
workspace at once, asymmetric with `commit-all` (which already documented
"without --id, commits every active workspace that's dirty"). After a
run leaves several workspaces active, cleanup meant N separate `pact
teardown --force <id>` invocations, each needing the full id slug typed
out by hand.

`id` is now `Option<String>`, mirroring `commit-all`'s exact pattern:
omitted means every active workspace, torn down independently -- one
that fails (a dirty workspace without `--force`, most commonly) is
reported and the batch continues rather than aborting, same "report and
continue" shape `commit-all` already used. Cross-workspace conflict
detection (previously computed once per single `teardown` call, right
before removal since it needs the branch teardown deletes) is now
computed once for the whole batch up front instead of once per
workspace -- a minor, deliberate timing change: still informational-only
(never blocks a teardown), and avoids N redundant `detect_conflicts`
calls for a bulk invocation. `FileConflict` gained a plain `Clone` derive
to support filtering a shared snapshot per workspace in the loop.

Verified end-to-end via the fake-agent harness (issue #157): bulk
teardown removes every workspace and `list` reports none left; a bulk
teardown where every workspace is dirty (no `--force`) reports each one
failed and leaves all of them still active, rather than tearing down
none or aborting after the first failure.

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

**Real leak found and fixed (issue #195, outside R4 regression report,
2026-07-29): cleanup only ever removed the repo directory, never the
workspace state directory.** `run_inner` opens a real `WorkspaceManager`
against the temp repo, which creates its state directory (locks/meta/
workspaces) as a **sibling** of the repo root -- `.pact-<repo-name>`, not
a subdirectory of it (see "Workspace lifecycle" below). `demo::run`'s
`remove_dir_all(&repo_root)` never touched that sibling, so every single
real `pact demo` run since the feature shipped leaked one directory into
the temp dir -- confirmed by finding over 150 of them already
accumulated in this project's own dev environment. `demo_leaves_no_leftover_temp_directory`
had been checking `repo_path.exists()` the whole time, which *was*
being cleaned up correctly -- that's precisely why this shipped and
stayed broken silently: the test's own blind spot matched the bug's
blind spot. Fixed by extracting the sibling-state-dir derivation
`WorkspaceManager::open` already did inline into a new public
`WorkspaceManager::state_dir_for(repo_root) -> Result<PathBuf>` (pure
path math, no filesystem access) that `open` and `demo::run`'s cleanup
now both call, so the two can never drift apart again -- and extended
the existing test to check it too, not just the repo path.

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

**Regression coverage backfilled after the fact**: at the time #178 was
found and fixed, the harness itself didn't yet have a dedicated test for
the exact scenario that caught it -- a real `spawn` that goes through
real dependency prep (writing the `-deps.json` sidecar), followed by a
real `list`. `spawn_through_real_dependency_prep_then_list_does_not_crash`
closes that gap: a zero-dependency `package.json` with no lockfile
(the "plain-install-no-lockfile" prep strategy -- a real, instant,
no-network `npm install --no-package-lock`) is enough to reproduce the
exact conditions #178 needed, without needing a real network-dependent
install.

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

### Slow integration tier (issue #240)

A real production report's method-level critique, not a single bug:
`require_passing_tests` was covered only by `true`/`false` fixtures that
never needed a dependency (couldn't have caught #232's "gate can't even
run" bug by construction); `store.rs` had no test exercising real
concurrent population; nothing asserted N `spawn-many` tasks produce N
workspaces under *real* contention rather than a fast/synthetic
approximation of it. "If a test's fixture command is `true` or `false`,
it's testing the plumbing, not the feature" -- verified true of this
project's suite before this section existed.

`crates/pact-cli/tests/slow_integration.rs` is the first entry in a
deliberately separate, `#[ignore]`d tier -- not part of default `cargo
test --workspace`, run explicitly (`cargo test --ignored` or `--
--include-ignored`) or on a schedule, since a real `npm ci` and 5-way
real concurrency cost real wall time CI shouldn't pay on every push.
Its one test exercises the exact combination the original report hit:
5 concurrent `spawn-many` tasks against a repo with a real (zero-dependency,
no-network) npm lockfile, then a dependency-requiring `--require-passing-
tests` gate -- covering #230 (count fidelity under real `git worktree
add` contention), #233 (each task's real `npm ci` succeeding under real
concurrent contention), and #232 (the gate's environment-vs-code
diagnosis) together, not simulated separately.

**This tier caught a real bug on its first run, which is the whole
point of it existing.** `prepare_npm` computed `store_hit` via a
separate `ContentStore::entry_exists` check made *before* calling
`populate_if_absent`, with no lock held for that check. Under the fake-agent
harness's fast synthetic concurrency this never showed up (threads never
raced tightly enough), but real concurrent processes hitting a real,
slower `npm ci` did: all 5 concurrent tasks observed the unlocked
pre-check as "doesn't exist yet" before any of them finished populating,
so all 5 reported `store_hit: false` -- even though `populate_if_absent`'s
own locking was already correct and only one `npm ci` actually ran.
Fixed by moving the hit/miss determination *inside* `populate_if_absent`'s
lock, at the exact point `entry.exists()` is authoritative: it now
returns `(PathBuf, bool)`, the `bool` meaning "this call performed a
fresh populate." `prepare_npm` derives `store_hit` from that instead of
a separate racy check. The fast, synthetic `populate_if_absent_reuses_a_
slow_populate_instead_of_duplicating_it` unit test (issue #233) was
strengthened to assert exactly one of 4 concurrent callers reports
`populated_now: true`, so this specific race has fast, deterministic
regression coverage too -- the slow tier found it, a fast test now
guards it.

**Both `store_hit` and `populate_if_absent` are gone now** -- issue #233
later deleted the whole content store this race lived in, in favor of
npm's own global cache (see "Content store removed in favor of npm's own
cache" under pact-deps). The paragraph above is kept as history: real
concurrency finding a real bug that fast synthetic tests couldn't, which
is what justified this tier existing in the first place, independent of
whether the specific subsystem it found the bug in survived.
`five_concurrent_tasks_each_run_a_real_npm_ci_under_real_contention`
(renamed from a "content store" name that no longer fit) now asserts the
simpler post-#233 property directly: all 5 concurrent tasks' `npm ci`
report `success: true` under real contention against one shared global
cache, no pact-side lock involved at all.

### `spawn-many --dry-run --estimate-cost` (issue #222)

Outside feature notes observed that first-time users on API-metered
adapters (Claude Code/Codex/Gemini, unlike Copilot's flat-rate plans)
hesitate to run `spawn-many` because they can't predict cost --
`--dry-run` shows what pact *would* do but nothing about tokens or
dollars. `--estimate-cost` (requires `--dry-run`) prints an input-token
count and a dollar range per adapter right after the existing per-task
preview, before `return Ok(())`.

**Simplification found while implementing, vs. the source notes**: they
suggested adding a new `preview_prompt()` method to `AgentAdapter` to get
the exact prompt string a launch would use. Not needed -- `batch:
Vec<SpawnManyTask>` (built before the dry-run branch even runs) already
holds every task's raw prompt text in memory, which is the dominant
token cost. `estimate_input_tokens` just char-counts that directly
(`chars().count() / 4`, within ~15% of cl100k for English prose/code --
no tokenizer dependency, since the estimate is already presented as a
range, not one number).

**Rate card** (`adapter_rate`): a static per-`AgentKind` table, (cheapest
model, priciest model) input/output $/MTok, verified against each
provider's own published pricing page on `RATES_LAST_UPDATED_LABEL`
(2026-08-03), not guessed -- Anthropic's Haiku 4.5/Opus 5, OpenAI's
gpt-5-nano/flagship GPT-5-class model, Google's Gemini Flash-Lite/Pro.
Copilot is `flat_rate: true` (Pro/Business is quota-bounded, not
token-metered) and prints request-quota impact instead of a dollar
range. A `RATES_STALE_AFTER_SECS` (90 days) check against the pinned
timestamp prints a `WARN` rather than silently presenting old numbers as
current.

**Mixed-adapter batches** (`claude:`/`copilot:` task prefixes) get one
breakdown per adapter (grouped in `print_cost_estimate` via a
`BTreeMap<&str, (task_count, input_tokens)>`), not one misleading
combined number -- matches the source notes' explicit "don't build
per-task rate overrides, just split by adapter" guidance.

**What's deliberately not estimated**, stated explicitly in the printed
output every time: output tokens (10x-100x input is typical but wildly
task-dependent), file-read tokens (agent-dependent, e.g. Copilot reads
more liberally than Claude Code), and MCP tool-call overhead. An honest
floor, not a total -- matches the source notes' "false precision is
worse than an honest wide range" framing.

**Out of scope for this issue**: `--json` output for the estimate.
`spawn-many` has no `--json` flag at all today (unlike `history`/
`merge-all --dry-run`/`doctor`/`status`) -- adding one just for the cost
estimate, without also JSON-ifying the existing per-task preview it sits
alongside, would be a half-measure; left for whenever `spawn-many`
itself gets a `--json` mode.

### `pact status` (issue #221)

Outside feature notes observed that understanding what pact is doing
after a `spawn-many` requires composing four separate commands (`list`,
`history`, `coord-status`, `inspect <id>`) -- a first-time user has to
already know all four exist before they can get a full picture. `pact
status [--json]` is one screen aggregating what those commands already
compute: no new data collection, just presentation. `build_status_rows`
reuses `Command::List`'s exact per-workspace logic (`Orchestrator::
is_dirty`, `run_metadata`'s `files_touched` annotation from issue #212,
`agent_process_alive`) and folds in each workspace's own coordination
claims/pending count from the same `CoordStatus` snapshot `coord-status`
already returns. `status_hints` is a small, deliberately dumb
pattern-matched rule set (N running / M touched-zero-files / any open
conflict / all-idle-suggest-merge) -- same shape as the existing
error-message hint chain (issue #123), no fuzzy logic.

**One correction made to the source notes before implementing**: they
suggested a header line like "coord server: running (pid 12744, uptime
23m)". That doesn't match pact's actual architecture -- `mcp-serve` is a
per-agent-session subprocess the agent CLI itself spawns as its own MCP
client, not a standing daemon pact owns or can query the liveness of
(see issue #201, "`coord_status` field can go stale after `mcp-serve`
dies mid-session -- confirmed by design"). There is no single
repo-wide coord-server PID to report. Replaced with an aggregate lease/
message count from the same `Orchestrator::coord_status` snapshot
`coord-status` already computes, which is real.

**Deliberately out of scope for this issue**: `--watch`/`--refresh`
polling mode. Every value `status` aggregates is a cheap filesystem/
SQLite read, so a refresh loop is straightforward to add later, but it's
a distinct small design surface (when to exit, terminal clear/reprint
vs. a plain `--refresh N`-and-exit) left for a follow-up rather than
folded into the first cut.

### Test suite's own state-directory leak (issue #225)

Found by accident while checking scratch directories during an
unrelated session: `WorkspaceManager::open` unconditionally creates the
sibling `.pact-<repo-name>` state directory (`locks/`, `meta/`,
`workspaces/`) the moment it's called, regardless of whether a
workspace is ever actually created in it. Every integration test that
calls `open` directly, or runs the real `pact` binary (which calls it
internally), leaked one of these on every run unless its own
`cleanup()` helper explicitly removed it too -- confirmed by counting
**~5800** leaked directories accumulated in `%TEMP%` from this
project's own test runs over the prior three weeks.

Same root cause class as issue #195 (`pact demo` leaking its state
dir), but broader: #195's fix only patched `demo::run`'s own cleanup
path, never the test suite's 15 separate `cleanup()` helpers (this
codebase doesn't share a common test-utils module -- each integration
test file is self-contained, per existing convention). 14 of the 15
had the exact same blind spot (`remove_dir_all(root)` without also
removing `WorkspaceManager::state_dir_for(root)`, the same shared
derivation #195 introduced); `spawn_dry_run.rs` alone did it correctly
(via its own local reimplementation of the same path math), proof the
pattern was known but never applied everywhere. Mechanical fix, no
`pact` behavior change: every `cleanup()` now also removes
`WorkspaceManager::state_dir_for(root)`. Verified by running the full
suite once with `%TEMP%` cleared first and confirming zero `.pact-*`
directories remained afterward, not just that the code compiles.

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
