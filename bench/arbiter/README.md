# Arbiter on real merge conflicts

Does pact's Arbiter resolve conflicts that real maintainers actually hit, and
would its test gate catch it when it gets one wrong? This harness answers that
on public history instead of synthetic conflicts.

## What it does

1. `mine.py` walks every two-parent merge commit in a set of repositories,
   replays it with `git merge-tree`, and keeps the merges git could not finish
   on its own: content conflicts only, at most 3 files, each under 60 KB, no
   lockfiles. The merge commit's own content is the human resolution.
2. `run.py` replays each case in a throwaway worktree (`git merge --no-commit`
   of the second parent onto the first) and scores four resolutions with the
   same checks:
   - `human`: what the maintainers committed
   - `ours` / `theirs`: keep one side, the naive baselines
   - `arbiter`: pact's Arbiter, reproduced step for step from
     `attempt_arbiter_resolution_inner` in `crates/pact-core/src/lib.rs`: the
     same `build_arbiter_prompt` text, OURS placeholder plus `git add`
     neutralization, the same marker, emptied-file and out-of-scope checks, then
     the test gate. The incoming branch's commit subjects stand in for pact's
     task text.
3. `summarize.py` turns `results.jsonl` into the headline numbers.

## How a resolution is judged

- **Test gate**: the project's full suite (stress tests deselected, 60 s per
  test). A resolution passes when it adds no failure the human merge does not
  already have, the same baseline rule `pact run` uses for verify commands. A
  case only has a gate when the human merge's own suite runs in this
  environment.
- **Change preservation**: lines that OURS or THEIRS added relative to BASE and
  the maintainers kept must be present; lines either side deleted that the
  maintainers also dropped must stay deleted; no line may appear that exists in
  none of BASE, OURS, THEIRS or the human merge. A resolution that meets all
  three is `faithful`.
- Outcomes, in order: `declined` (Arbiter's own validation rejected it),
  `caught` (the gate rejected it), `match` (identical to the human merge after
  whitespace normalization), `faithful`, `partial`.

## Running it

```sh
python mine.py <repo>... --since 2021-01-01 --out cases.json
python run.py --cases cases.json --repos <dir of clones> --venvs <dir of venvs> \
  --work <scratch dir> --out <results dir> --model sonnet --jobs 6
python summarize.py <results dir>/results.jsonl --json-out summary.json
```

`--venvs` holds one virtualenv per repository name with that project's test
dependencies; the worktree's `src/` and root go on `PYTHONPATH`, so the
package under test is always the replayed tree. `--skip-agent` scores only the
baselines, which is free. Arbiter runs through `claude -p` or `codex exec`, so a
run spends real model quota (a Sonnet pilot measured about $0.68 of list-price
usage per conflict). A usage-limit error stops the run without recording
anything, and rerunning the same command resumes where it stopped.

`gate.py` scores a reference-free preservation gate on the results: a line one
side added is required when its hunk is a pure insertion or does not overlap
the other side's hunks.

## Results (October 2026)

`results/` holds the run reported on issue #379: 105 conflicts mined from
click, flask, jinja, werkzeug, marshmallow and pluggy (`cases.json`), of which
the first 44 (all 31 from click, 13 from flask, merged 2021 to 2026) were
resolved before the account's quota ran out. Arbiter ran on Codex CLI with
gpt-5.6-sol at medium reasoning, one attempt per conflict.

| | Arbiter | take ours | take theirs |
|---|---|---|---|
| identical to the human merge | 21 | 9 | 5 |
| every kept change, different text | 9 | 1 | 4 |
| silently dropped a change | 14 | 32 | 33 |
| changes kept (by line) | 89% | 44% | 60% |
| passed the test gate (35 with a suite) | 35/35 | 33/35 | 33/35 |

The test gate passed every lossy Arbiter resolution. The preservation gate
flagged 10 of the 14 lossy ones and 1 of the 30 good ones; requiring every
added line flags 13 of 14 but also 19 of 30 good ones. Mining also found that
Arbiter's substring marker check rejected `.rst` files with headings (5 of
the 105 cases), fixed in #381.
