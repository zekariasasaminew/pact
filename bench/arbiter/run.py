"""Run pact's Arbiter against mined real-world merge conflicts.

Each case is replayed in its own worktree: check out the merge's first
parent, `git merge --no-commit` the second, and score four resolutions of
the conflicted files with the same checks and the project's own tests:

- human:   the content the maintainers actually committed in the merge
- ours:    keep the first parent's side
- theirs:  keep the second parent's side
- arbiter: pact's Arbiter, reproduced step for step from
           crates/pact-core/src/lib.rs (attempt_arbiter_resolution_inner):
           same prompt, OURS placeholder + `git add` neutralization, same
           marker/emptied/out-of-scope validation, same test-command gate

Tests only count for a case when the human resolution passes them in this
environment; otherwise that case is scored on text agreement alone.
"""

import argparse
import difflib
import json
import os
import re
import shutil
import subprocess
import threading
import xml.etree.ElementTree as ET
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

CONFLICT_MARKER = re.compile(r"^(<{7}|>{7})( |$)", re.MULTILINE)
ALLOWED_TOOLS = "Read Write Edit Glob Grep Bash(git *) Bash(npm *) Bash(pnpm *) Bash(yarn *) Bash(cargo *) Bash(go *) Bash(pip *) Bash(uv *) Bash(mvn *) Bash(gradle *)"
TEST_TIMEOUT_S = 600
AGENT_TIMEOUT_S = 900

write_lock = threading.Lock()
repo_locks: dict[str, threading.Lock] = {}


def repo_lock(name: str) -> threading.Lock:
    with write_lock:
        return repo_locks.setdefault(name, threading.Lock())


def git(cwd: Path, *args: str, check: bool = True, binary: bool = False) -> subprocess.CompletedProcess:
    kwargs = {} if binary else {"text": True, "encoding": "utf-8", "errors": "replace"}
    return subprocess.run(["git", "-C", str(cwd), *args], capture_output=True, check=check, **kwargs)


def build_arbiter_prompt(task_text: str, stages: list[dict]) -> str:
    sections = ""
    for stage in stages:
        base = stage["base"] if stage["base"] is not None else "(no common ancestor -- this file was added independently on at least one side)"
        sections += (
            f"\n--- {stage['path']} ---\nBASE (common ancestor):\n{base}\n\n"
            f"OURS (already in the target branch):\n{stage['ours']}\n\n"
            f"THEIRS (incoming change):\n{stage['theirs']}\n"
        )
    file_names = ", ".join(s["path"] for s in stages)
    return (
        "You are resolving a real git merge conflict left behind by pact's `merge-all`. "
        "Use the Write tool only for this -- never Edit -- for every listed file: compose the "
        "file's ENTIRE final content yourself from the BASE/OURS/THEIRS text given below (not by "
        "reading and patching the file's current on-disk content), then call Write once per file "
        "with that complete content. Do not use Edit on these files under any circumstances, "
        "even to make a small change -- Edit will be denied. "
        f"The change being merged in came from this task:\n\n{task_text}\n\n"
        "It conflicts with work already merged from other agents. Below is each conflicted "
        "file's three-way content -- BASE (the common ancestor before either side changed it), "
        "OURS (already merged into the target branch), and THEIRS (the incoming change). Your "
        "Write's content should reflect the intent of BOTH sides -- do not just pick one side and "
        "discard the other unless they are truly incompatible. The file on disk right now still "
        "has git's raw conflict markers in it (<<<<<<<, =======, >>>>>>>) -- ignore those, they "
        "are not part of either side's actual content; do not treat this as an incremental edit to "
        "that on-disk text. Do not edit, create, or delete any file outside this list: "
        f"{file_names}. Do not run any `git` command yourself -- pact stages and verifies your result afterward.\n{sections}"
    )


def show(wt: Path, spec: str) -> str | None:
    out = git(wt, "show", spec, check=False, binary=True)
    return out.stdout.decode("utf-8", errors="replace") if out.returncode == 0 else None


def changed_paths(wt: Path) -> set[str]:
    out = git(wt, "status", "--porcelain=v1", "--untracked-files=all").stdout
    return {line[3:].strip().strip('"') for line in out.splitlines() if line.strip()}


def normalize(text: str) -> str:
    lines = [line.rstrip() for line in text.replace("\r\n", "\n").split("\n")]
    while lines and not lines[-1]:
        lines.pop()
    return "\n".join(lines)


