# pact-coord (Python)

A thin Python client for [pact-coord](https://github.com/zekariasasaminew/pact),
pact's MCP-based file-lease/messaging coordination server -- for building
your own agent framework/tooling against the same coordination layer
pact's own Claude Code/Copilot/Codex/Gemini adapters use, without going
through pact's spawn/merge orchestration at all.

Not published to PyPI yet (v1, no external users yet) -- install directly:

```sh
pip install /path/to/pact/bindings/python
# or, from a git checkout:
pip install "git+https://github.com/zekariasasaminew/pact.git#subdirectory=bindings/python"
```

Requires a `pact` binary on `PATH` (or an explicit path) -- this client
spawns `pact mcp-serve` itself and speaks real MCP over stdio; it does
not talk to any standing network server, because pact-coord doesn't run
as one. See pact's own `DESIGN.md` ("pact-coord SDK bindings v1") for why.

Tested against pact `v0.3.0`'s wire shape; if a future pact release
changes `pact-coord`'s tool schemas, pin to a matching pact version.

## Usage

```python
import asyncio
from pact_coord import PactCoordClient

async def main():
    async with PactCoordClient.spawn("/path/to/repo", "my-agent", "/path/to/repo") as client:
        result = await client.claim_files(["src/**/*.py"])
        if result.has_conflicts:
            print("someone else is already touching:", result.conflicts)

        await client.send_message("heads up", "renamed foo() to bar()")

        for msg in await client.check_messages():
            print(msg.from_, msg.subject, msg.body)

        await client.release_files(["src/**/*.py"])

asyncio.run(main())
```

## API

- `claim_files(globs, *, ttl_seconds=None) -> ClaimResult` -- `accepted`
  is always `True` (leases are advisory, never enforced); check
  `has_conflicts`/`conflicts` yourself.
- `release_files(globs) -> str` -- pact-coord's own confirmation text
  (e.g. `"released 2 lease(s)"`), not parsed into a count -- that text is
  a sentence, not a stable machine-readable format, by pact-coord's own
  design.
- `send_message(subject, body, *, to=None) -> str` -- omit `to` to
  broadcast. Returns pact-coord's own confirmation text.
- `check_messages() -> list[Message]` -- messages sent to this agent or
  broadcast, since this agent last checked.

All four raise `PactCoordError` (carrying pact-coord's own error text) on
an MCP `isError: true` response.
