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

use crate::{leases, messages};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClaimFilesParams {
    /// Glob patterns (e.g. "src/**/*.rs") you're about to edit.
    pub globs: Vec<String>,
    /// How long the claim lasts, in seconds. Defaults to 15 minutes if
    /// omitted. Must be positive and at most 86400 (24 hours).
    pub ttl_seconds: Option<i64>,
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
        description = "Claim an advisory lease on file glob patterns you're about to edit, so other agents can see you're working on them. This is never enforced against other agents -- the claim is recorded (accepted: true) even when another agent already holds an overlapping one; check has_conflicts/conflicts in the response yourself and decide what to do (e.g. message the other agent or avoid the overlap). Do not treat a successful response as exclusive access."
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
        ) {
            Ok(result) => text_result(
                serde_json::to_string_pretty(&result)
                    .unwrap_or_else(|e| format!("error serializing result: {e}")),
            ),
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
            Ok(n) => text_result(format!("released {n} lease(s)")),
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
            Ok(id) => text_result(format!("sent message {id}")),
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
}

#[tool_handler]
impl ServerHandler for CoordServer {
    fn get_info(&self) -> ServerInfo {
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
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn claim_files_sets_is_error_on_malformed_glob() {
        let server = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        let result = server
            .claim_files(Parameters(ClaimFilesParams { globs: vec!["[".to_string()], ttl_seconds: None }))
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn claim_files_leaves_is_error_false_on_success() {
        let server = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        let result = server
            .claim_files(Parameters(ClaimFilesParams { globs: vec!["some.txt".to_string()], ttl_seconds: None }))
            .unwrap();
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn claim_files_sets_is_error_on_invalid_ttl() {
        let server = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        let result = server
            .claim_files(Parameters(ClaimFilesParams { globs: vec!["some.txt".to_string()], ttl_seconds: Some(-1) }))
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
}
