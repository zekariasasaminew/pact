"""Evaluate a reference-free preservation gate on benchmark results.

The gate needs only BASE, OURS, THEIRS and the proposed resolution, so it
could run inside pact where no human answer exists. A line one side added
relative to BASE is required when its hunk is a pure insertion or does not
overlap the other side's hunks; the gate flags a resolution missing any
required line. `--naive` requires every added line instead. Scored
against the benchmark's labels (partial = lost a change the maintainers
kept) and run on the maintainers' own merges to measure false alarms.
"""

import argparse
import difflib
import json
import subprocess
from pathlib import Path

from summarize import outcome


def show(repo: Path, rev: str, path: str) -> str:
    out = subprocess.run(["git", "-C", str(repo), "show", f"{rev}:{path}"], capture_output=True, text=True, encoding="utf-8", errors="replace")
    return out.stdout if out.returncode == 0 else ""


def lines(text: str) -> set[str]:
    return {line.strip() for line in text.splitlines() if line.strip()}


def hunks(base: list[str], side: list[str]) -> list[tuple[int, int, list[str]]]:
    matcher = difflib.SequenceMatcher(None, base, side, autojunk=False)
    return [(i1, i2, side[j1:j2]) for tag, i1, i2, j1, j2 in matcher.get_opcodes() if tag != "equal"]


def touches(a: tuple[int, int, list[str]], b: tuple[int, int, list[str]]) -> bool:
    return a[0] <= b[1] and b[0] <= a[1]


def flags(base: str, ours: str, theirs: str, result: str, hunk_aware: bool = True) -> bool:
    if not hunk_aware:
        b = lines(base)
        return bool(((lines(ours) - b) | (lines(theirs) - b)) - lines(result))
    base_lines, ours_lines, theirs_lines = base.splitlines(), ours.splitlines(), theirs.splitlines()
    ours_hunks, theirs_hunks = hunks(base_lines, ours_lines), hunks(base_lines, theirs_lines)
    required: set[str] = set()
    for mine, other in ((ours_hunks, theirs_hunks), (theirs_hunks, ours_hunks)):
        for hunk in mine:
            pure_insertion = hunk[0] == hunk[1]
            if pure_insertion or not any(touches(hunk, o) for o in other):
                required |= {line.strip() for line in hunk[2] if line.strip()}
    return bool(required - lines(result))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("--cases", type=Path, required=True)
    parser.add_argument("--repos", type=Path, required=True)
    parser.add_argument("--naive", action="store_true", help="require every added line, ignoring overlapping hunks")
    args = parser.parse_args()

    cases = {c["id"]: c for c in json.loads(args.cases.read_text(encoding="utf-8"))}
    resolutions = args.results.parent / "resolutions"
    tp = fp = fn = tn = human_flagged = human_total = 0
    for line in args.results.read_text(encoding="utf-8").splitlines():
        record = json.loads(line)
        label = outcome(record)
        if label in ("declined", "caught"):
            continue
        case = cases[record["id"]]
        repo = args.repos / case["repo"]
        res_file = resolutions / f"{case['id']}.json"
        if not res_file.exists():
            continue
        proposed = json.loads(res_file.read_text(encoding="utf-8"))
        flagged = human_hit = False
        for path in case["files"]:
            base, ours, theirs = show(repo, case["base"], path), show(repo, case["ours"], path), show(repo, case["theirs"], path)
            flagged |= flags(base, ours, theirs, proposed[path], not args.naive)
            human_hit |= flags(base, ours, theirs, show(repo, case["merge"], path), not args.naive)
        bad = label == "partial"
        tp += flagged and bad
        fp += flagged and not bad
        fn += (not flagged) and bad
        tn += (not flagged) and not bad
        human_total += 1
        human_flagged += human_hit
    print(json.dumps({
        "arbiter_partial_caught": f"{tp}/{tp + fn}",
        "arbiter_good_flagged": f"{fp}/{fp + tn}",
        "human_merges_flagged": f"{human_flagged}/{human_total}",
    }, indent=2))


if __name__ == "__main__":
    main()
