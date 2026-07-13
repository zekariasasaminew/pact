/// The default permission mode for headless launches. Not a quiet default:
/// callers (the CLI) are expected to warn loudly whenever this is the mode
/// actually in effect, since it means the agent bypasses every permission
/// check with no human in the loop. It's the *only* mode of the six
/// Claude Code offers that's safe from hanging forever in headless use --
/// there is no TTY to answer an interactive prompt, and every mode short of
/// bypassing still gates at least some tool categories (e.g. `acceptEdits`
/// auto-accepts file edits but still prompts for arbitrary Bash/tool
/// calls), so a task that touches one of those would just hang rather than
/// running or being denied.
pub const DEFAULT_PERMISSION_MODE: &str = "bypassPermissions";

/// Name the coordination server is registered under in the generated MCP
/// config -- used both when writing that config and when checking the
/// init event's connection status for it.
pub const COORD_SERVER_NAME: &str = "agentyard-coord";

/// Program name and args for a headless Claude Code launch. `mcp_config`,
/// when present, is a path to a JSON file of the shape
/// `{"mcpServers": {...}}` (confirmed against the real CLI -- an unwrapped
/// file is rejected with a loud error before the session starts, so a
/// config that loads at all is never silently ignored for a shape reason).
pub fn build_command(
    task: &str,
    permission_mode: &str,
    mcp_config: Option<&std::path::Path>,
) -> (String, Vec<String>) {
    let mut args = vec![
        "-p".to_string(),
        task.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--permission-mode".to_string(),
        permission_mode.to_string(),
    ];
    if let Some(path) = mcp_config {
        args.push("--mcp-config".to_string());
        args.push(path.to_string_lossy().to_string());
    }
    ("claude".to_string(), args)
}
