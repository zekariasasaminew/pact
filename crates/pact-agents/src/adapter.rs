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

/// A `--safety` value users can give without knowing any one adapter's own
/// vocabulary -- see DESIGN.md ("pact-agents > safety profiles", issue
/// #161). Aliases layered *on top of* the existing raw pass-through, never
/// a replacement: any other string (`acceptEdits`, `read-only`, ...) keeps
/// flowing through to `build_command` completely unchanged, exactly as
/// before this existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyProfile {
    /// Each adapter's most restrictive real mode.
    Strict,
    /// pact's own existing default for that adapter -- deliberately *not*
    /// a distinct, more-permissive-than-default setting: for Codex and
    /// Gemini, pact's existing default is already the only mode confirmed
    /// to complete real headless work at all (see each adapter's own
    /// DESIGN.md section), so "workspace-write" and "unrestricted" both
    /// resolve to that same default for those two -- there is no safer
    /// mode that still gets real work done to alias instead.
    WorkspaceWrite,
    /// Each adapter's full-bypass mode.
    Unrestricted,
}

impl SafetyProfile {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "strict" => Some(Self::Strict),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "unrestricted" => Some(Self::Unrestricted),
            _ => None,
        }
    }
}

/// Resolves a raw `--safety` value to what `build_command` should actually
/// receive: if it's one of the three profile names, mapped per-adapter
/// below; otherwise returned unchanged, so any adapter-specific raw value
/// keeps working exactly as it did before profiles existed.
pub fn resolve_safety_profile(agent: AgentKind, safety: Option<&str>) -> Option<String> {
    let name = safety?;
    let Some(profile) = SafetyProfile::parse(name) else {
        if let Some(warning) = raw_safety_value_warning(agent, name) {
            tracing::warn!("{warning}");
        }
        return Some(name.to_string());
    };
    match (agent, profile) {
        (AgentKind::Claude, SafetyProfile::Strict) => Some("plan".to_string()),
        (AgentKind::Claude, SafetyProfile::WorkspaceWrite) => None,
        (AgentKind::Claude, SafetyProfile::Unrestricted) => Some("bypassPermissions".to_string()),

        // Codex: confirmed by hand (see DESIGN.md "Codex adapter"), a
        // plain `--sandbox` mode -- even `workspace-write` -- still
        // refuses to write files at all in headless mode; the only
        // confirmed-working "gets real work done" setting is the full
        // bypass flag (pact's existing `None` default). `workspace-write`
        // here is a deliberately inspect-only/no-real-edits run, faithful
        // to Codex's own current limitation, not a pact bug.
        (AgentKind::Codex, SafetyProfile::Strict) => Some("read-only".to_string()),
        (AgentKind::Codex, SafetyProfile::WorkspaceWrite) => Some("workspace-write".to_string()),
        (AgentKind::Codex, SafetyProfile::Unrestricted) => None,

        (AgentKind::Gemini, SafetyProfile::Strict) => Some("plan".to_string()),
        (AgentKind::Gemini, SafetyProfile::WorkspaceWrite) => Some("auto_edit".to_string()),
        (AgentKind::Gemini, SafetyProfile::Unrestricted) => None,

        // Copilot CLI has no gradient at all -- `build_command` ignores
        // `safety_override` unconditionally and always passes
        // `--allow-all-tools` (see `copilot.rs`). All three profiles
        // resolve to the same no-op here, faithfully: there is no
        // distinct restricted mode this adapter's CLI actually offers.
        (AgentKind::Copilot, _) => None,
    }
}

/// Copilot CLI's own vocabulary has no safety gradient at all -- `--safety`
/// is a guaranteed no-op there regardless of what raw string is passed, so
/// a raw value that doesn't even match one of pact's own portable profile
/// names (`strict`/`workspace-write`/`unrestricted`) is worth flagging: a
/// deliberate profile name is self-explanatory and already documented as a
/// no-op for this adapter, but an unrecognized raw value most likely means
/// the caller assumed Copilot has a real vocabulary the way Claude
/// Code/Codex/Gemini do (issue #205, outside R5 report). Deliberately
/// scoped to Copilot only -- the other three adapters' own raw vocabularies
/// are real and evolving, and pact doesn't want to hard-code them here just
/// to warn on a typo, risking false positives against a legitimate new
/// value.
fn raw_safety_value_warning(agent: AgentKind, name: &str) -> Option<String> {
    if agent != AgentKind::Copilot || SafetyProfile::parse(name).is_some() {
        return None;
    }
    Some(format!(
        "--safety \"{name}\" was passed through raw for Copilot, which has no safety gradient \
         and ignores it entirely -- if this was meant to be one of pact's own portable profile \
         names (strict/workspace-write/unrestricted), it may be a typo"
    ))
}

