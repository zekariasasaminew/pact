use std::path::Path;

use serde_json::Value;

use crate::adapter::{AcpLaunchRequest, AgentAdapter, CoordConfig, LaunchRequest, LaunchSpec};
use crate::event::AgentEvent;

pub struct CopilotAdapter;

/// Commands a lean Copilot launch denies at the CLI's own tool gate --
/// see DESIGN.md ("pact-agents > Copilot lean profile", issue #284).
/// Dependency mutation would write into a linked, shared `node_modules`
/// (issue #283); full builds and dev servers are the verifier's job and
/// the memory-heaviest thing an editor workspace can do (`next build`
/// measured at 1.9 GB peak). Confirmed by hand: a denied rule produces
/// `tool.execution_complete` with `error.code = "denied"` and the agent
/// adapts, no hang; `shell(npm install:*)` matches the bare command too.
pub const LEAN_DENY_RULES: &[&str] = &[
    "shell(npm install:*)",
    "shell(npm i:*)",
    "shell(npm ci:*)",
    "shell(npm add:*)",
    "shell(npm uninstall:*)",
    "shell(npm rm:*)",
    "shell(npm update:*)",
    "shell(pnpm install:*)",
    "shell(pnpm i:*)",
    "shell(pnpm add:*)",
    "shell(pnpm remove:*)",
    "shell(pnpm update:*)",
    "shell(yarn install:*)",
    "shell(yarn add:*)",
    "shell(yarn remove:*)",
    "shell(bun install:*)",
    "shell(bun add:*)",
    "shell(bun remove:*)",
    "shell(npm run build:*)",
    "shell(npm run dev:*)",
    "shell(pnpm build:*)",
    "shell(pnpm dev:*)",
    "shell(pnpm run build:*)",
    "shell(pnpm run dev:*)",
    "shell(yarn build:*)",
    "shell(yarn dev:*)",
    "shell(next build:*)",
    "shell(next dev:*)",
    "shell(npx next build:*)",
    "shell(npx next dev:*)",
];

/// Files copied verbatim from the user's own `COPILOT_HOME` into a lean
/// per-agent home. `config.json` holds the login pointer (the token
/// itself lives in the OS credential store; confirmed by hand that a home
/// without it fails with "no authenticated GitHub host available" and a
/// home with only it authenticates). `settings.json` holds the user's
/// default model and similar preferences. Deliberately not copied:
/// `mcp-config.json` (the point of the profile), `permissions-config.json`
/// and the session store (isolating them per agent is what avoids the
/// concurrent-write race in github/copilot-cli#3563), and
/// `copilot-instructions.md` (a user's global instructions describe their
/// own workflow, including pushing and opening PRs, which a worker must
/// not do; the repo's own `.github/copilot-instructions.md` still loads
/// from the worktree).
const LEAN_HOME_FILES: &[&str] = &["config.json", "settings.json"];

