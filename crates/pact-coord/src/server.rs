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
        description = "Claim an advisory lease on file glob patterns you're about to edit, so other agents can see you're working on them. Returns any conflicting claims held by other agents -- this does not block you, it's informational, act on it yourself (e.g. message the other agent or avoid the overlap)."
    )]
    fn claim_files(
        &self,
        Parameters(params): Parameters<ClaimFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.conn.lock().unwrap();
        let body = match leases::claim_files(
            &conn,
            &self.workspace_root,
            &self.agent_id,
            &params.globs,
            params.ttl_seconds,
        ) {
            Ok(result) => serde_json::to_string_pretty(&result)
                .unwrap_or_else(|e| format!("error serializing result: {e}")),
            Err(e) => format!("error: {e:#}"),
        };
        text_result(body)
    }

    #[tool(
        description = "Release file glob patterns you previously claimed with claim_files. Matches either the exact pattern string you originally claimed, or any pattern here that overlaps the same actual files -- so releasing a broader or differently-worded glob than the original claim still works."
    )]
    fn release_files(
        &self,
        Parameters(params): Parameters<ReleaseFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.conn.lock().unwrap();
        let body = match leases::release_files(&conn, &self.workspace_root, &self.agent_id, &params.globs) {
            Ok(n) => format!("released {n} lease(s)"),
            Err(e) => format!("error: {e:#}"),
        };
        text_result(body)
    }

    #[tool(
        description = "Send a message to another agent by its workspace id, or broadcast to all agents by omitting `to`. Use this to tell other agents about changes that affect them -- e.g. a changed function signature they depend on."
    )]
    fn send_message(
        &self,
        Parameters(params): Parameters<SendMessageParams>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.conn.lock().unwrap();
        let body = match messages::send_message(
            &conn,
            &self.agent_id,
            params.to.as_deref(),
            &params.subject,
            &params.body,
        ) {
            Ok(id) => format!("sent message {id}"),
            Err(e) => format!("error: {e:#}"),
        };
        text_result(body)
    }

    #[tool(
        description = "Check for messages sent to you directly or broadcast to all agents, since you last checked."
    )]
    fn check_messages(&self) -> Result<CallToolResult, McpError> {
        let conn = self.conn.lock().unwrap();
        let body = match messages::check_messages(&conn, &self.agent_id) {
            Ok(msgs) => serde_json::to_string_pretty(&msgs)
                .unwrap_or_else(|e| format!("error serializing result: {e}")),
            Err(e) => format!("error: {e:#}"),
        };
        text_result(body)
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
