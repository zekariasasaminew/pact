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
});
