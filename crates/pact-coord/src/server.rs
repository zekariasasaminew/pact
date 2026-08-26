use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{leases, messages, operations};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClaimFilesParams {
    /// Glob patterns (e.g. "src/**/*.rs") you're about to edit.
    pub globs: Vec<String>,
    /// How long the claim lasts, in seconds. Defaults to 15 minutes if
    /// omitted. Must be positive and at most 86400 (24 hours).
    pub ttl_seconds: Option<i64>,
    /// Opt-in strict mode: reject the claim outright (isError: true) if it
    /// overlaps another agent's active lease, instead of the default
    /// advisory behavior (accepted: true, with has_conflicts/conflicts
    /// for you to judge). A rejected claim is never recorded -- nothing
    /// changes for anyone if you get this back. Defaults to false.
    pub fail_on_conflict: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReleaseFilesParams {
    /// Glob patterns previously passed to claim_files.
    pub globs: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMessageParams {
    /// Recipient's agent id (workspace id). Omit to broadcast to all agents.
    pub to: Option<String>,
    pub subject: String,
    pub body: String,
}

#[derive(Clone)]
pub struct CoordServer {
    conn: Arc<Mutex<Connection>>,
    agent_id: String,
    workspace_root: PathBuf,
    tool_router: ToolRouter<Self>,
}

fn text_result(body: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(body)]))
}

/// `isError: true` on the MCP result, so a calling model can tell a real
/// tool-level failure (a malformed glob, a store/DB error) apart from a
/// normal successful response by the standard MCP convention, instead of
/// having to string-match a body that happens to start with "error:".
/// Every handler below previously funneled its `Err` path through
/// `text_result` -- a real failure, but `isError: false` -- same as a
/// success.
fn error_result(body: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![Content::text(body)]))
}

/// Best-effort operation-log write -- a logging failure must never break
/// the tool call it's attached to, so this only warns, matching the
/// `set_agent_pid` precedent in `pact-core` for "record this, but don't
/// let recording it become a new way to fail."
fn log_op(conn: &Connection, op_type: &str, agent_id: &str, detail: serde_json::Value) {
    if let Err(err) = operations::log_operation(conn, op_type, Some(agent_id), &detail) {
        tracing::warn!("failed to record {op_type} operation for {agent_id}: {err:#}");
    }
}

#[tool_router]
impl CoordServer {
    pub fn new(conn: Connection, agent_id: String, workspace_root: PathBuf) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            agent_id,
            workspace_root,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Claim an advisory lease on file glob patterns you're about to edit, so other agents can see you're working on them. By default this is never enforced against other agents -- the claim is recorded (accepted: true) even when another agent already holds an overlapping one; check has_conflicts/conflicts in the response yourself and decide what to do (e.g. message the other agent or avoid the overlap). Pass fail_on_conflict: true to instead reject an overlapping claim outright (isError: true, nothing recorded) rather than accepting it advisorily."
    )]
    fn claim_files(
        &self,
        Parameters(params): Parameters<ClaimFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.conn.lock().unwrap();
        match leases::claim_files(
            &conn,
            &self.workspace_root,
            &self.agent_id,
            &params.globs,
            params.ttl_seconds,
            params.fail_on_conflict.unwrap_or(false),
        ) {
            Ok(result) => {
                log_op(
                    &conn,
                    "claim",
                    &self.agent_id,
                    serde_json::json!({
                        "patterns": params.globs,
                        "has_conflicts": result.has_conflicts,
                    }),
                );
                text_result(
                    serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|e| format!("error serializing result: {e}")),
                )
            }
            Err(e) => error_result(format!("error: {e:#}")),
        }
    }

    #[tool(
        description = "Release file glob patterns you previously claimed with claim_files. Matches either the exact pattern string you originally claimed, or any pattern here that overlaps the same actual files -- so releasing a broader or differently-worded glob than the original claim still works."
    )]
    fn release_files(
        &self,
        Parameters(params): Parameters<ReleaseFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.conn.lock().unwrap();
        match leases::release_files(&conn, &self.workspace_root, &self.agent_id, &params.globs) {
            Ok(n) => {
                log_op(
                    &conn,
                    "release",
                    &self.agent_id,
                    serde_json::json!({ "patterns": params.globs, "released": n }),
                );
                text_result(format!("released {n} lease(s)"))
            }
            Err(e) => error_result(format!("error: {e:#}")),
        }
    }

    #[tool(
        description = "Send a message to another agent by its workspace id, or broadcast to all agents by omitting `to`. Use this to tell other agents about changes that affect them -- e.g. a changed function signature they depend on."
    )]
    fn send_message(
        &self,
        Parameters(params): Parameters<SendMessageParams>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.conn.lock().unwrap();
        match messages::send_message(
            &conn,
            &self.agent_id,
            params.to.as_deref(),
            &params.subject,
            &params.body,
        ) {
            Ok(id) => {
                let op_type = if params.to.is_some() { "message" } else { "broadcast" };
                log_op(
                    &conn,
                    op_type,
                    &self.agent_id,
                    serde_json::json!({ "to": params.to, "subject": params.subject }),
                );
                text_result(format!("sent message {id}"))
            }
            Err(e) => error_result(format!("error: {e:#}")),
        }
    }

    #[tool(
        description = "Check for messages sent to you directly or broadcast to all agents, since you last checked."
    )]
    fn check_messages(&self) -> Result<CallToolResult, McpError> {
        let conn = self.conn.lock().unwrap();
        match messages::check_messages(&conn, &self.agent_id) {
            Ok(msgs) => text_result(
                serde_json::to_string_pretty(&msgs).unwrap_or_else(|e| format!("error serializing result: {e}")),
            ),
            Err(e) => error_result(format!("error: {e:#}")),
        }
    }

    #[tool(
        description = "List every currently-unexpired file lease in this coordination session -- pattern, holder, and expiry -- across all agents, not just your own. Use this to check what's claimed before deciding whether to claim something yourself, without guessing from claim_files' conflict responses alone. Read-only: unlike check_messages, calling this never marks anything as read or changes any state."
    )]
    fn list_claims(&self) -> Result<CallToolResult, McpError> {
        let conn = self.conn.lock().unwrap();
        match leases::list_active_leases(&conn) {
            Ok(active) => text_result(
                serde_json::to_string_pretty(&active).unwrap_or_else(|e| format!("error serializing result: {e}")),
            ),
            Err(e) => error_result(format!("error: {e:#}")),
        }
    }
}