impl AgentAdapter for CopilotAdapter {
    fn coord_server_name(&self) -> &'static str {
        "pact-coord"
    }

    /// See DESIGN.md ("pact-agents > Copilot CLI safety default").
    fn default_safety_description(&self) -> &'static str {
        "--allow-all-tools (can run any shell command and edit any file with no restriction)"
    }

    /// `safety_override` is accepted for interface consistency but ignored
    /// -- see DESIGN.md ("pact-agents > Copilot CLI safety default").
    fn build_command(
        &self,
        task: &str,
        _safety_override: Option<&str>,
        coord: Option<&CoordConfig>,
        _workspace_path: &std::path::Path,
    ) -> (String, Vec<String>) {
        let mut args = vec![
            "-p".to_string(),
            task.to_string(),
            "--output-format".to_string(),
            "json".to_string(),
            "--allow-all-tools".to_string(),
        ];
        if let Some(coord) = coord {
            if crate::adapter::write_mcp_json_config(&coord.config_path, coord).is_ok() {
                args.push("--additional-mcp-config".to_string());
                args.push(format!("@{}", coord.config_path.to_string_lossy()));
            } else {
                tracing::warn!(
                    "failed to write MCP config to {}; launching without coordination",
                    coord.config_path.display()
                );
            }
        }
        ("copilot".to_string(), args)
    }

    /// The lean profile -- see DESIGN.md ("pact-agents > Copilot lean
    /// profile", issue #284). Measured on the same trivial prompt: the
    /// user's full home with 7 MCP servers took 57 s and 1.37 GB peak; a
    /// per-agent home holding only `config.json` took 6.3 s and 326 MB.
    fn build_launch(&self, request: &LaunchRequest<'_>) -> LaunchSpec {
        let (program, mut args) =
            self.build_command(request.task, request.safety_override, request.coord, request.workspace_path);
        if !request.lean {
            return LaunchSpec { program, args, env: Vec::new() };
        }
        args.extend(["--session-id", request.session_id].map(str::to_string));
        args.extend(lean_args());
        LaunchSpec { program, args, env: lean_home_env(request.agent_home) }
    }

    fn supports_acp(&self) -> bool {
        true
    }

    /// `copilot --acp`: one process hosting one ACP session per lane
    /// (issue #331). Verified live against CLI 1.0.90: eight concurrent
    /// sessions finished a trivial task in 5.6 s and 445 MB where eight
    /// `copilot -p` processes took 50.9 s and 2,456 MB. Same safety
    /// posture as the headless launch (`--allow-all-tools`, see
    /// `default_safety_description`), same lean trimmings when `lean`;
    /// no `--output-format` (events arrive as ACP `session/update`s) and
    /// no `--session-id` (each session gets its own id from the agent).
    fn build_acp_launch(&self, request: &AcpLaunchRequest<'_>) -> Option<LaunchSpec> {
        let mut args = vec!["--acp".to_string(), "--allow-all-tools".to_string()];
        if !request.lean {
            return Some(LaunchSpec { program: "copilot".to_string(), args, env: Vec::new() });
        }
        args.extend(lean_args());
        Some(LaunchSpec { program: "copilot".to_string(), args, env: lean_home_env(request.agent_home) })
    }

    fn parse_line(&self, line: &str) -> Vec<AgentEvent> {
        parse_line(line)
    }
}

/// The lean flags shared by the headless and ACP launches: no built-in
/// MCP servers, no auto-update, and the deny rules that keep a worker from
/// running the long-lived commands that would hang a lane.
fn lean_args() -> Vec<String> {
    let mut args: Vec<String> = ["--disable-builtin-mcps", "--no-auto-update"].map(str::to_string).to_vec();
    for rule in LEAN_DENY_RULES {
        args.push("--deny-tool".to_string());
        args.push(rule.to_string());
    }
    args
}

/// `COPILOT_HOME` pointed at a freshly prepared lean home, or nothing (and
/// a warning) when the user's own home cannot be found or copied, in
/// which case the CLI boots with every user-level MCP server.
fn lean_home_env(agent_home: &std::path::Path) -> Vec<(String, String)> {
    match user_copilot_home()
        .ok_or_else(|| anyhow::anyhow!("could not determine the user's Copilot home"))
        .and_then(|source| prepare_lean_home(&source, agent_home))
    {
        Ok(()) => vec![("COPILOT_HOME".to_string(), agent_home.to_string_lossy().to_string())],
        Err(err) => {
            tracing::warn!(
                "could not prepare a lean COPILOT_HOME at {}: {err:#}; launching with the user's own \
                 home (every user-level MCP server will load)",
                agent_home.display()
            );
            Vec::new()
        }
    }
}

/// The user's real Copilot home: `$COPILOT_HOME` if set, else `~/.copilot`
/// (Copilot CLI's own default).
fn user_copilot_home() -> Option<std::path::PathBuf> {
    if let Some(home) = std::env::var_os("COPILOT_HOME").filter(|v| !v.is_empty()) {
        return Some(std::path::PathBuf::from(home));
    }
    dirs::home_dir().map(|h| h.join(".copilot"))
}

/// Materializes `agent_home` as a minimal Copilot config home: the
/// `LEAN_HOME_FILES` copied from `source` (the user's real home), plus an
/// empty `mcp-config.json`. Fails (so the caller falls back to the user's
/// home) when `source` has no `config.json`, since a home without it
/// cannot authenticate.
fn prepare_lean_home(source: &Path, agent_home: &Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let source_config = source.join("config.json");
    if !source_config.is_file() {
        anyhow::bail!(
            "{} does not exist; Copilot CLI's login pointer lives there and a home without it \
             cannot authenticate",
            source_config.display()
        );
    }
    std::fs::create_dir_all(agent_home).with_context(|| format!("creating {}", agent_home.display()))?;
    for name in LEAN_HOME_FILES {
        let from = source.join(name);
        if from.is_file() {
            std::fs::copy(&from, agent_home.join(name))
                .with_context(|| format!("copying {} into {}", from.display(), agent_home.display()))?;
        }
    }
    std::fs::write(agent_home.join("mcp-config.json"), "{\"mcpServers\":{}}\n")
        .with_context(|| format!("writing mcp-config.json into {}", agent_home.display()))?;
    Ok(())
}

