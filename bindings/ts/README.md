# pact-coord (TypeScript)

A thin TypeScript client for [pact-coord](https://github.com/zekariasasaminew/pact),
pact's MCP-based file-lease/messaging coordination server -- for building
your own agent framework/tooling against the same coordination layer
pact's own Claude Code/Copilot/Codex/Gemini adapters use, without going
through pact's spawn/merge orchestration at all.

Not published to npm yet (v1, no external users yet) -- install directly:

```sh
npm install /path/to/pact/bindings/ts
# or, from a git checkout:
npm install "github:zekariasasaminew/pact#path:bindings/ts"
```

Requires a `pact` binary on `PATH` (or an explicit path) -- this client
spawns `pact mcp-serve` itself and speaks real MCP over stdio; it does
not talk to any standing network server, because pact-coord doesn't run
as one. See pact's own `DESIGN.md` ("pact-coord SDK bindings v1") for why.

Tested against pact `v0.3.0`'s wire shape; if a future pact release
changes `pact-coord`'s tool schemas, pin to a matching pact version.

## Usage

```ts
import { withClient } from "pact-coord";

await withClient("/path/to/repo", "my-agent", "/path/to/repo", async (client) => {
  const result = await client.claimFiles(["src/**/*.ts"]);
  if (result.hasConflicts) {
    console.log("someone else is already touching:", result.conflicts);
  }

  await client.sendMessage("heads up", "renamed foo() to bar()");

  for (const msg of await client.checkMessages()) {
    console.log(msg.from, msg.subject, msg.body);
  }

  await client.releaseFiles(["src/**/*.ts"]);
});
```

Or manage the client's lifetime yourself with `PactCoordClient.spawn`/`close`.

## API

- `claimFiles(globs, ttlSeconds?) -> ClaimResult` -- `accepted` is always
  `true` (leases are advisory, never enforced); check `hasConflicts`/
  `conflicts` yourself.
- `releaseFiles(globs) -> string` -- pact-coord's own confirmation text
  (e.g. `"released 2 lease(s)"`), not parsed into a count -- that text is
  a sentence, not a stable machine-readable format, by pact-coord's own
  design.
- `sendMessage(subject, body, to?) -> string` -- omit `to` to broadcast.
  Returns pact-coord's own confirmation text.
- `checkMessages() -> Message[]` -- messages sent to this agent or
  broadcast, since this agent last checked.

All four reject with `PactCoordError` (carrying pact-coord's own error
text) on an MCP `isError: true` response.
