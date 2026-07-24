import { execFile } from "node:child_process";
import * as vscode from "vscode";
import { Operation, renderHtml } from "./render";

let panel: vscode.WebviewPanel | undefined;

export function activate(context: vscode.ExtensionContext): void {
  context.subscriptions.push(vscode.commands.registerCommand("pact.showHistory", () => showHistory()));
}

export function deactivate(): void {
  panel?.dispose();
}

async function showHistory(): Promise<void> {
  const folder = vscode.workspace.workspaceFolders?.[0];
  if (!folder) {
    vscode.window.showErrorMessage("pact: open a folder to show its operation history.");
    return;
  }
  const repoPath = folder.uri.fsPath;

  if (panel) {
    panel.reveal(vscode.ViewColumn.Active);
  } else {
    panel = vscode.window.createWebviewPanel("pactHistory", "pact: Operation History", vscode.ViewColumn.Active, {
      enableScripts: true,
      retainContextWhenHidden: true,
    });
    panel.onDidDispose(() => {
      panel = undefined;
    });
    panel.webview.onDidReceiveMessage((message: { command?: string }) => {
      if (message.command === "refresh") {
        void refresh(repoPath);
      }
    });
  }

  await refresh(repoPath);
}

async function refresh(repoPath: string): Promise<void> {
  if (!panel) {
    return;
  }
  try {
    const operations = await runPactHistory(repoPath);
    panel.webview.html = renderHtml(operations, undefined);
  } catch (err) {
    panel.webview.html = renderHtml([], err instanceof Error ? err.message : String(err));
  }
}

function runPactHistory(repoPath: string): Promise<Operation[]> {
  const config = vscode.workspace.getConfiguration("pact");
  const binary = config.get<string>("binaryPath", "pact");
  return new Promise((resolve, reject) => {
    execFile(
      binary,
      ["--repo", repoPath, "history", "--json"],
      { maxBuffer: 10 * 1024 * 1024 },
      (error, stdout, stderr) => {
        if (error) {
          reject(new Error(stderr.trim() || error.message));
          return;
        }
        try {
          resolve(JSON.parse(stdout) as Operation[]);
        } catch (parseErr) {
          reject(new Error(`failed to parse "pact history --json" output: ${String(parseErr)}`));
        }
      },
    );
  });
}