/// Schema modeled against real captured output -- see DESIGN.md
/// ("pact-agents > Copilot CLI output schema").
fn parse_line(line: &str) -> Vec<AgentEvent> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![AgentEvent::Other(Value::String(line.to_string()))],
    };

    match value.get("type").and_then(Value::as_str) {
        Some("session.mcp_server_status_changed") => {
            let data = value.get("data");
            match (
                data.and_then(|d| d.get("serverName")).and_then(Value::as_str),
                data.and_then(|d| d.get("status")).and_then(Value::as_str),
            ) {
                (Some(name), Some(status)) => vec![AgentEvent::CoordStatus {
                    name: name.to_string(),
                    status: status.to_string(),
                }],
                _ => vec![AgentEvent::Other(value)],
            }
        }
        Some("session.mcp_servers_loaded") => value
            .get("data")
            .and_then(|d| d.get("servers"))
            .and_then(Value::as_array)
            .map(|servers| {
                servers
                    .iter()
                    .filter_map(|s| {
                        let name = s.get("name")?.as_str()?.to_string();
                        let status = s.get("status")?.as_str()?.to_string();
                        Some(AgentEvent::CoordStatus { name, status })
                    })
                    .collect()
            })
            .unwrap_or_else(|| vec![AgentEvent::Other(value.clone())]),
        Some("assistant.message") => parse_assistant_message(&value),
        Some("result") => {
            let exit_code = value.get("exitCode").and_then(Value::as_i64).unwrap_or(-1);
            vec![AgentEvent::Result {
                success: exit_code == 0,
                summary: format!("exit code {exit_code}"),
            }]
        }
        _ => vec![AgentEvent::Other(value)],
    }
}

