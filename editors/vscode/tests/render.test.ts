import { describe, expect, it } from "vitest";
import { escapeHtml, formatTimestamp, Operation, renderHtml } from "../src/render";

// Real captured output from a real `pact history --json` run (two agents
// claiming/releasing/messaging via the pact-coord Python binding against
// a real throwaway repo) -- not fabricated, see DESIGN.md ("VS Code
// extension v1", issue #128).
const REAL_OPERATIONS: Operation[] = [
  {
    id: 3,
    created_at: 1784906399,
    op_type: "release",
    workspace_id: "agent-a",
    detail: { patterns: ["src/**/*.py"], released: 1 },
  },
  {
    id: 2,
    created_at: 1784906399,
    op_type: "broadcast",
    workspace_id: "agent-a",
    detail: { to: null, subject: "heads up" },
  },
  {
    id: 1,
    created_at: 1784906399,
    op_type: "claim",
    workspace_id: "agent-a",
    detail: { patterns: ["src/**/*.py"], has_conflicts: false },
  },
];

describe("escapeHtml", () => {
  it("escapes the five HTML-significant characters", () => {
    expect(escapeHtml(`<script>alert("x")</script> & 'y'`)).toBe(
      `&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; 'y'`,
    );
  });
});

describe("formatTimestamp", () => {
  it("converts a unix-seconds timestamp to a locale date string", () => {
    // Just confirm it's a non-empty, parseable-looking date, not an
    // exact string -- locale formatting is environment-dependent.
    const formatted = formatTimestamp(1784906399);
    expect(formatted.length).toBeGreaterThan(0);
    expect(new Date(formatted).getTime()).not.toBeNaN();
  });
});

describe("renderHtml", () => {
  it("renders a row per operation, real captured data", () => {
    const html = renderHtml(REAL_OPERATIONS, undefined);
    expect(html).toContain("<table>");
    expect(html).toContain("agent-a");
    expect(html).toContain("claim");
    expect(html).toContain("release");
    expect(html).toContain("broadcast");
    expect(html).toContain("heads up");
    // detail JSON gets pretty-printed and HTML-escaped inline
    expect(html).toContain("has_conflicts");
  });

  it("shows an empty-state message for zero operations", () => {
    const html = renderHtml([], undefined);
    expect(html).toContain("No operations recorded yet");
    expect(html).not.toContain("<table>");
  });

  it("shows an error message instead of a table when pact history fails", () => {
    const html = renderHtml([], "no such file or directory");
    expect(html).toContain("Failed to load pact history");
    expect(html).toContain("no such file or directory");
    expect(html).not.toContain("<table>");
  });

  it("escapes detail content so it can't break out of the <pre> block", () => {
    const malicious: Operation[] = [
      {
        id: 1,
        created_at: 1784906399,
        op_type: "message",
        workspace_id: "agent-a",
        detail: { subject: "</pre><script>alert(1)</script>" },
      },
    ];
    const html = renderHtml(malicious, undefined);
    expect(html).not.toContain("<script>alert(1)</script>");
    expect(html).toContain("&lt;script&gt;alert(1)&lt;/script&gt;");
  });

  it("falls back to the default color for an unrecognized op_type", () => {
    const unknown: Operation[] = [
      { id: 1, created_at: 1784906399, op_type: "some_future_type", workspace_id: "agent-a", detail: {} },
    ];
    const html = renderHtml(unknown, undefined);
    expect(html).toContain("some_future_type");
    expect(html).toContain("var(--vscode-foreground)");
  });
});
