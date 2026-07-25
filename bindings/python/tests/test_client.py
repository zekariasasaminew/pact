"""Real end-to-end coverage: spawns a real `pact mcp-serve` subprocess
(built from this repo) against a real throwaway git repo, no mocking --
spawning it costs nothing and involves no LLM, so there's no reason to
stub it the way a real agent-CLI call would need to be. See DESIGN.md
("pact-coord SDK bindings v1", issue #127)."""

import shutil
import subprocess
import uuid
from pathlib import Path

import pytest

from pact_coord import ActiveLease, PactCoordClient, PactCoordError

REPO_ROOT = Path(__file__).resolve().parents[3]


def _find_pact_binary() -> str:
    for profile in ("debug", "release"):
        candidate = REPO_ROOT / "target" / profile / "pact.exe"
        if candidate.exists():
            return str(candidate)
        candidate = REPO_ROOT / "target" / profile / "pact"
        if candidate.exists():
            return str(candidate)
    found = shutil.which("pact")
    if found:
        return found
    pytest.skip("no built pact binary found under target/{debug,release} and pact isn't on PATH")


def _run_git(cwd: Path, *args: str) -> None:
    result = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    assert result.returncode == 0, f"git {args} failed: {result.stderr}"


@pytest.fixture
def scratch_repo(tmp_path: Path) -> Path:
    repo = tmp_path / f"pact-coord-py-test-{uuid.uuid4()}"
    repo.mkdir()
    _run_git(repo, "init", "-q")
    _run_git(repo, "config", "user.email", "test@test.com")
    _run_git(repo, "config", "user.name", "test")
    (repo / "README.md").write_text("# demo\n")
    _run_git(repo, "add", "-A")
    _run_git(repo, "commit", "-q", "-m", "init")
    return repo


@pytest.mark.asyncio
async def test_claim_files_reports_no_conflicts_when_no_one_else_holds_the_glob(scratch_repo: Path) -> None:
    pact_bin = _find_pact_binary()
    async with PactCoordClient.spawn(scratch_repo, "agent-a", scratch_repo, pact_bin=pact_bin) as client:
        result = await client.claim_files(["src/**/*.py"])
        assert result.accepted is True
        assert result.has_conflicts is False
        assert result.conflicts == []


@pytest.mark.asyncio
async def test_two_agents_claiming_overlapping_globs_surfaces_a_conflict(scratch_repo: Path) -> None:
    # claim_files matches glob patterns against real files on disk (see
    # pact-coord's expand_glob) -- a pattern naming a file that doesn't
    # exist yet expands to nothing, so no conflict could ever be detected
    # against it. Real usage claims a file that's actually there.
    (scratch_repo / "src").mkdir()
    (scratch_repo / "src" / "shared.py").write_text("# shared\n")

    pact_bin = _find_pact_binary()
    async with PactCoordClient.spawn(scratch_repo, "agent-a", scratch_repo, pact_bin=pact_bin) as client_a:
        await client_a.claim_files(["src/shared.py"])

    async with PactCoordClient.spawn(scratch_repo, "agent-b", scratch_repo, pact_bin=pact_bin) as client_b:
        result = await client_b.claim_files(["src/shared.py"])
        assert result.accepted is True, "leases are advisory -- a conflicting claim is still accepted"
        assert result.has_conflicts is True
        assert any(c.holder == "agent-a" for c in result.conflicts)


@pytest.mark.asyncio
async def test_list_claims_shows_active_leases_across_agents(scratch_repo: Path) -> None:
    pact_bin = _find_pact_binary()
    async with PactCoordClient.spawn(scratch_repo, "agent-a", scratch_repo, pact_bin=pact_bin) as client:
        assert await client.list_claims() == []

        await client.claim_files(["src/shared.py"])
        active = await client.list_claims()
        assert len(active) == 1
        assert isinstance(active[0], ActiveLease)
        assert active[0].holder == "agent-a"
        assert active[0].pattern == "src/shared.py"

        # list_claims is read-only -- calling it again doesn't consume
        # or change anything, unlike check_messages.
        assert await client.list_claims() == active


@pytest.mark.asyncio
async def test_release_files_returns_pact_coords_own_confirmation_text(scratch_repo: Path) -> None:
    pact_bin = _find_pact_binary()
    async with PactCoordClient.spawn(scratch_repo, "agent-a", scratch_repo, pact_bin=pact_bin) as client:
        await client.claim_files(["src/a.py", "src/b.py"])
        text = await client.release_files(["src/a.py", "src/b.py"])
        assert "released" in text
        assert "2" in text


@pytest.mark.asyncio
async def test_send_and_check_messages_round_trip(scratch_repo: Path) -> None:
    pact_bin = _find_pact_binary()
    async with PactCoordClient.spawn(scratch_repo, "agent-a", scratch_repo, pact_bin=pact_bin) as client_a:
        await client_a.send_message("heads up", "renamed foo() to bar()", to="agent-b")

    async with PactCoordClient.spawn(scratch_repo, "agent-b", scratch_repo, pact_bin=pact_bin) as client_b:
        messages = await client_b.check_messages()
        assert len(messages) == 1
        assert messages[0].from_ == "agent-a"
        assert messages[0].subject == "heads up"
        assert messages[0].body == "renamed foo() to bar()"

        # check_messages only returns what's arrived since last checked.
        again = await client_b.check_messages()
        assert again == []


@pytest.mark.asyncio
async def test_broadcast_message_omits_to(scratch_repo: Path) -> None:
    pact_bin = _find_pact_binary()
    async with PactCoordClient.spawn(scratch_repo, "agent-a", scratch_repo, pact_bin=pact_bin) as client_a:
        await client_a.send_message("all-hands", "starting the refactor")

    async with PactCoordClient.spawn(scratch_repo, "agent-b", scratch_repo, pact_bin=pact_bin) as client_b:
        messages = await client_b.check_messages()
        assert len(messages) == 1
        assert messages[0].to is None


@pytest.mark.asyncio
async def test_malformed_glob_raises_pact_coord_error(scratch_repo: Path) -> None:
    pact_bin = _find_pact_binary()
    async with PactCoordClient.spawn(scratch_repo, "agent-a", scratch_repo, pact_bin=pact_bin) as client:
        with pytest.raises(PactCoordError):
            await client.claim_files(["["])
