//! Phase 3 (not yet implemented): cross-agent coordination.
//!
//! Advisory, glob-based, TTL-expiring file leases plus a threaded message
//! log between agent sessions, backed by an embedded SQLite store at
//! `.agentyard-<repo>/state.db`. Exposed as an MCP server so agent sessions
//! call `claim_files`, `release_files`, `send_message`, and `check_messages`
//! as native tool calls, the same way Claude Code / Codex / Copilot CLI
//! already integrate with any other MCP server.
