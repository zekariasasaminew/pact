// Real end-to-end coverage: spawns a real `pact mcp-serve` subprocess
// (built from this repo) against a real throwaway git repo, no mocking
// -- spawning it costs nothing and involves no LLM, so there's no reason
// to stub it the way a real agent-CLI call would need to be. See
// DESIGN.md ("pact-coord SDK bindings v1", issue #127).

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { PactCoordError, withClient } from "../src/client.js";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = join(__dirname, "..", "..", "..");

function findPactBinary(): string {
  for (const profile of ["debug", "release"]) {
    for (const name of ["pact.exe", "pact"]) {
      const candidate = join(REPO_ROOT, "target", profile, name);
      if (existsSync(candidate)) return candidate;
    }
  }
  throw new Error("no built pact binary found under target/{debug,release} -- run `cargo build` first");
}

function runGit(cwd: string, ...args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "pipe" });
}

let pactBin: string;
let scratchRepo: string;

beforeEach(() => {
  pactBin = findPactBinary();
  scratchRepo = mkdtempSync(join(tmpdir(), "pact-coord-ts-test-"));
  runGit(scratchRepo, "init", "-q");
  runGit(scratchRepo, "config", "user.email", "test@test.com");
  runGit(scratchRepo, "config", "user.name", "test");
  writeFileSync(join(scratchRepo, "README.md"), "# demo\n");
  runGit(scratchRepo, "add", "-A");
  runGit(scratchRepo, "commit", "-q", "-m", "init");
});

afterEach(() => {
  // Best-effort -- a lingering scratch repo under the OS temp dir is
  // harmless clutter, not worth failing the test suite over.
});

