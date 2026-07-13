use serde_json::Value;

use crate::adapter::{AgentAdapter, CoordConfig};
use crate::event::AgentEvent;

/// **Not live-verified** -- unlike Claude Code and Copilot CLI, `codex` was
/// not installed on the machine this project was built on, so nothing
/// here has actually been run. Everything below is built from OpenAI's own
/// published docs (developers.openai.com/codex, non-interactive-mode and
/// config-reference pages), not observed behavior. Treat this adapter as
/// "should be roughly right per the docs," not "confirmed working" --
/// the other two adapters in this crate earned that claim by being
/// launched for real; this one hasn't, and says so rather than implying
/// otherwise.
pub struct CodexAdapter;

impl AgentAdapter for CodexAdapter {
    fn coord_server_name(&self) -> &'static str {
        "agentyard-coord"
    }

    fn default_safety_description(&self) -> &'static str {
        "--ask-for-approval never --sandbox danger-full-access"
    }

    /// Per docs: `--json` emits JSONL, one line per event, and current
    /// approval-policy values are `untrusted`/`on-request`/`never`
    /// (`on-failure` still parses but is deprecated).
    ///
    /// MCP servers are normally configured via `$CODEX_HOME/config.toml`,
    /// not a per-invocation flag -- but pointing `CODEX_HOME` at a
    /// per-workspace directory would also relocate Codex's auth/session
    /// state (credentials, history, its own SQLite db), which plausibly
    /// breaks headless login on first use. Using inline `-c
    /// mcp_servers.<id>.*` overrides instead (also documented, supports
    /// dotted keys) avoids touching auth state entirely -- no config file
    /// is written for this adapter at all.
    fn build_command(
        &self,
        task: &str,
        safety_override: Option<&str>,
        coord: Option<&CoordConfig>,
    ) -> (String, Vec<String>) {
        let approval = safety_override.unwrap_or("never");
        let mut args = vec![
            "exec".to_string(),
            task.to_string(),
            "--json".to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
            "--ask-for-approval".to_string(),
            approval.to_string(),
        ];
        if let Some(coord) = coord {
            args.push("-c".to_string());
            args.push(format!(
                "mcp_servers.{}.command={}",
                coord.server_name,
                toml_string(&coord.command)
            ));
            args.push("-c".to_string());
            args.push(format!(
                "mcp_servers.{}.args={}",
                coord.server_name,
                toml_string_array(&coord.args)
            ));
        }
        ("codex".to_string(), args)
    }

    /// No confirmed schema exists for this (see the struct-level doc
    /// comment) -- every line is passed through as `Other` rather than
    /// guessing specific field names that would look supported while
    /// silently misparsing. Concretely, this means the coordination
    /// connectivity check and Result-based success/failure detection do
    /// *not* work for this adapter yet; that's honestly represented by
    /// producing no `CoordStatus`/`Result` events at all, not by faking
    /// them.
    fn parse_line(&self, line: &str) -> Vec<AgentEvent> {
        match serde_json::from_str::<Value>(line) {
            Ok(value) => vec![AgentEvent::Other(value)],
            Err(_) => vec![AgentEvent::Other(Value::String(line.to_string()))],
        }
    }
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn toml_string_array(items: &[String]) -> String {
    let rendered: Vec<String> = items.iter().map(|s| toml_string(s)).collect();
    format!("[{}]", rendered.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing about this adapter that's checkable without the real
    /// `codex` binary: that a Windows path (backslashes) and the args list
    /// round-trip into valid, correctly-escaped TOML literal syntax for a
    /// `-c mcp_servers.<id>.*` override. Everything else about this
    /// adapter's correctness depends on a schema this project has no
    /// confirmed access to -- see the module doc comment.
    #[test]
    fn build_command_escapes_windows_paths_for_toml() {
        let coord = CoordConfig {
            server_name: "agentyard-coord".to_string(),
            command: r"C:\Users\test\agentyard.exe".to_string(),
            args: vec!["mcp-serve".to_string(), r"--workspace".to_string()],
            config_path: std::path::PathBuf::new(),
        };
        let (program, args) = CodexAdapter.build_command("do the thing", None, Some(&coord));
        assert_eq!(program, "codex");

        let command_arg = args
            .iter()
            .find(|a| a.starts_with("mcp_servers.agentyard-coord.command="))
            .expect("command override present");
        assert_eq!(
            command_arg,
            r#"mcp_servers.agentyard-coord.command="C:\\Users\\test\\agentyard.exe""#
        );

        let args_arg = args
            .iter()
            .find(|a| a.starts_with("mcp_servers.agentyard-coord.args="))
            .expect("args override present");
        assert_eq!(
            args_arg,
            r#"mcp_servers.agentyard-coord.args=["mcp-serve","--workspace"]"#
        );
    }

    #[test]
    fn build_command_omits_mcp_overrides_when_no_coord_config() {
        let (_, args) = CodexAdapter.build_command("do the thing", None, None);
        assert!(!args.iter().any(|a| a.contains("mcp_servers")));
    }
}
