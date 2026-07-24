/**
 * Thin, opinionated TypeScript client for pact-coord, pact's MCP-based
 * coordination server (file leases + inter-agent messaging).
 *
 * Spawns `pact mcp-serve` itself and speaks real MCP (via Anthropic's own
 * `@modelcontextprotocol/sdk`) over stdio -- pact's own DESIGN.md
 * ("pact-coord SDK bindings v1", issue #127) covers why this is the
 * right shape (there is no standing coordination server to connect to
 * instead) and why the response parsing below is asymmetric
 * (claimFiles/checkMessages return real JSON text; releaseFiles/
 * sendMessage return a plain human-readable sentence, by pact-coord's
 * own design, not an oversight here).
 */

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

export class PactCoordError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "PactCoordError";
  }
}

export interface Conflict {
  holder: string;
  pattern: string;
  exampleFiles: string[];
}

/** Mirrors pact-coord's `ClaimResult` field-for-field. `accepted` is
 * always true -- pact-coord records every claim it's given, full stop;
 * leases are advisory, not exclusive. Check `hasConflicts`/`conflicts`
 * yourself before assuming you hold a file alone. */
export interface ClaimResult {
  accepted: boolean;
  expiresAt: number;
  hasConflicts: boolean;
  conflicts: Conflict[];
}

export interface Message {
  id: number;
  from: string;
  /** `undefined` means this was a broadcast, not addressed to one agent. */
  to?: string;
  subject: string;
  body: string;
  createdAt: number;
}

interface RawClaimResult {
  accepted: boolean;
  expires_at: number;
  has_conflicts: boolean;
  conflicts?: { holder: string; pattern: string; example_files: string[] }[];
}

interface RawMessage {
  id: number;
  from: string;
  to?: string | null;
  subject: string;
  body: string;
  created_at: number;
}

function parseClaimResult(text: string): ClaimResult {
  const data = JSON.parse(text) as RawClaimResult;
  return {
    accepted: data.accepted,
    expiresAt: data.expires_at,
    hasConflicts: data.has_conflicts,
    conflicts: (data.conflicts ?? []).map((c) => ({
      holder: c.holder,
      pattern: c.pattern,
      exampleFiles: c.example_files,
    })),
  };
}

function parseMessages(text: string): Message[] {
  const data = JSON.parse(text) as RawMessage[];
  return data.map((m) => ({
    id: m.id,
    from: m.from,
    to: m.to ?? undefined,
    subject: m.subject,
    body: m.body,
    createdAt: m.created_at,
  }));
}

export interface SpawnOptions {
  /** Path (or bare name, resolved via PATH) to the pact binary. Defaults to "pact". */
  pactBin?: string;
}

/**
 * An open MCP session speaking pact-coord's four tools.
 *
 * Construct via `PactCoordClient.spawn(...)` (spawns `pact mcp-serve`
 * itself), then call `close()` when done -- or use `withClient` to have
 * that handled automatically.
 */
export class PactCoordClient {
  private constructor(private readonly session: Client) {}

  /**
   * Spawns `pact --repo <repoRoot> mcp-serve --agent-id <agentId>
   * --workspace <workspace>` and returns a connected `PactCoordClient`.
   * Caller is responsible for calling `close()` when done.
   */
  static async spawn(
    repoRoot: string,
    agentId: string,
    workspace: string,
    options: SpawnOptions = {},
  ): Promise<PactCoordClient> {
    const transport = new StdioClientTransport({
      command: options.pactBin ?? "pact",
      args: ["--repo", repoRoot, "mcp-serve", "--agent-id", agentId, "--workspace", workspace],
    });
    const client = new Client({ name: "pact-coord-ts", version: "0.1.0" });
    await client.connect(transport);
    return new PactCoordClient(client);
  }

  /** Claims an advisory lease on the given glob patterns. Never enforced
   * against other agents -- always accepted; check `hasConflicts`/
   * `conflicts` on the result yourself. */
  async claimFiles(globs: string[], ttlSeconds?: number): Promise<ClaimResult> {
    const args: Record<string, unknown> = { globs };
    if (ttlSeconds !== undefined) args.ttl_seconds = ttlSeconds;
    const text = await this.call("claim_files", args);
    return parseClaimResult(text);
  }

  /** Releases previously-claimed glob patterns. Returns pact-coord's own
   * human-readable confirmation text (e.g. "released 2 lease(s)") --
   * not parsed into a count, since that text is a sentence, not a
   * stable machine-readable format. */
  async releaseFiles(globs: string[]): Promise<string> {
    return this.call("release_files", { globs });
  }

  /** Sends a message to `to` (an agent id), or broadcasts if `to` is
   * omitted. Returns pact-coord's own confirmation text (e.g. "sent
   * message 42"). */
  async sendMessage(subject: string, body: string, to?: string): Promise<string> {
    const args: Record<string, unknown> = { subject, body };
    if (to !== undefined) args.to = to;
    return this.call("send_message", args);
  }

  /** Messages sent to this agent directly or broadcast, since this
   * agent last checked. */
  async checkMessages(): Promise<Message[]> {
    const text = await this.call("check_messages", {});
    return parseMessages(text);
  }

  async close(): Promise<void> {
    await this.session.close();
  }

  private async call(tool: string, args: Record<string, unknown>): Promise<string> {
    const result = await this.session.callTool({ name: tool, arguments: args });
    const content = (result.content ?? []) as { type: string; text?: string }[];
    const text = content
      .filter((block) => block.type === "text" && typeof block.text === "string")
      .map((block) => block.text as string)
      .join("");
    if (result.isError) {
      throw new PactCoordError(text);
    }
    return text;
  }
}

/** Spawns a client, runs `fn`, and closes the client afterward
 * regardless of whether `fn` throws. */
export async function withClient<T>(
  repoRoot: string,
  agentId: string,
  workspace: string,
  fn: (client: PactCoordClient) => Promise<T>,
  options: SpawnOptions = {},
): Promise<T> {
  const client = await PactCoordClient.spawn(repoRoot, agentId, workspace, options);
  try {
    return await fn(client);
  } finally {
    await client.close();
  }
}