describe("PactCoordClient", () => {
  it("reports no conflicts when no one else holds the glob", async () => {
    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      const result = await client.claimFiles(["src/**/*.py"]);
      expect(result.accepted).toBe(true);
      expect(result.hasConflicts).toBe(false);
      expect(result.conflicts).toEqual([]);
    }, { pactBin });
  });

  it("surfaces a real conflict when two agents claim overlapping globs", async () => {
    // claim_files matches glob patterns against real files on disk (see
    // pact-coord's expand_glob) -- a pattern naming a file that doesn't
    // exist yet expands to nothing, so no conflict could ever be
    // detected against it. Real usage claims a file that's actually there.
    mkdirSync(join(scratchRepo, "src"));
    writeFileSync(join(scratchRepo, "src", "shared.py"), "# shared\n");

    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      await client.claimFiles(["src/shared.py"]);
    }, { pactBin });

    await withClient(scratchRepo, "agent-b", scratchRepo, async (client) => {
      const result = await client.claimFiles(["src/shared.py"]);
      expect(result.accepted).toBe(true);
      expect(result.hasConflicts).toBe(true);
      expect(result.conflicts.some((c) => c.holder === "agent-a")).toBe(true);
    }, { pactBin });
  });

  it("rejects instead of accepting when failOnConflict is set on an overlapping claim", async () => {
    mkdirSync(join(scratchRepo, "src"));
    writeFileSync(join(scratchRepo, "src", "shared.py"), "# shared\n");

    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      await client.claimFiles(["src/shared.py"]);
    }, { pactBin });

    await withClient(scratchRepo, "agent-b", scratchRepo, async (client) => {
      await expect(client.claimFiles(["src/shared.py"], undefined, true)).rejects.toBeInstanceOf(PactCoordError);
    }, { pactBin });
  });

  it("lists active leases across agents via listClaims", async () => {
    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      expect(await client.listClaims()).toEqual([]);

      await client.claimFiles(["src/shared.py"]);
      const active = await client.listClaims();
      expect(active).toHaveLength(1);
      expect(active[0].holder).toBe("agent-a");
      expect(active[0].pattern).toBe("src/shared.py");

      // Read-only -- calling it again doesn't consume or change anything.
      expect(await client.listClaims()).toEqual(active);
    }, { pactBin });
  });

  it("returns pact-coord's own confirmation text from releaseFiles", async () => {
    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      await client.claimFiles(["src/a.py", "src/b.py"]);
      const text = await client.releaseFiles(["src/a.py", "src/b.py"]);
      expect(text).toContain("released");
      expect(text).toContain("2");
    }, { pactBin });
  });

  it("round-trips a direct message between two agents", async () => {
    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      await client.sendMessage("heads up", "renamed foo() to bar()", "agent-b");
    }, { pactBin });

    await withClient(scratchRepo, "agent-b", scratchRepo, async (client) => {
      const messages = await client.checkMessages();
      expect(messages).toHaveLength(1);
      expect(messages[0].from).toBe("agent-a");
      expect(messages[0].subject).toBe("heads up");
      expect(messages[0].body).toBe("renamed foo() to bar()");

      // check_messages only returns what's arrived since last checked.
      const again = await client.checkMessages();
      expect(again).toEqual([]);
    }, { pactBin });
  });

  it("omits `to` on a broadcast message", async () => {
    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      await client.sendMessage("all-hands", "starting the refactor");
    }, { pactBin });

    await withClient(scratchRepo, "agent-b", scratchRepo, async (client) => {
      const messages = await client.checkMessages();
      expect(messages).toHaveLength(1);
      expect(messages[0].to).toBeUndefined();
    }, { pactBin });
  });

  it("raises PactCoordError on a malformed glob", async () => {
    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      await expect(client.claimFiles(["["])).rejects.toBeInstanceOf(PactCoordError);
    }, { pactBin });
  });

  it("delivers a new handoff request to its target immediately", async () => {
    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      const result = await client.requestHandoff("agent-b", ["src/shared.py"], "can I take this?");
      expect(result.requestId).toBeGreaterThan(0);
      expect(result.expiresAt).toBeGreaterThan(0);
    }, { pactBin });

    await withClient(scratchRepo, "agent-b", scratchRepo, async (client) => {
      const incoming = await client.checkHandoffs();
      expect(incoming).toHaveLength(1);
      expect(incoming[0].from).toBe("agent-a");
      expect(incoming[0].status).toBe("pending");
      expect(incoming[0].files).toEqual(["src/shared.py"]);
    }, { pactBin });
  });

  it("does not echo the requester's own still-pending request back to them", async () => {
    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      await client.requestHandoff("agent-b", ["src/shared.py"], "can I take this?");
      const outgoing = await client.checkHandoffs();
      expect(outgoing).toEqual([]);
    }, { pactBin });
  });

  it("round-trips an accepted response back to the requester", async () => {
    let requestId = -1;
    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      const result = await client.requestHandoff("agent-b", ["src/shared.py"], "can I take this?");
      requestId = result.requestId;
    }, { pactBin });

    await withClient(scratchRepo, "agent-b", scratchRepo, async (client) => {
      const text = await client.respondHandoff(requestId, "accept", undefined, "go ahead");
      expect(text).toContain(String(requestId));
    }, { pactBin });

    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      const outgoing = await client.checkHandoffs();
      expect(outgoing).toHaveLength(1);
      expect(outgoing[0].status).toBe("accepted");
      expect(outgoing[0].responseMessage).toBe("go ahead");
    }, { pactBin });
  });

  it("carries the counter-offered scope on a narrowed response", async () => {
    let requestId = -1;
    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      const result = await client.requestHandoff("agent-b", ["src/*.py"], "can I take these?");
      requestId = result.requestId;
    }, { pactBin });

    await withClient(scratchRepo, "agent-b", scratchRepo, async (client) => {
      await client.respondHandoff(requestId, "narrow", ["src/only_this.py"], "only this one is free");
    }, { pactBin });

    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      const outgoing = await client.checkHandoffs();
      expect(outgoing).toHaveLength(1);
      expect(outgoing[0].status).toBe("narrowed");
      expect(outgoing[0].narrowedFiles).toEqual(["src/only_this.py"]);
    }, { pactBin });
  });

  it("raises PactCoordError responding to a request addressed to someone else", async () => {
    let requestId = -1;
    await withClient(scratchRepo, "agent-a", scratchRepo, async (client) => {
      const result = await client.requestHandoff("agent-b", ["src/shared.py"], "can I take this?");
      requestId = result.requestId;
    }, { pactBin });

    await withClient(scratchRepo, "agent-c", scratchRepo, async (client) => {
      await expect(client.respondHandoff(requestId, "accept")).rejects.toBeInstanceOf(PactCoordError);
    }, { pactBin });
  });
});
