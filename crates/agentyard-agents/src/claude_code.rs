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

/// Program name and args for a headless Claude Code launch.
///
/// `--mcp-config` is deliberately not included yet -- Phase 3 (the
/// coordination server) will add it here once there's an MCP server to
/// point at, without needing to change this function's shape.
pub fn build_command(task: &str, permission_mode: &str) -> (String, Vec<String>) {
    (
        "claude".to_string(),
        vec![
            "-p".to_string(),
            task.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--permission-mode".to_string(),
            permission_mode.to_string(),
        ],
    )
}
