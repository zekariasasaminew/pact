//! Phase 2-4 (not yet implemented): agent adapters.
//!
//! One `AgentAdapter` implementation per agent CLI (Claude Code first, then
//! Codex and GitHub Copilot CLI). Each adapter builds the headless launch
//! command for its CLI, injects the MCP config pointing at the local
//! agentyard-coord server, and normalizes streamed output into a common
//! event type for the orchestrator to display.