/// Copilot CLI can bundle response text *and* tool calls into a single
/// `assistant.message` event -- see DESIGN.md ("pact-agents > Copilot CLI
/// output schema").
fn parse_assistant_message(value: &Value) -> Vec<AgentEvent> {
    let data = match value.get("data") {
        Some(d) => d,
        None => return vec![AgentEvent::Other(value.clone())],
    };

    let mut events = Vec::new();

    if let Some(text) = data.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            events.push(AgentEvent::AssistantText(text.to_string()));
        }
    }

    if let Some(requests) = data.get("toolRequests").and_then(Value::as_array) {
        for request in requests {
            let name = request
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown_tool")
                .to_string();
            let input = request.get("arguments").cloned().unwrap_or(Value::Null);
            events.push(AgentEvent::ToolUse { name, input });
        }
    }

    if events.is_empty() {
        events.push(AgentEvent::Other(value.clone()));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pact-agents-copilot-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn request<'a>(agent_home: &'a Path, lean: bool) -> LaunchRequest<'a> {
        LaunchRequest {
            task: "do the thing",
            safety_override: None,
            coord: None,
            workspace_path: Path::new("/tmp/workspace"),
            agent_home,
            session_id: "11111111-2222-3333-4444-555555555555",
            lean,
        }
    }

    #[test]
    fn non_lean_launch_is_exactly_build_command_with_no_env() {
        let home = scratch("non-lean");
        let launch = CopilotAdapter.build_launch(&request(&home, false));
        let (program, args) = CopilotAdapter.build_command("do the thing", None, None, Path::new("/tmp/workspace"));
        assert_eq!(launch.program, program);
        assert_eq!(launch.args, args);
        assert!(launch.env.is_empty());
        assert!(!home.join("mcp-config.json").exists(), "a non-lean launch must not touch the agent home");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn acp_launch_is_one_server_process_with_the_same_safety_and_lean_posture() {
        let home = scratch("acp");
        let lean = CopilotAdapter
            .build_acp_launch(&AcpLaunchRequest { agent_home: &home, lean: true })
            .expect("Copilot has an ACP mode");
        assert_eq!(lean.program, "copilot");
        assert_eq!(lean.args[0], "--acp", "ACP server mode first: {:?}", lean.args);
        for flag in ["--allow-all-tools", "--disable-builtin-mcps", "--no-auto-update"] {
            assert!(lean.args.iter().any(|a| a == flag), "missing {flag} in {:?}", lean.args);
        }
        for absent in ["-p", "--output-format", "--session-id", "--additional-mcp-config"] {
            assert!(
                !lean.args.iter().any(|a| a == absent),
                "{absent} is per session or per stream, not per process: {:?}",
                lean.args
            );
        }
        let denied = lean.args.windows(2).filter(|w| w[0] == "--deny-tool").count();
        assert_eq!(denied, LEAN_DENY_RULES.len(), "the ACP process carries every deny rule");
        for (key, value) in &lean.env {
            assert_eq!(key, "COPILOT_HOME");
            assert_eq!(std::path::Path::new(value), home);
        }

        let plain = CopilotAdapter.build_acp_launch(&AcpLaunchRequest { agent_home: &home, lean: false }).unwrap();
        assert_eq!(plain.args, vec!["--acp", "--allow-all-tools"]);
        assert!(plain.env.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn lean_launch_adds_isolation_flags_session_id_and_every_deny_rule() {
        let home = scratch("lean-args");
        let launch = CopilotAdapter.build_launch(&request(&home, true));
        assert_eq!(launch.program, "copilot");
        for flag in ["--allow-all-tools", "--disable-builtin-mcps", "--no-auto-update"] {
            assert!(launch.args.iter().any(|a| a == flag), "missing {flag} in {:?}", launch.args);
        }
        let sid = launch.args.iter().position(|a| a == "--session-id").expect("--session-id");
        assert_eq!(launch.args[sid + 1], "11111111-2222-3333-4444-555555555555");
        let denied: Vec<&String> = launch
            .args
            .windows(2)
            .filter(|w| w[0] == "--deny-tool")
            .map(|w| &w[1])
            .collect();
        assert_eq!(denied.len(), LEAN_DENY_RULES.len());
        for rule in LEAN_DENY_RULES {
            assert!(denied.iter().any(|d| d.as_str() == *rule), "missing deny rule {rule}");
        }
        // Whether COPILOT_HOME is set depends on the machine running the
        // tests having a real Copilot home to copy config.json from; when
        // it is set, it must point at the per-agent home.
        for (key, value) in &launch.env {
            assert_eq!(key, "COPILOT_HOME");
            assert_eq!(std::path::Path::new(value), home);
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn prepare_lean_home_copies_login_pointer_and_settings_but_not_mcp_servers() {
        let source = scratch("source-home");
        std::fs::write(source.join("config.json"), "{\"loggedInUsers\":[{\"host\":\"https://github.com\",\"login\":\"me\"}]}").unwrap();
        std::fs::write(source.join("settings.json"), "{\"model\":\"claude-opus-5\"}").unwrap();
        std::fs::write(source.join("mcp-config.json"), "{\"mcpServers\":{\"chrome\":{\"command\":\"npx\"}}}").unwrap();
        std::fs::write(source.join("permissions-config.json"), "{\"locations\":{}}").unwrap();
        std::fs::write(source.join("copilot-instructions.md"), "always open a PR").unwrap();
        let home = scratch("agent-home");

        prepare_lean_home(&source, &home).unwrap();

        assert_eq!(std::fs::read_to_string(home.join("config.json")).unwrap(), std::fs::read_to_string(source.join("config.json")).unwrap());
        assert_eq!(std::fs::read_to_string(home.join("settings.json")).unwrap(), "{\"model\":\"claude-opus-5\"}");
        let mcp: Value = serde_json::from_str(&std::fs::read_to_string(home.join("mcp-config.json")).unwrap()).unwrap();
        assert_eq!(mcp, serde_json::json!({"mcpServers": {}}), "the user's MCP servers must not carry over");
        assert!(!home.join("permissions-config.json").exists());
        assert!(!home.join("copilot-instructions.md").exists());
        let _ = std::fs::remove_dir_all(&source);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn prepare_lean_home_refuses_without_a_login_pointer_to_copy() {
        let source = scratch("source-no-config");
        let home = scratch("agent-home-no-config");
        let err = prepare_lean_home(&source, &home).unwrap_err();
        assert!(err.to_string().contains("config.json"), "got: {err:#}");
        assert!(!home.join("mcp-config.json").exists(), "nothing should be written on failure");
        let _ = std::fs::remove_dir_all(&source);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn lean_deny_rules_cover_every_javascript_installer_and_full_builds() {
        for needle in ["npm install", "pnpm add", "yarn add", "bun install", "next build", "npm run build", "next dev"] {
            assert!(LEAN_DENY_RULES.iter().any(|r| r.contains(needle)), "no deny rule mentions {needle}");
        }
    }
}
