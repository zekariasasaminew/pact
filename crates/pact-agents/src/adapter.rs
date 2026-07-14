use std::path::Path;

use anyhow::{Context, Result};

use crate::event::AgentEvent;

/// What to tell an agent CLI about the coordination server -- adapter
/// agnostic. Each adapter decides *how* to hand this to its CLI: Claude
/// Code and Copilot CLI both want a JSON file passed via a flag (see
/// `write_mcp_json_config`); Codex takes inline `-c mcp_servers.<id>.*`
/// overrides instead and needs no file at all.
pub struct CoordConfig {
    pub server_name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Where to write a config file, for adapters that need one (Claude
    /// Code, Copilot CLI). Set by the orchestrator, which already owns the
    /// per-workspace state directory -- adapters that don't need a file
    /// (Codex, which takes inline overrides instead) simply ignore this.
    pub config_path: std::path::PathBuf,
}

/// One agent CLI's integration: how to launch it headlessly and how to
/// make sense of what it prints. `parse_line` returns a `Vec` rather than
/// a single event because not every CLI's schema is one-event-per-line
/// (Copilot CLI's isn't -- see `process::run_and_stream`'s doc comment).
pub trait AgentAdapter {
    /// Name to register the coordination server under in this adapter's
    /// MCP config -- also what `pact-core`'s connectivity check looks
    /// for among the `AgentEvent::CoordStatus` events this adapter emits.
    fn coord_server_name(&self) -> &'static str;

    /// Describes the unattended-safety setting this adapter falls back to
    /// when `safety_override` is `None`, so the caller can warn about it.
    /// Every adapter needs *some* such setting in headless mode -- there's
    /// no TTY to answer an interactive prompt with any of these CLIs, not
    /// just Claude Code -- so this is never "no warning needed", only
    /// "which words to put in the warning."
    fn default_safety_description(&self) -> &'static str;

    /// Builds the program name and args for a headless launch.
    /// `safety_override`, if given, is passed through *raw* to this
    /// adapter's own safety/approval vocabulary (Claude Code's
    /// `--permission-mode` values, Codex's `--ask-for-approval` values,
    /// etc.) -- these vocabularies don't share a common enum, so no
    /// attempt is made to unify them into one. `workspace_path` exists for
    /// the rare adapter (Gemini CLI) whose MCP config isn't handed over
    /// via a flag at all, but read from a fixed path relative to its own
    /// working directory -- every other adapter ignores it.
    fn build_command(
        &self,
        task: &str,
        safety_override: Option<&str>,
        coord: Option<&CoordConfig>,
        workspace_path: &Path,
    ) -> (String, Vec<String>);

    /// Parses one raw output line into zero or more normalized events.
    fn parse_line(&self, line: &str) -> Vec<AgentEvent>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Claude,
    Copilot,
    Codex,
    /// Built from a real installed CLI but not live-verified against a
    /// real authenticated session -- see `gemini.rs`'s doc comment and
    /// issue #9.
    Gemini,
}

impl AgentKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            "copilot" => Some(Self::Copilot),
            "codex" => Some(Self::Codex),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }
}

pub fn adapter(kind: AgentKind) -> Box<dyn AgentAdapter> {
    match kind {
        AgentKind::Claude => Box::new(crate::claude_code::ClaudeCodeAdapter),
        AgentKind::Copilot => Box::new(crate::copilot::CopilotAdapter),
        AgentKind::Codex => Box::new(crate::codex::CodexAdapter),
        AgentKind::Gemini => Box::new(crate::gemini::GeminiAdapter),
    }
}

/// Writes `{"mcpServers": {<name>: {"command": ..., "args": [...]}}}` to
/// `path` -- the shape confirmed (by deliberately pointing both real CLIs
/// at a broken command and observing a loud, non-silent failure) to work
/// for both Claude Code's `--mcp-config` and Copilot CLI's
/// `--additional-mcp-config @<path>`. Codex doesn't use this at all -- it
/// takes inline config overrides instead of a file.
pub fn write_mcp_json_config(path: &Path, coord: &CoordConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let config = serde_json::json!({
        "mcpServers": {
            coord.server_name.clone(): {
                "command": coord.command,
                "args": coord.args,
            }
        }
    });
    std::fs::write(path, serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("writing MCP config file to {}", path.display()))
}
