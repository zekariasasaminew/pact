/** Pure HTML-rendering logic for the pact history webview -- no
 * dependency on the `vscode` module, so it can be unit-tested directly
 * (see tests/render.test.ts) without a full Extension Host. */

export interface Operation {
  id: number;
  created_at: number;
  op_type: string;
  workspace_id: string;
  detail: unknown;
}

const TYPE_COLORS: Record<string, string> = {
  claim: "var(--vscode-charts-blue)",
  release: "var(--vscode-charts-purple)",
  message: "var(--vscode-charts-green)",
  broadcast: "var(--vscode-charts-green)",
  merge_all: "var(--vscode-charts-orange)",
  arbiter_decision: "var(--vscode-charts-yellow)",
  teardown: "var(--vscode-charts-red)",
};

export function escapeHtml(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

export function formatTimestamp(unixSeconds: number): string {
  return new Date(unixSeconds * 1000).toLocaleString();
}

export function renderHtml(operations: Operation[], error: string | undefined): string {
  const rows = operations
    .map((op) => {
      const color = TYPE_COLORS[op.op_type] ?? "var(--vscode-foreground)";
      return `
        <tr>
          <td class="ts">${escapeHtml(formatTimestamp(op.created_at))}</td>
          <td><span class="badge" style="border-color:${color};color:${color}">${escapeHtml(op.op_type)}</span></td>
          <td class="workspace">${escapeHtml(op.workspace_id)}</td>
          <td><pre class="detail">${escapeHtml(JSON.stringify(op.detail, null, 2))}</pre></td>
        </tr>`;
    })
    .join("\n");

  const body =
    error !== undefined
      ? `<p class="error">Failed to load pact history: ${escapeHtml(error)}</p>
         <p class="hint">Check "pact.binaryPath" in Settings if pact isn't on PATH, and that this folder has a pact coordination log to show.</p>`
      : operations.length === 0
        ? `<p class="empty">No operations recorded yet for this repo.</p>`
        : `<table>
             <thead><tr><th>Time</th><th>Type</th><th>Workspace</th><th>Detail</th></tr></thead>
             <tbody>${rows}</tbody>
           </table>`;

  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<style>
  body {
    font-family: var(--vscode-font-family);
    color: var(--vscode-foreground);
    background: var(--vscode-editor-background);
    padding: 0 16px 16px;
  }
  header {
    position: sticky;
    top: 0;
    background: var(--vscode-editor-background);
    padding: 12px 0;
    display: flex;
    align-items: center;
    justify-content: space-between;
    border-bottom: 1px solid var(--vscode-panel-border);
  }
  h1 { font-size: 1.1em; margin: 0; }
  button {
    background: var(--vscode-button-background);
    color: var(--vscode-button-foreground);
    border: none;
    padding: 4px 12px;
    border-radius: 2px;
    cursor: pointer;
  }
  button:hover { background: var(--vscode-button-hoverBackground); }
  table { width: 100%; border-collapse: collapse; margin-top: 12px; }
  th { text-align: left; font-size: 0.85em; color: var(--vscode-descriptionForeground); padding: 6px 8px; }
  td { padding: 6px 8px; border-top: 1px solid var(--vscode-panel-border); vertical-align: top; }
  td.ts { white-space: nowrap; font-variant-numeric: tabular-nums; color: var(--vscode-descriptionForeground); }
  td.workspace { font-family: var(--vscode-editor-font-family); }
  .badge {
    display: inline-block;
    border: 1px solid;
    border-radius: 100px;
    padding: 1px 8px;
    font-size: 0.85em;
  }
  pre.detail {
    margin: 0;
    font-family: var(--vscode-editor-font-family);
    font-size: 0.85em;
    white-space: pre-wrap;
  }
  .error { color: var(--vscode-errorForeground); }
  .hint, .empty { color: var(--vscode-descriptionForeground); }
</style>
</head>
<body>
  <header>
    <h1>pact: Operation History</h1>
    <button id="refresh">Refresh</button>
  </header>
  ${body}
  <script>
    const vscode = acquireVsCodeApi();
    document.getElementById("refresh").addEventListener("click", () => {
      vscode.postMessage({ command: "refresh" });
    });
  </script>
</body>
</html>`;
}
