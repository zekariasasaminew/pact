"""Thin, opinionated Python client for pact-coord, pact's MCP-based
coordination server (file leases + inter-agent messaging).

Spawns `pact mcp-serve` itself and speaks real MCP (via Anthropic's own
`mcp` package) over stdio -- pact's own DESIGN.md ("pact-coord SDK
bindings v1", issue #127) covers why this is the right shape (there is
no standing coordination server to connect to instead) and why the
response parsing below is asymmetric (claim_files/check_messages return
real JSON text; release_files/send_message return a plain human-readable
sentence, by pact-coord's own design, not an oversight here).
"""

from __future__ import annotations

import json
import shutil
from contextlib import AsyncExitStack
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client


class PactCoordError(Exception):
    """Raised when a pact-coord tool call returns is_error: true.

    Carries the exact error text pact-coord itself produced (e.g. "error:
    invalid glob pattern '['"), not a generic MCP protocol error.
    """


@dataclass
class Conflict:
    holder: str
    pattern: str
    example_files: list[str]


@dataclass
class ClaimResult:
    """Mirrors pact-coord's `ClaimResult` field-for-field.

    `accepted` is always true -- pact-coord records every claim it's
    given, full stop; leases are advisory, not exclusive. Check
    `has_conflicts`/`conflicts` yourself before assuming you hold a file
    alone.
    """

    accepted: bool
    expires_at: int
    has_conflicts: bool
    conflicts: list[Conflict] = field(default_factory=list)


@dataclass
class ActiveLease:
    pattern: str
    holder: str
    expires_at: int


@dataclass
class Message:
    id: int
    from_: str
    to: Optional[str]
    subject: str
    body: str
    created_at: int


def _parse_claim_result(text: str) -> ClaimResult:
    data = json.loads(text)
    return ClaimResult(
        accepted=data["accepted"],
        expires_at=data["expires_at"],
        has_conflicts=data["has_conflicts"],
        conflicts=[
            Conflict(holder=c["holder"], pattern=c["pattern"], example_files=c["example_files"])
            for c in data.get("conflicts", [])
        ],
    )


def _parse_active_leases(text: str) -> list[ActiveLease]:
    data = json.loads(text)
    return [ActiveLease(pattern=l["pattern"], holder=l["holder"], expires_at=l["expires_at"]) for l in data]


def _parse_messages(text: str) -> list[Message]:
    data = json.loads(text)
    return [
        Message(
            id=m["id"],
            from_=m["from"],
            to=m.get("to"),
            subject=m["subject"],
            body=m["body"],
            created_at=m["created_at"],
        )
        for m in data
    ]


class PactCoordClient:
    """An open MCP session speaking pact-coord's four tools.

    Construct via `PactCoordClient.spawn(...)` (spawns `pact mcp-serve`
    itself) used as an async context manager:

        async with PactCoordClient.spawn(repo_root, "my-agent", workspace) as client:
            result = await client.claim_files(["src/**/*.py"])
            if result.has_conflicts:
                ...
            await client.release_files(["src/**/*.py"])
    """

    def __init__(self, session: ClientSession) -> None:
        self._session = session

    @staticmethod
    def spawn(
        repo_root: str | Path,
        agent_id: str,
        workspace: str | Path,
        *,
        pact_bin: str = "pact",
    ) -> "_PactCoordSession":
        """Spawns `pact --repo <repo_root> mcp-serve --agent-id <agent_id>
        --workspace <workspace>` and returns an async context manager
        yielding a connected `PactCoordClient`.

        `pact_bin` is resolved via `shutil.which` first so a bare "pact"
        works the same way it would from a shell with pact on PATH; pass
        an explicit path if pact isn't on PATH.
        """
        resolved = shutil.which(pact_bin) or pact_bin
        params = StdioServerParameters(
            command=resolved,
            args=[
                "--repo",
                str(repo_root),
                "mcp-serve",
                "--agent-id",
                agent_id,
                "--workspace",
                str(workspace),
            ],
        )
        return _PactCoordSession(params)

    async def claim_files(self, globs: list[str], *, ttl_seconds: Optional[int] = None) -> ClaimResult:
        """Claims an advisory lease on the given glob patterns. Never
        enforced against other agents -- always accepted; check
        `has_conflicts`/`conflicts` on the result yourself."""
        args: dict[str, Any] = {"globs": globs}
        if ttl_seconds is not None:
            args["ttl_seconds"] = ttl_seconds
        text = await self._call("claim_files", args)
        return _parse_claim_result(text)

    async def release_files(self, globs: list[str]) -> str:
        """Releases previously-claimed glob patterns. Returns pact-coord's
        own human-readable confirmation text (e.g. "released 2
        lease(s)") -- not parsed into a count, since that text is a
        sentence, not a stable machine-readable format."""
        return await self._call("release_files", {"globs": globs})

    async def send_message(self, subject: str, body: str, *, to: Optional[str] = None) -> str:
        """Sends a message to `to` (an agent id), or broadcasts if `to`
        is omitted. Returns pact-coord's own confirmation text (e.g.
        "sent message 42")."""
        args: dict[str, Any] = {"subject": subject, "body": body}
        if to is not None:
            args["to"] = to
        return await self._call("send_message", args)

    async def check_messages(self) -> list[Message]:
        """Messages sent to this agent directly or broadcast, since this
        agent last checked."""
        text = await self._call("check_messages", {})
        return _parse_messages(text)

    async def list_claims(self) -> list[ActiveLease]:
        """Every currently-unexpired file lease across all agents in this
        coordination session, not just this client's own. Read-only --
        unlike check_messages, calling this never marks anything as read."""
        text = await self._call("list_claims", {})
        return _parse_active_leases(text)

    async def _call(self, tool: str, args: dict[str, Any]) -> str:
        result = await self._session.call_tool(tool, args)
        text = "".join(block.text for block in result.content if hasattr(block, "text"))
        if result.is_error:
            raise PactCoordError(text)
        return text


class _PactCoordSession:
    """Async context manager returned by `PactCoordClient.spawn` --
    owns the subprocess's stdio streams and the MCP session's lifetime,
    both torn down on `__aexit__` regardless of how the `async with`
    block exits."""

    def __init__(self, params: StdioServerParameters) -> None:
        self._params = params
        self._stack: Optional[AsyncExitStack] = None

    async def __aenter__(self) -> PactCoordClient:
        self._stack = AsyncExitStack()
        read, write = await self._stack.enter_async_context(stdio_client(self._params))
        session = await self._stack.enter_async_context(ClientSession(read, write))
        await session.initialize()
        return PactCoordClient(session)

    async def __aexit__(self, *exc_info: object) -> None:
        assert self._stack is not None
        await self._stack.aclose()
