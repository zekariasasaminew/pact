# pact: Operation History (VS Code extension, v1)

A minimal webview visualizing pact's operation log (`pact history`) --
every claim, release, message, merge-all, arbiter decision, and teardown
recorded for the open workspace's repo.

Not published to the Marketplace yet -- this is a working v1, install
from source:

```sh
cd editors/vscode
npm install
npm run build
```

Then in VS Code: `Extensions` > `...` menu > `Install from VSIX...` (once
packaged with `vsce package`), or run it in an Extension Development Host
via F5 from this folder.

## Usage

Open a folder that's a pact-managed git repo, run the command **pact:
Show Operation History** (Command Palette), and a panel opens showing
the same data as `pact history --json`, rendered as a table with a
**Refresh** button. If `pact` isn't on `PATH`, set `pact.binaryPath` in
Settings.

## Scope (v1)

Deliberately minimal: one read-only view, no auto-refresh, no filtering
UI (use `pact history`'s own `--workspace`/`--type`/`--since` flags from
a terminal for that). See pact's own `DESIGN.md` ("VS Code extension v1",
issue #128) for what's genuinely verified vs. not yet for this extension
specifically.
