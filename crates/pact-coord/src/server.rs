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

use crate::{handoffs, leases, messages, operations};
use crate::handoffs::HandoffDecision;

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
pub struct RequestHandoffParams {
    /// Recipient's agent id (workspace id).
    pub to: String,
    /// Glob patterns or file paths you're asking `to` to hand over, hold
    /// off on, or otherwise coordinate on -- the actual scope of the ask.
    pub files: Vec<String>,
    /// Why you're asking -- free text, shown to the recipient.
    pub message: String,
    /// How long before this request auto-expires if never responded to,
    /// in seconds. Defaults to 10 minutes if omitted. Must be positive
    /// and at most 86400 (24 hours).
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RespondHandoffParams {
    pub request_id: i64,
    /// "accept", "reject", or "narrow".
    pub decision: String,
    /// Required when decision is "narrow": the narrower file scope
    /// you're actually willing to hand over instead of the original ask.
    /// The requester sees this and can send a fresh request_handoff
    /// scoped to it if they want to accept the counter-offer.
    pub narrowed_files: Option<Vec<String>>,
    /// Optional free-text explanation, shown to the requester.
    pub message: Option<String>,
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

    #[tool(
        description = "Ask another agent to hand over, hold off on, or otherwise coordinate on a specific set of files -- a structured alternative to a prose send_message, with a real status you (and they) can poll: pending, accepted, rejected, narrowed (they're offering a smaller/different scope instead -- see the narrowed_files field on what check_handoffs returns), expired (they never responded in time), or cancelled. This does not block -- it returns immediately with a request_id and expires_at; check on it later with check_handoffs, the same way you'd poll check_messages. If narrowed, and you want to accept the counter-offer, send a fresh request_handoff scoped to the narrowed files."
    )]
    fn request_handoff(
        &self,
        Parameters(params): Parameters<RequestHandoffParams>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.conn.lock().unwrap();
        match handoffs::request_handoff(&conn, &self.agent_id, &params.to, &params.files, &params.message, params.ttl_seconds) {
            Ok(result) => {
                log_op(
                    &conn,
                    "handoff_request",
                    &self.agent_id,
                    serde_json::json!({ "to": params.to, "files": params.files, "request_id": result.request_id }),
                );
                text_result(
                    serde_json::to_string_pretty(&result).unwrap_or_else(|e| format!("error serializing result: {e}")),
                )
            }
            Err(e) => error_result(format!("error: {e:#}")),
        }
    }

    #[tool(
        description = "Check for handoff requests relevant to you since you last checked -- both new requests addressed to you (which you can act on with respond_handoff) and responses to requests you sent. A request you sent doesn't show up here while it's still pending (you already know you sent it); it appears once the other agent accepts, rejects, narrows, or it expires."
    )]
    fn check_handoffs(&self) -> Result<CallToolResult, McpError> {
        let conn = self.conn.lock().unwrap();
        match handoffs::check_handoffs(&conn, &self.agent_id) {
            Ok(requests) => text_result(
                serde_json::to_string_pretty(&requests).unwrap_or_else(|e| format!("error serializing result: {e}")),
            ),
            Err(e) => error_result(format!("error: {e:#}")),
        }
    }

    #[tool(
        description = "Respond to a pending handoff request addressed to you. decision must be exactly \"accept\", \"reject\", or \"narrow\" -- narrow requires narrowed_files (the smaller/different scope you're actually willing to hand over instead of the original ask). Only the request's own recipient can respond, and only while it's still pending -- an already-expired, already-responded, or already-cancelled request errors instead of silently being overwritten."
    )]
    fn respond_handoff(
        &self,
        Parameters(params): Parameters<RespondHandoffParams>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.conn.lock().unwrap();
        let decision = match params.decision.as_str() {
            "accept" => HandoffDecision::Accept,
            "reject" => HandoffDecision::Reject,
            "narrow" => HandoffDecision::Narrow(params.narrowed_files.clone().unwrap_or_default()),
            other => {
                return error_result(format!("error: decision must be \"accept\", \"reject\", or \"narrow\", got \"{other}\""));
            }
        };
        match handoffs::respond_handoff(&conn, params.request_id, &self.agent_id, decision, params.message.as_deref()) {
            Ok(()) => {
                log_op(
                    &conn,
                    "handoff_respond",
                    &self.agent_id,
                    serde_json::json!({ "request_id": params.request_id, "decision": params.decision }),
                );
                text_result(format!("responded to handoff request {}", params.request_id))
            }
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
            );
            CREATE TABLE handoff_requests (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_agent TEXT NOT NULL,
                to_agent TEXT NOT NULL,
                requested_files TEXT NOT NULL,
                message TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                responded_at INTEGER,
                response_message TEXT,
                narrowed_files TEXT,
                activity_seq INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE handoff_read_cursors (
                agent_id TEXT PRIMARY KEY,
                last_seen_handoff_id INTEGER NOT NULL DEFAULT 0
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

    #[test]
    fn request_handoff_sets_is_error_on_empty_files() {
        let server = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        let result = server
            .request_handoff(Parameters(RequestHandoffParams {
                to: "agent-b".to_string(),
                files: vec![],
                message: "please".to_string(),
                ttl_seconds: None,
            }))
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn request_handoff_succeeds_and_target_sees_it_via_check_handoffs() {
        let requester = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        let result = requester
            .request_handoff(Parameters(RequestHandoffParams {
                to: "agent-b".to_string(),
                files: vec!["src/*.rs".to_string()],
                message: "can I take these?".to_string(),
                ttl_seconds: None,
            }))
            .unwrap();
        assert_eq!(result.is_error, Some(false));

        let target = CoordServer { conn: requester.conn.clone(), agent_id: "agent-b".to_string(), workspace_root: std::env::temp_dir(), tool_router: CoordServer::tool_router() };
        let incoming = target.check_handoffs().unwrap();
        assert_eq!(incoming.is_error, Some(false));
        let requests: Vec<handoffs::HandoffRequest> = serde_json::from_str(result_text(&incoming)).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].from, "agent-a");
        assert_eq!(requests[0].message, "can I take these?");
    }

    #[test]
    fn respond_handoff_rejects_an_unknown_decision_string() {
        let requester = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        requester
            .request_handoff(Parameters(RequestHandoffParams { to: "agent-b".to_string(), files: vec!["x.txt".to_string()], message: "please".to_string(), ttl_seconds: None }))
            .unwrap();

        let target = CoordServer { conn: requester.conn.clone(), agent_id: "agent-b".to_string(), workspace_root: std::env::temp_dir(), tool_router: CoordServer::tool_router() };
        let result = target
            .respond_handoff(Parameters(RespondHandoffParams { request_id: 1, decision: "maybe".to_string(), narrowed_files: None, message: None }))
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn respond_handoff_accept_flows_through_to_the_requester() {
        let requester = CoordServer::new(test_conn(), "agent-a".to_string(), std::env::temp_dir());
        requester
            .request_handoff(Parameters(RequestHandoffParams { to: "agent-b".to_string(), files: vec!["x.txt".to_string()], message: "please".to_string(), ttl_seconds: None }))
            .unwrap();

        let target = CoordServer { conn: requester.conn.clone(), agent_id: "agent-b".to_string(), workspace_root: std::env::temp_dir(), tool_router: CoordServer::tool_router() };
        let response = target
            .respond_handoff(Parameters(RespondHandoffParams { request_id: 1, decision: "accept".to_string(), narrowed_files: None, message: Some("go ahead".to_string()) }))
            .unwrap();
        assert_eq!(response.is_error, Some(false));

        let outgoing = requester.check_handoffs().unwrap();
        let requests: Vec<handoffs::HandoffRequest> = serde_json::from_str(result_text(&outgoing)).unwrap();
        assert_eq!(requests.len(), 1);
        assert!(matches!(requests[0].status, handoffs::HandoffStatus::Accepted));
    }

    fn result_text(result: &CallToolResult) -> &str {
        match result.content.first() {
            Some(content) => content.as_text().map(|t| t.text.as_str()).unwrap_or_default(),
            None => "",
        }
    }
}
