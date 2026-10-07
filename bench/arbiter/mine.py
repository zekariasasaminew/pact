"""Mine real merge conflicts from public repos' history.

For every two-parent merge commit since --since, replay the merge with
`git merge-tree` and keep the ones git could not auto-merge. The merge
commit's own content for each conflicted file is the human resolution.
"""

import argparse
import json
import subprocess
from pathlib import Path

NEVER_AUTO_RESOLVE = ("package-lock.json", "yarn.lock", "pnpm-lock.yaml", "Cargo.lock", "poetry.lock", "uv.lock")
MAX_FILES = 3
MAX_BYTES = 60_000


def git(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True, encoding="utf-8", errors="replace", check=check)


def blob_size(repo: Path, rev: str, path: str) -> int | None:
    out = git(repo, "cat-file", "-s", f"{rev}:{path}", check=False)
    return int(out.stdout.strip()) if out.returncode == 0 else None


def is_text(repo: Path, rev: str, path: str) -> bool:
    out = subprocess.run(["git", "-C", str(repo), "cat-file", "-p", f"{rev}:{path}"], capture_output=True, check=False)
    return out.returncode == 0 and b"\0" not in out.stdout[:8000]


def content_conflicts(repo: Path, ours: str, theirs: str) -> list[str] | None:
    out = git(repo, "merge-tree", "--write-tree", "--no-messages", ours, theirs, check=False)
    if out.returncode == 0:
        return []
    if out.returncode != 1:
        return None
    stages: dict[str, set[int]] = {}
    for line in out.stdout.splitlines()[1:]:
        if not line.strip():
            break
        meta, _, path = line.partition("\t")
        stages.setdefault(path, set()).add(int(meta.split()[2]))
    if any(not {2, 3} <= s for s in stages.values()):
        return None
    return sorted(stages)


def mine(repo: Path, since: str, limit: int) -> list[dict]:
    merges = git(repo, "rev-list", "--merges", "--min-parents=2", "--max-parents=2", f"--since={since}", "HEAD").stdout.split()
    cases = []
    for merge in merges:
        parents = git(repo, "rev-list", "--parents", "-n1", merge).stdout.split()[1:]
        ours, theirs = parents
        files = content_conflicts(repo, ours, theirs)
        if not files or len(files) > MAX_FILES:
            continue
        if any(Path(f).name in NEVER_AUTO_RESOLVE for f in files):
            continue
        sizes = [blob_size(repo, merge, f) for f in files]
        if any(s is None or s > MAX_BYTES for s in sizes):
            continue
        if not all(is_text(repo, merge, f) for f in files):
            continue
        base = git(repo, "merge-base", ours, theirs).stdout.strip()
        subjects = git(repo, "log", "--format=%s", "-n10", f"{base}..{theirs}").stdout.strip()
        cases.append({
            "id": f"{repo.name}-{merge[:10]}",
            "repo": repo.name,
            "merge": merge,
            "ours": ours,
            "theirs": theirs,
            "base": base,
            "files": files,
            "task_text": subjects,
            "subject": git(repo, "log", "--format=%s", "-n1", merge).stdout.strip(),
        })
        if len(cases) >= limit:
            break
    return cases


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("repos", nargs="+", type=Path)
    parser.add_argument("--since", default="2021-01-01")
    parser.add_argument("--limit", type=int, default=200)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    cases = []
    for repo in args.repos:
        found = mine(repo, args.since, args.limit)
        print(f"{repo.name}: {len(found)} conflicting merges")
        cases.extend(found)
    args.out.write_text(json.dumps(cases, indent=2), encoding="utf-8")
    print(f"total {len(cases)} -> {args.out}")


if __name__ == "__main__":
    main()