def similarity(a: str, b: str) -> float:
    return difflib.SequenceMatcher(None, normalize(a).split("\n"), normalize(b).split("\n"), autojunk=False).ratio()


def syntax_ok(files: dict[str, str]) -> bool:
    for path, content in files.items():
        if path.endswith(".py"):
            try:
                compile(content, path, "exec")
            except SyntaxError:
                return False
    return True


def write_files(wt: Path, files: dict[str, str]) -> None:
    for path, content in files.items():
        (wt / path).write_bytes(content.encode("utf-8"))


def run_tests(wt: Path, venv: Path) -> dict | None:
    """Full suite, no -x: returns the failing test ids so a resolution is
    judged against the human merge's own failures, never against zero."""
    if not (wt / "tests").is_dir():
        return None
    env = dict(os.environ)
    env["PYTHONPATH"] = os.pathsep.join([str(wt / "src"), str(wt)])
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    python = venv / "Scripts" / "python.exe"
    report = wt / ".bench-junit.xml"
    started = time.monotonic()
    try:
        subprocess.run(
            [str(python), "-m", "pytest", "-q", "-p", "no:cacheprovider", "-o", "addopts=", "-o", "filterwarnings=",
             "-W", "ignore", "--timeout=60", "-k", "not stress", f"--junitxml={report}", "tests"],
            cwd=wt, env=env, capture_output=True, stdin=subprocess.DEVNULL, timeout=TEST_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        return {"total": 0, "failed": [], "timed_out": True, "seconds": TEST_TIMEOUT_S}
    seconds = round(time.monotonic() - started, 1)
    if not report.exists():
        return {"total": 0, "failed": [], "seconds": seconds}
    root = ET.parse(report).getroot()
    report.unlink()
    total, failed = 0, []
    for case in root.iter("testcase"):
        total += 1
        if case.find("failure") is not None or case.find("error") is not None:
            failed.append(f"{case.get('classname')}::{case.get('name')}")
    return {"total": total, "failed": sorted(failed), "seconds": seconds}


def gate_passes(result: dict | None, human: dict) -> bool:
    if not result or result.get("timed_out"):
        return False
    if result["total"] < human["total"]:
        return False
    return set(result["failed"]) <= set(human["failed"])


def gate_usable(human: dict | None) -> bool:
    return bool(human) and not human.get("timed_out") and human["total"] > 0 and len(human["failed"]) <= max(5, human["total"] // 20)


class UsageLimitHit(RuntimeError):
    """The agent's account is out of quota: not a resolution outcome, retry later."""


LIMIT_MARKERS = ("hit your session limit", "usage limit", "rate limit", "rate_limit", "429", "quota")


def check_limit(text: str) -> None:
    lowered = text.lower()
    if any(marker.lower() in lowered for marker in LIMIT_MARKERS):
        raise UsageLimitHit(text[:300])


def run_claude(prompt: str, wt: Path, model: str) -> tuple[bool, str, dict]:
    out = subprocess.run(
        [shutil.which("claude") or "claude", "-p", "--output-format", "json", "--allowedTools", ALLOWED_TOOLS, "--strict-mcp-config", "--model", model],
        input=prompt, cwd=wt, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=AGENT_TIMEOUT_S,
    )
    try:
        meta = json.loads(out.stdout)
    except json.JSONDecodeError:
        meta = {}
    detail = str(meta.get("result", out.stdout or out.stderr))
    if out.returncode != 0 or meta.get("is_error"):
        check_limit(detail)
        return False, detail, {"usd": meta.get("total_cost_usd")}
    return True, detail, {"usd": meta.get("total_cost_usd")}


def run_codex(prompt: str, wt: Path, model: str) -> tuple[bool, str, dict]:
    out = subprocess.run(
        [shutil.which("codex") or "codex", "exec", "-", "-m", model, "-c", 'model_reasoning_effort="medium"', "--json", "--dangerously-bypass-approvals-and-sandbox"],
        input=prompt, cwd=wt, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=AGENT_TIMEOUT_S,
    )
    usage, failure = {}, None
    for line in out.stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "turn.completed":
            for key, value in (event.get("usage") or {}).items():
                usage[key] = usage.get(key, 0) + value
        elif event.get("type") in ("turn.failed", "error"):
            failure = json.dumps(event)
    if failure or out.returncode != 0:
        detail = failure or out.stderr[-300:]
        check_limit(detail)
        return False, detail, {"usage": usage}
    return True, "", {"usage": usage}


AGENTS = {"claude": run_claude, "codex": run_codex}


def has_conflict_markers(content: str) -> bool:
    return bool(CONFLICT_MARKER.search(content))


def run_arbiter(wt: Path, case: dict, stages: list[dict], agent: str, model: str) -> dict:
    baseline = changed_paths(wt)
    for stage in stages:
        (wt / stage["path"]).write_bytes(stage["ours"].encode("utf-8"))
        git(wt, "add", "--", stage["path"])
    prompt = build_arbiter_prompt(case["task_text"], stages)
    started = time.monotonic()
    try:
        ok, detail, cost = AGENTS[agent](prompt, wt, model)
    except subprocess.TimeoutExpired:
        return {"status": "rejected", "reason": "agent timed out", "seconds": AGENT_TIMEOUT_S}
    result = {"seconds": time.monotonic() - started, "cost_usd": cost.get("usd"), "usage": cost.get("usage")}
    if not ok:
        return {**result, "status": "rejected", "reason": f"agent reported failure: {detail[:300]}"}

    contents = {}
    for stage in stages:
        path = stage["path"]
        try:
            content = (wt / path).read_bytes().decode("utf-8", errors="replace")
        except FileNotFoundError:
            return {**result, "status": "rejected", "reason": f"could not re-read {path}"}
        if has_conflict_markers(content):
            return {**result, "status": "rejected", "reason": f"left conflict markers in {path}", "contents": {path: content}}
        if not content.strip():
            return {**result, "status": "rejected", "reason": f"emptied {path} entirely"}
        contents[path] = content
    out_of_scope = sorted(p for p in changed_paths(wt) if p not in case["files"] and p not in baseline)
    if out_of_scope:
        return {**result, "status": "rejected", "reason": f"changed files outside the conflicted-file list {out_of_scope}", "contents": contents}
    if all(contents[s["path"]] == s["ours"] for s in stages):
        result["unchanged_from_placeholder"] = True
    return {**result, "status": "written", "contents": contents}


def line_set(text: str | None) -> set[str]:
    return {line.strip() for line in (text or "").splitlines() if line.strip()}


def preservation(result: str, human: str, stage: dict) -> dict:
    """How faithfully `result` carries the changes the maintainers kept.

    kept:        lines either side added (vs BASE) that the human merge kept
    resurrected: lines either side deleted (vs BASE) that the human merge also dropped
    invented:    lines found nowhere in BASE, OURS, THEIRS or the human merge
    """
    base, ours, theirs = line_set(stage["base"]), line_set(stage["ours"]), line_set(stage["theirs"])
    human_lines, result_lines = line_set(human), line_set(result)
    kept = ((ours - base) | (theirs - base)) & human_lines
    deleted = ((base - ours) | (base - theirs)) - human_lines
    known = base | ours | theirs | human_lines
    return {
        "kept_total": len(kept),
        "kept_hit": len(kept & result_lines),
        "deleted_total": len(deleted),
        "resurrected": len(deleted & result_lines),
        "invented": len(result_lines - known),
    }


def score(contents: dict[str, str], human: dict[str, str], stages: list[dict]) -> dict:
    exact = all(normalize(contents[p]) == normalize(human[p]) for p in human)
    sim = sum(similarity(contents[p], human[p]) for p in human) / len(human)
    totals = {"kept_total": 0, "kept_hit": 0, "deleted_total": 0, "resurrected": 0, "invented": 0}
    for stage in stages:
        for key, value in preservation(contents[stage["path"]], human[stage["path"]], stage).items():
            totals[key] += value
    faithful = totals["kept_hit"] == totals["kept_total"] and totals["resurrected"] == 0 and totals["invented"] == 0
    return {"exact": exact, "similarity": round(sim, 4), "syntax_ok": syntax_ok(contents), "faithful": faithful, **totals}


def run_case(case: dict, repos: Path, venvs: Path, work: Path, out_dir: Path, agent: str, model: str, skip_agent: bool) -> dict:
    repo = repos / case["repo"]
    wt = work / case["id"]
    with repo_lock(case["repo"]):
        git(repo, "worktree", "remove", "--force", str(wt), check=False)
        shutil.rmtree(wt, ignore_errors=True)
        git(repo, "worktree", "prune")
        git(repo, "worktree", "add", "--detach", str(wt), case["ours"])
    try:
        merge = git(wt, "-c", "user.email=bench@pact", "-c", "user.name=bench", "merge", "--no-commit", "--no-ff", case["theirs"], check=False)
        unmerged = set(git(wt, "diff", "--name-only", "--diff-filter=U").stdout.split())
        if unmerged != set(case["files"]):
            return {"id": case["id"], "skipped": f"replayed conflict set {sorted(unmerged)} differs from mined {case['files']} ({merge.stdout[-200:]})"}

        stages = [{"path": f, "base": show(wt, f":1:{f}"), "ours": show(wt, f":2:{f}"), "theirs": show(wt, f":3:{f}")} for f in case["files"]]
        human = {f: show(wt, f"{case['merge']}:{f}") for f in case["files"]}
        venv = venvs / case["repo"]
        record = {"id": case["id"], "repo": case["repo"], "files": case["files"], "subject": case["subject"]}

        write_files(wt, human)
        human_run = run_tests(wt, venv)
        record["test_gate_usable"] = gate_usable(human_run)
        record["human"] = {"run": human_run, "syntax_ok": syntax_ok(human)}

        for side in ("ours", "theirs"):
            contents = {s["path"]: s[side] for s in stages}
            write_files(wt, contents)
            tests = gate_passes(run_tests(wt, venv), human_run) if record["test_gate_usable"] else None
            record[side] = {**score(contents, human, stages), "tests": tests}

        if not skip_agent:
            arb = run_arbiter(wt, case, stages, agent, model)
            arb["agent"] = f"{agent}:{model}"
            contents = arb.pop("contents", None)
            if contents:
                (out_dir / "resolutions").mkdir(parents=True, exist_ok=True)
                (out_dir / "resolutions" / f"{case['id']}.json").write_text(json.dumps(contents), encoding="utf-8")
            if arb["status"] == "written":
                arb.update(score(contents, human, stages))
                arb["tests"] = gate_passes(run_tests(wt, venv), human_run) if record["test_gate_usable"] else None
                arb["accepted"] = bool(arb["tests"]) if record["test_gate_usable"] else None
            else:
                arb["accepted"] = False
            record["arbiter"] = arb
        return record
    finally:
        git(wt, "merge", "--abort", check=False)
        with repo_lock(case["repo"]):
            git(repo, "worktree", "remove", "--force", str(wt), check=False)
            shutil.rmtree(wt, ignore_errors=True)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cases", type=Path, required=True)
    parser.add_argument("--repos", type=Path, required=True)
    parser.add_argument("--venvs", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--agent", choices=sorted(AGENTS), default="claude")
    parser.add_argument("--model", default="sonnet")
    parser.add_argument("--only", nargs="*")
    parser.add_argument("--limit", type=int)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--skip-agent", action="store_true")
    args = parser.parse_args()

    cases = json.loads(args.cases.read_text(encoding="utf-8"))
    if args.only:
        cases = [c for c in cases if c["id"] in args.only]
    if args.limit:
        cases = cases[: args.limit]
    args.out.mkdir(parents=True, exist_ok=True)
    args.work.mkdir(parents=True, exist_ok=True)
    results_path = args.out / "results.jsonl"
    done = set()
    if results_path.exists():
        done = {json.loads(line)["id"] for line in results_path.read_text(encoding="utf-8").splitlines() if line.strip()}
    todo = [c for c in cases if c["id"] not in done]
    print(f"{len(todo)} cases to run ({len(done)} already done)", flush=True)

    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = {pool.submit(run_case, c, args.repos, args.venvs, args.work, args.out, args.agent, args.model, args.skip_agent): c for c in todo}
        for future in as_completed(futures):
            case = futures[future]
            try:
                record = future.result()
            except UsageLimitHit as err:
                print(f"{case['id']}: usage limit hit, not recorded: {err}", flush=True)
                for pending in futures:
                    pending.cancel()
                continue
            except Exception as err:  # noqa: BLE001
                record = {"id": case["id"], "error": repr(err)}
            with write_lock:
                with results_path.open("a", encoding="utf-8") as fh:
                    fh.write(json.dumps(record) + "\n")
            arb = record.get("arbiter", {})
            print(f"{record['id']}: gate={record.get('test_gate_usable')} human_fail={len(((record.get('human') or {}).get('run') or {}).get('failed', []))} "
                  f"arbiter={arb.get('status')} exact={arb.get('exact')} sim={arb.get('similarity')} tests={arb.get('tests')} "
                  f"{record.get('skipped') or record.get('error') or arb.get('reason') or ''}", flush=True)


if __name__ == "__main__":
    main()
