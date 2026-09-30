use serde_json::Value;

use crate::adapter::{AgentAdapter, CoordConfig, LaunchRequest, LaunchSpec};
use crate::event::AgentEvent;

pub struct ClaudeCodeAdapter;

/// Common safe operations covering every ecosystem `pact-deps` already
/// knows how to prepare, plus every tool this adapter's own coordination
/// MCP server exposes -- see DESIGN.md ("pact-agents > Claude Code safety
/// default", issue #104) for why the `mcp__pact-coord__*` entry is
/// required, not optional: without it, `claim_files`/`release_files`/
/// `send_message`/`check_messages` are silently denied by Claude Code's
/// own permission gate, even though the MCP server itself is reachable.
const DEFAULT_ALLOWED_TOOLS: &str =
    "Read Write Edit Glob Grep Bash(git *) Bash(npm *) Bash(pnpm *) Bash(yarn *) Bash(cargo *) Bash(go *) Bash(pip *) Bash(uv *) Bash(mvn *) Bash(gradle *) mcp__pact-coord__*";

/// The lean profile's allowlist -- see DESIGN.md ("pact-agents > Claude
/// lean profile", issue #288). Same as the default minus the blanket
/// `Bash(npm *)`/`Bash(pnpm *)`/`Bash(yarn *)`, which permitted `npm
/// install` into a linked, shared `node_modules` (issue #283) and full
/// `npm run build`s; in their place, the JavaScript commands an editor
/// workspace legitimately needs. Anything not listed is denied cleanly
/// under `-p` (confirmed by hand, documented under "Claude Code safety
/// default"), so the deny half of Copilot's profile needs no separate
/// deny list here.
const LEAN_ALLOWED_TOOLS: &str = "Read Write Edit Glob Grep Bash(git *) \
     Bash(node *) Bash(npx tsc *) Bash(npx vitest *) Bash(npx eslint *) Bash(npx prettier *) \
     Bash(npm test*) Bash(npm run lint*) Bash(npm run test*) Bash(npm run typecheck*) Bash(npm ls*) Bash(npm view *) \
     Bash(cargo *) Bash(go *) Bash(pip *) Bash(uv *) Bash(mvn *) Bash(gradle *) mcp__pact-coord__*";

impl AgentAdapter for ClaudeCodeAdapter {
    fn coord_server_name(&self) -> &'static str {
        "pact-coord"
    }

    /// See DESIGN.md ("pact-agents > Claude Code safety default").
    fn default_safety_description(&self) -> &'static str {
        "--allowedTools (curated safe operations, no full permission bypass)"
    }

    /// See DESIGN.md ("pact-agents > Claude Code safety default").
    fn build_command(
        &self,
        task: &str,
        safety_override: Option<&str>,
        coord: Option<&CoordConfig>,
        _workspace_path: &std::path::Path,
    ) -> (String, Vec<String>) {
        let mut args = vec![
            "-p".to_string(),
            task.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--allowedTools".to_string(),
            DEFAULT_ALLOWED_TOOLS.to_string(),
        ];
        if let Some(mode) = safety_override {
            args.push("--permission-mode".to_string());
            args.push(mode.to_string());
        }
        if let Some(coord) = coord {
            if crate::adapter::write_mcp_json_config(&coord.config_path, coord).is_ok() {
                args.push("--mcp-config".to_string());
                args.push(coord.config_path.to_string_lossy().to_string());
            } else {
                tracing::warn!(
                    "failed to write MCP config to {}; launching without coordination",
                    coord.config_path.display()
                );
            }
        }
        ("claude".to_string(), args)
    }

    /// The lean profile -- see DESIGN.md ("pact-agents > Claude lean
    /// profile", issue #288). Measured on a trivial prompt (Claude Code
    /// 2.1.284, haiku): the user's 9 MCP servers made `init` take 14-19 s
    /// and the process 25 s; `--strict-mcp-config` brings that to 5-6 s
    /// and 9-13 s while keeping pact-coord (`--safe-mode` would reach 3 s
    /// but disables every MCP server, pact's own included, and CLAUDE.md).
    fn build_launch(&self, request: &LaunchRequest<'_>) -> LaunchSpec {
        let (program, mut args) =
            self.build_command(request.task, request.safety_override, request.coord, request.workspace_path);
        if !request.lean {
            return LaunchSpec { program, args, env: Vec::new() };
        }
        if let Some(idx) = args.iter().position(|a| a == "--allowedTools") {
            args[idx + 1] = LEAN_ALLOWED_TOOLS.to_string();
        }
        args.extend(["--strict-mcp-config", "--session-id", request.session_id].map(str::to_string));
        if !args.iter().any(|a| a == "--mcp-config") {
            // `--strict-mcp-config` with no `--mcp-config` at all still
            // means "no MCP servers"; an explicit empty config keeps the
            // intent visible in `--dry-run` and in the run metadata.
            let empty = request.agent_home.join("empty-mcp.json");
            if std::fs::create_dir_all(request.agent_home).and_then(|_| std::fs::write(&empty, "{\"mcpServers\":{}}\n")).is_ok() {
                args.push("--mcp-config".to_string());
                args.push(empty.to_string_lossy().to_string());
            }
        }
        LaunchSpec { program, args, env: Vec::new() }
    }

    fn parse_line(&self, line: &str) -> Vec<AgentEvent> {
        parse_line(line)
    }
}

