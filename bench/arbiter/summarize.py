"""Summarize results.jsonl into headline numbers and a per-case outcome list."""

import argparse
import json
from collections import Counter
from pathlib import Path


def outcome(record: dict, side: str = "arbiter") -> str:
    res = record.get(side) or {}
    if side == "arbiter" and res.get("status") != "written":
        return "declined"
    if record.get("test_gate_usable") and not res.get("tests"):
        return "caught"
    if res.get("exact"):
        return "match"
    if res.get("faithful"):
        return "faithful"
    return "partial"


def preservation_rate(rows: list[dict], side: str) -> float:
    hit = sum((r.get(side) or {}).get("kept_hit", 0) for r in rows)
    total = sum((r.get(side) or {}).get("kept_total", 0) for r in rows)
    return round(hit / total, 3) if total else 0.0


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("results", type=Path)
    parser.add_argument("--json-out", type=Path)
    args = parser.parse_args()

    records = [json.loads(line) for line in args.results.read_text(encoding="utf-8").splitlines() if line.strip()]
    scored = [r for r in records if "arbiter" in r]
    gated = [r for r in scored if r.get("test_gate_usable")]
    outcomes = Counter(outcome(r) for r in scored)
    costs = [r["arbiter"].get("cost_usd") or 0 for r in scored]
    seconds = sorted(r["arbiter"].get("seconds") or 0 for r in scored)

    def rate(rows: list[dict], side: str, key: str) -> str:
        hits = sum(1 for r in rows if (r.get(side) or {}).get(key))
        return f"{hits}/{len(rows)}"

    summary = {
        "cases": len(scored),
        "repos": dict(Counter(r["repo"] for r in scored)),
        "skipped": [r["id"] for r in records if "arbiter" not in r],
        "outcomes": dict(outcomes),
        "with_test_gate": len(gated),
        "gate_pass": {side: rate(gated, side, "tests") for side in ("ours", "theirs", "arbiter")},
        "exact_match_human": {side: rate(scored, side, "exact") for side in ("ours", "theirs", "arbiter")},
        "outcomes_by_side": {side: dict(Counter(outcome(r, side) for r in scored)) for side in ("ours", "theirs", "arbiter")},
        "kept_change_rate": {side: preservation_rate(scored, side) for side in ("ours", "theirs", "arbiter")},
        "code_cases": sum(1 for r in scored if any(f.endswith(".py") for f in r["files"])),
        "code_outcomes": dict(Counter(outcome(r) for r in scored if any(f.endswith(".py") for f in r["files"]))),
        "mean_similarity": {
            side: round(sum((r.get(side) or {}).get("similarity", 0) for r in scored) / max(1, len(scored)), 3)
            for side in ("ours", "theirs", "arbiter")
        },
        "arbiter_cost_usd_total": round(sum(costs), 2),
        "arbiter_cost_usd_mean": round(sum(costs) / max(1, len(costs)), 3),
        "arbiter_seconds_median": seconds[len(seconds) // 2] if seconds else None,
        "per_case": [
            {"id": r["id"], "repo": r["repo"], "files": r["files"], "outcome": outcome(r),
             "similarity": (r["arbiter"] or {}).get("similarity"), "gate": bool(r.get("test_gate_usable"))}
            for r in sorted(scored, key=lambda r: r["id"])
        ],
    }
    printable = {k: v for k, v in summary.items() if k != "per_case"}
    print(json.dumps(printable, indent=2))
    if args.json_out:
        args.json_out.write_text(json.dumps(summary, indent=2), encoding="utf-8")


if __name__ == "__main__":
    main()