/// Writes `{"mcpServers": {<name>: {"command": ..., "args": [...]}}}` to
/// `path`, the shape Claude Code's `--mcp-config` and Copilot CLI's
/// `--additional-mcp-config @<path>` both expect -- see DESIGN.md
/// ("pact-agents > MCP config format confirmation"). Codex doesn't use
/// this at all -- it takes inline config overrides instead of a file.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_safety_profile_maps_claude_strict_and_unrestricted() {
        assert_eq!(resolve_safety_profile(AgentKind::Claude, Some("strict")), Some("plan".to_string()));
        assert_eq!(
            resolve_safety_profile(AgentKind::Claude, Some("unrestricted")),
            Some("bypassPermissions".to_string())
        );
    }

    #[test]
    fn resolve_safety_profile_workspace_write_is_claudes_own_default() {
        assert_eq!(resolve_safety_profile(AgentKind::Claude, Some("workspace-write")), None);
    }

    #[test]
    fn resolve_safety_profile_maps_codex_profiles() {
        assert_eq!(resolve_safety_profile(AgentKind::Codex, Some("strict")), Some("read-only".to_string()));
        assert_eq!(
            resolve_safety_profile(AgentKind::Codex, Some("workspace-write")),
            Some("workspace-write".to_string())
        );
        assert_eq!(resolve_safety_profile(AgentKind::Codex, Some("unrestricted")), None);
    }

    #[test]
    fn resolve_safety_profile_maps_gemini_profiles() {
        assert_eq!(resolve_safety_profile(AgentKind::Gemini, Some("strict")), Some("plan".to_string()));
        assert_eq!(
            resolve_safety_profile(AgentKind::Gemini, Some("workspace-write")),
            Some("auto_edit".to_string())
        );
        assert_eq!(resolve_safety_profile(AgentKind::Gemini, Some("unrestricted")), None);
    }

    #[test]
    fn resolve_safety_profile_is_a_no_op_for_copilot_regardless_of_profile() {
        for profile in ["strict", "workspace-write", "unrestricted"] {
            assert_eq!(resolve_safety_profile(AgentKind::Copilot, Some(profile)), None);
        }
    }

    #[test]
    fn resolve_safety_profile_passes_through_a_raw_non_profile_value_unchanged() {
        assert_eq!(
            resolve_safety_profile(AgentKind::Claude, Some("acceptEdits")),
            Some("acceptEdits".to_string())
        );
        assert_eq!(
            resolve_safety_profile(AgentKind::Codex, Some("danger-full-access")),
            Some("danger-full-access".to_string())
        );
    }

    #[test]
    fn resolve_safety_profile_is_none_when_no_override_given() {
        assert_eq!(resolve_safety_profile(AgentKind::Claude, None), None);
    }

    #[test]
    fn raw_safety_value_warning_fires_for_copilot_on_an_unrecognized_raw_value() {
        let warning = raw_safety_value_warning(AgentKind::Copilot, "accpetEdits").unwrap();
        assert!(warning.contains("accpetEdits"), "got: {warning}");
        assert!(warning.contains("no safety gradient"), "got: {warning}");
    }

    #[test]
    fn raw_safety_value_warning_is_none_for_a_recognized_profile_name() {
        // A deliberate portable profile name is self-explanatory and
        // already documented as a no-op for Copilot -- only genuinely
        // unrecognized raw values should warn, not every no-op.
        assert!(SafetyProfile::parse("strict").is_some());
        assert_eq!(raw_safety_value_warning(AgentKind::Copilot, "strict"), None);
    }

    #[test]
    fn raw_safety_value_warning_is_none_for_non_copilot_agents() {
        // The other adapters' raw vocabularies are real and evolving --
        // deliberately not hard-coded here, so no warning fires for them.
        for agent in [AgentKind::Claude, AgentKind::Codex, AgentKind::Gemini] {
            assert_eq!(raw_safety_value_warning(agent, "totally-bogus-value"), None);
        }
    }
}