/// Schema modeled against real captured output -- see DESIGN.md
/// ("pact-agents > Claude Code output schema").
fn parse_line(line: &str) -> Vec<AgentEvent> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![AgentEvent::Other(Value::String(line.to_string()))],
    };

    match value.get("type").and_then(Value::as_str) {
        Some("system") if value.get("subtype").and_then(Value::as_str) == Some("init") => {
            let session_id = value
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut events = vec![AgentEvent::Init { session_id }];
            if let Some(servers) = value.get("mcp_servers").and_then(Value::as_array) {
                for server in servers {
                    if let (Some(name), Some(status)) = (
                        server.get("name").and_then(Value::as_str),
                        server.get("status").and_then(Value::as_str),
                    ) {
                        events.push(AgentEvent::CoordStatus {
                            name: name.to_string(),
                            status: status.to_string(),
                        });
                    }
                }
            }
            events
        }
        Some("assistant") => vec![parse_assistant(&value)],
        Some("result") => {
            let success = value.get("is_error").and_then(Value::as_bool) == Some(false);
            let summary = value
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("(no result text)")
                .to_string();
            vec![AgentEvent::Result { success, summary }]
        }
        _ => vec![AgentEvent::Other(value)],
    }
}

fn parse_assistant(value: &Value) -> AgentEvent {
    let content = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array);

    let Some(blocks) = content else {
        return AgentEvent::Other(value.clone());
    };

    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    return AgentEvent::AssistantText(text.to_string());
                }
            }
            Some("tool_use") => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown_tool")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                return AgentEvent::ToolUse { name, input };
            }
            _ => continue,
        }
    }

    AgentEvent::Other(value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_omits_permission_mode_but_includes_allowlist() {
        let (program, args) = ClaudeCodeAdapter.build_command(
            "do the thing",
            None,
            None,
            std::path::Path::new("/tmp/workspace"),
        );
        assert_eq!(program, "claude");
        assert!(args.contains(&"--allowedTools".to_string()));
        assert!(!args.contains(&"--permission-mode".to_string()));
    }

    #[test]
    fn override_adds_explicit_permission_mode_alongside_allowlist() {
        let (_, args) = ClaudeCodeAdapter.build_command(
            "do the thing",
            Some("bypassPermissions"),
            None,
            std::path::Path::new("/tmp/workspace"),
        );
        assert!(args.contains(&"--allowedTools".to_string()));
        let mode_idx = args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(args[mode_idx + 1], "bypassPermissions");
    }

    /// Regression test for issue #104: a real spawn at default safety
    /// denied every coordination MCP tool call, because
    /// `DEFAULT_ALLOWED_TOOLS` never listed them.
    #[test]
    fn default_allowlist_includes_the_coordination_mcp_tools() {
        let (_, args) = ClaudeCodeAdapter.build_command(
            "do the thing",
            None,
            None,
            std::path::Path::new("/tmp/workspace"),
        );
        let idx = args.iter().position(|a| a == "--allowedTools").unwrap();
        assert!(
            args[idx + 1].contains("mcp__pact-coord__*"),
            "expected the coordination MCP tools to be allowed by default, got: {}",
            args[idx + 1]
        );
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pact-agents-claude-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn request<'a>(agent_home: &'a std::path::Path, coord: Option<&'a CoordConfig>, lean: bool) -> LaunchRequest<'a> {
        LaunchRequest {
            task: "do the thing",
            safety_override: None,
            coord,
            workspace_path: std::path::Path::new("/tmp/workspace"),
            agent_home,
            session_id: "11111111-2222-3333-4444-555555555555",
            lean,
        }
    }

    fn allowed_tools(args: &[String]) -> &str {
        let idx = args.iter().position(|a| a == "--allowedTools").unwrap();
        &args[idx + 1]
    }

    #[test]
    fn non_lean_launch_is_exactly_build_command() {
        let home = scratch("non-lean");
        let launch = ClaudeCodeAdapter.build_launch(&request(&home, None, false));
        let (program, args) = ClaudeCodeAdapter.build_command("do the thing", None, None, std::path::Path::new("/tmp/workspace"));
        assert_eq!(launch.program, program);
        assert_eq!(launch.args, args);
        assert!(launch.env.is_empty());
        assert!(!home.join("empty-mcp.json").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn lean_launch_drops_user_mcp_servers_pins_a_session_and_tightens_the_allowlist() {
        let home = scratch("lean");
        let launch = ClaudeCodeAdapter.build_launch(&request(&home, None, true));
        assert!(launch.args.contains(&"--strict-mcp-config".to_string()));
        let sid = launch.args.iter().position(|a| a == "--session-id").expect("--session-id");
        assert_eq!(launch.args[sid + 1], "11111111-2222-3333-4444-555555555555");
        let allowed = allowed_tools(&launch.args);
        for gone in ["Bash(npm *)", "Bash(pnpm *)", "Bash(yarn *)"] {
            assert!(!allowed.contains(gone), "{gone} must not be allowed in the lean profile: {allowed}");
        }
        for kept in ["Bash(git *)", "Bash(npx vitest *)", "Bash(npx tsc *)", "Bash(npm test*)", "mcp__pact-coord__*", "Read", "Edit"] {
            assert!(allowed.contains(kept), "{kept} must stay allowed: {allowed}");
        }
        // No coordination config given: an explicit empty MCP config makes
        // the "no servers" intent visible instead of relying on the flag's
        // implicit behavior.
        let mcp = launch.args.iter().position(|a| a == "--mcp-config").expect("--mcp-config");
        assert_eq!(std::path::Path::new(&launch.args[mcp + 1]), home.join("empty-mcp.json"));
        assert!(home.join("empty-mcp.json").exists());
        assert!(launch.env.is_empty(), "the Claude profile relocates nothing");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn lean_launch_keeps_the_coordination_server_config() {
        let home = scratch("lean-coord");
        let coord = CoordConfig {
            server_name: "pact-coord".to_string(),
            command: "pact".to_string(),
            args: vec!["mcp-serve".to_string()],
            config_path: home.join("coord.json"),
        };
        let launch = ClaudeCodeAdapter.build_launch(&request(&home, Some(&coord), true));
        let mcp_flags: Vec<usize> = launch.args.iter().enumerate().filter(|(_, a)| *a == "--mcp-config").map(|(i, _)| i).collect();
        assert_eq!(mcp_flags.len(), 1, "exactly one --mcp-config, the coordination one: {:?}", launch.args);
        assert_eq!(std::path::Path::new(&launch.args[mcp_flags[0] + 1]), coord.config_path);
        assert!(launch.args.contains(&"--strict-mcp-config".to_string()), "strict mode drops the user's servers but keeps this one");
        assert!(!home.join("empty-mcp.json").exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