#[tool_handler]
impl ServerHandler for CoordServer {
    /// Called by rmcp while answering the client's `initialize` request --
    /// the one point in this protocol every real MCP client reaches
    /// before any tool call, regardless of whether it ever calls one.
    /// Logging a `coord_connect` operation here (issue #235) is what lets
    /// `pact coord-status` tell "no leases because nobody's claimed
    /// anything yet" apart from "no leases because this agent's MCP
    /// client never connected at all" -- previously indistinguishable, and
    /// a real production run hit exactly the second case silently.
    fn get_info(&self) -> ServerInfo {
        let conn = self.conn.lock().unwrap();
        log_op(&conn, "coord_connect", &self.agent_id, serde_json::json!({}));
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

pub async fn serve(conn: Connection, agent_id: String, workspace_root: PathBuf) -> anyhow::Result<()> {
    let server = CoordServer::new(conn, agent_id, workspace_root);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE leases (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pattern TEXT NOT NULL,
                holder TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
            );
            CREATE UNIQUE INDEX leases_holder_pattern ON leases(holder, pattern);
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_agent TEXT NOT NULL,
                to_agent TEXT,
                subject TEXT NOT NULL,
                body TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE read_cursors (
                agent_id TEXT PRIMARY KEY,
                last_seen_message_id INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE operations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at INTEGER NOT NULL,
                op_type TEXT NOT NULL,
                workspace_id TEXT,
                detail TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn get_info_logs_a_coord_connect_operation_for_this_agent() {
        let server = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        let _ = ServerHandler::get_info(&server);

        let conn = server.conn.lock().unwrap();
        let connected = crate::operations::connected_workspace_ids(&conn).unwrap();
        assert!(connected.contains("agent-a"), "expected get_info to log a coord_connect for agent-a, got: {connected:?}");
    }

    #[test]
    fn claim_files_sets_is_error_on_malformed_glob() {
        let server = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        let result = server
            .claim_files(Parameters(ClaimFilesParams { globs: vec!["[".to_string()], ttl_seconds: None, fail_on_conflict: None }))
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn claim_files_leaves_is_error_false_on_success() {
        let server = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        let result = server
            .claim_files(Parameters(ClaimFilesParams { globs: vec!["some.txt".to_string()], ttl_seconds: None, fail_on_conflict: None }))
            .unwrap();
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn claim_files_with_fail_on_conflict_still_succeeds_when_nothing_conflicts() {
        // The actual rejection behavior is covered directly in
        // leases.rs's own tests (real overlapping claims, two holders
        // sharing one connection) -- this only confirms fail_on_conflict
        // threads through the MCP layer without breaking the ordinary
        // non-conflicting path.
        let server = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        let result = server
            .claim_files(Parameters(ClaimFilesParams {
                globs: vec!["some.txt".to_string()],
                ttl_seconds: None,
                fail_on_conflict: Some(true),
            }))
            .unwrap();
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn claim_files_sets_is_error_on_invalid_ttl() {
        let server = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        let result = server
            .claim_files(Parameters(ClaimFilesParams { globs: vec!["some.txt".to_string()], ttl_seconds: Some(-1), fail_on_conflict: None }))
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn release_files_sets_is_error_on_malformed_glob() {
        let server = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        let result = server
            .release_files(Parameters(ReleaseFilesParams { globs: vec!["[".to_string()] }))
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn list_claims_is_empty_with_no_active_leases() {
        let server = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        let result = server.list_claims().unwrap();
        assert_eq!(result.is_error, Some(false));
        assert_eq!(serde_json::from_str::<Vec<leases::ActiveLease>>(result_text(&result)).unwrap(), vec![]);
    }

    #[test]
    fn list_claims_shows_an_active_lease_regardless_of_who_holds_it() {
        // Unlike check_messages, list_claims has no self/other distinction
        // -- it's a full snapshot of coordination state, so one agent's
        // own claim showing up in its own list_claims call is correct,
        // not a self-message leaking through.
        let server = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        server
            .claim_files(Parameters(ClaimFilesParams { globs: vec!["some.txt".to_string()], ttl_seconds: None, fail_on_conflict: None }))
            .unwrap();

        let result = server.list_claims().unwrap();
        assert_eq!(result.is_error, Some(false));
        let active: Vec<leases::ActiveLease> = serde_json::from_str(result_text(&result)).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].holder, "agent-a");
        assert_eq!(active[0].pattern, "some.txt");
    }

    fn result_text(result: &CallToolResult) -> &str {
        match result.content.first() {
            Some(content) => content.as_text().map(|t| t.text.as_str()).unwrap_or_default(),
            None => "",
        }
    }
}
