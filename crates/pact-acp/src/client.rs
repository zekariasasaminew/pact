//! Async ACP client over a child process's stdio (issue #330).
//!
//! One `AcpClient` is one agent process. Requests from pact are
//! multiplexed by JSON-RPC id; the agent's own requests
//! (`session/request_permission`) are answered inline by a policy; its
//! `session/update` notifications are fanned out to whichever lane owns
//! the session. When the process exits, every pending request fails with
//! [`AcpError::RuntimeExited`] and every session's update stream ends,
//! so lane threads never wait on a dead agent.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

use crate::protocol::*;

/// How to start an agent as an ACP server.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AcpError {
    #[error("agent answered `{method}` with JSON-RPC error {code}: {message}")]
    Rpc { method: String, code: i64, message: String, data: Option<Box<Value>> },
    #[error("agent process exited ({status}) with work still pending{}", stderr_suffix(.stderr_tail))]
    RuntimeExited { status: String, stderr_tail: String },
    #[error("i/o with the agent process: {0}")]
    Io(String),
    #[error("protocol: {0}")]
    Protocol(String),
}

fn stderr_suffix(tail: &str) -> String {
    if tail.trim().is_empty() {
        String::new()
    } else {
        format!("; stderr tail:\n{tail}")
    }
}

impl AcpError {
    pub fn is_method_not_found(&self) -> bool {
        matches!(self, AcpError::Rpc { code: -32601, .. })
    }
}

pub type PermissionPolicy = Arc<dyn Fn(&PermissionRequest) -> PermissionDecision + Send + Sync>;

#[derive(Debug, Clone)]
pub struct ExitInfo {
    pub status: String,
    pub stderr_tail: String,
}

const STDERR_TAIL_LINES: usize = 20;

/// Requests awaiting an answer: id to (method, reply slot).
type Pending = HashMap<u64, (String, oneshot::Sender<Result<Value, AcpError>>)>;

struct Inner {
    pid: u32,
    writer: tokio::sync::Mutex<Option<ChildStdin>>,
    child: tokio::sync::Mutex<Option<Child>>,
    next_id: AtomicU64,
    pending: Mutex<Pending>,
    sessions: Mutex<HashMap<String, mpsc::UnboundedSender<SessionUpdate>>>,
    permission_policy: PermissionPolicy,
    exited: Mutex<Option<ExitInfo>>,
    stderr_tail: Mutex<VecDeque<String>>,
}

#[derive(Clone)]
pub struct AcpClient {
    inner: Arc<Inner>,
}

impl AcpClient {
    /// Starts the agent process and the reader tasks. Does not send
    /// `initialize`; call [`AcpClient::initialize`] next.
    pub async fn spawn(spec: AgentSpec, permission_policy: PermissionPolicy) -> Result<Self, AcpError> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args).envs(spec.env.iter().map(|(k, v)| (k, v)));
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = command.spawn().map_err(|err| AcpError::Io(format!("spawning `{}`: {err}", spec.program)))?;
        let pid = child.id().ok_or_else(|| AcpError::Io("agent process has no pid".into()))?;
        let stdin = child.stdin.take().ok_or_else(|| AcpError::Io("agent stdin not piped".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| AcpError::Io("agent stdout not piped".into()))?;
        let stderr = child.stderr.take().ok_or_else(|| AcpError::Io("agent stderr not piped".into()))?;

        let inner = Arc::new(Inner {
            pid,
            writer: tokio::sync::Mutex::new(Some(stdin)),
            child: tokio::sync::Mutex::new(Some(child)),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            permission_policy,
            exited: Mutex::new(None),
            stderr_tail: Mutex::new(VecDeque::new()),
        });

        let for_stderr = inner.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "pact_acp::agent_stderr", "{line}");
                let mut tail = for_stderr.stderr_tail.lock().unwrap();
                if tail.len() == STDERR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        });

        let for_reader = inner.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(&line) {
                            Ok(msg) => for_reader.dispatch(msg).await,
                            Err(err) => tracing::warn!("ignoring non-JSON line from agent: {err}: {}", truncate(&line, 200)),
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        tracing::warn!("reading from agent: {err}");
                        break;
                    }
                }
            }
            for_reader.on_exit().await;
        });

        Ok(AcpClient { inner })
    }

    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    /// `Some` once the agent process has gone away.
    pub fn exit_info(&self) -> Option<ExitInfo> {
        self.inner.exited.lock().unwrap().clone()
    }

    pub async fn initialize(&self) -> Result<InitializeResult, AcpError> {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_capabilities: ClientCapabilities::default(),
            client_info: ClientInfo { name: "pact".into(), version: env!("CARGO_PKG_VERSION").into() },
        };
        let result = self.request("initialize", serde_json::to_value(params).unwrap()).await?;
        let parsed: InitializeResult = serde_json::from_value(result).map_err(|err| AcpError::Protocol(format!("initialize result: {err}")))?;
        if parsed.protocol_version != PROTOCOL_VERSION {
            return Err(AcpError::Protocol(format!(
                "agent speaks ACP protocol version {}, pact speaks {PROTOCOL_VERSION}",
                parsed.protocol_version
            )));
        }
        Ok(parsed)
    }

    /// Creates a session and returns its update stream. The stream ends
    /// when the session is closed or the agent exits.
    pub async fn new_session(
        &self,
        cwd: &std::path::Path,
        mcp_servers: Vec<McpServer>,
    ) -> Result<(NewSessionResult, mpsc::UnboundedReceiver<SessionUpdate>), AcpError> {
        let params = NewSessionParams { cwd: cwd.to_string_lossy().to_string(), mcp_servers };
        let result = self.request("session/new", serde_json::to_value(params).unwrap()).await?;
        let parsed: NewSessionResult = serde_json::from_value(result).map_err(|err| AcpError::Protocol(format!("session/new result: {err}")))?;
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.sessions.lock().unwrap().insert(parsed.session_id.clone(), tx);
        Ok((parsed, rx))
    }

    /// Sends one text prompt and waits for the turn to end.
    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<StopReason, AcpError> {
        let params = PromptParams { session_id: session_id.to_string(), prompt: vec![ContentBlock::Text { text: text.to_string() }] };
        let result = self.request("session/prompt", serde_json::to_value(params).unwrap()).await?;
        let parsed: PromptResult = serde_json::from_value(result).map_err(|err| AcpError::Protocol(format!("session/prompt result: {err}")))?;
        Ok(parsed.stop_reason)
    }

    /// Asks the agent to stop the session's current turn. A notification:
    /// the in-flight `prompt` resolves with `StopReason::Cancelled`.
    pub async fn cancel(&self, session_id: &str) -> Result<(), AcpError> {
        let params = SessionIdParams { session_id: session_id.to_string() };
        self.notify("session/cancel", serde_json::to_value(params).unwrap()).await
    }

    /// Frees the session. Agents without the `close` capability answer
    /// -32601, which is treated as success: there is nothing to free.
    pub async fn close(&self, session_id: &str) -> Result<(), AcpError> {
        let params = SessionIdParams { session_id: session_id.to_string() };
        let outcome = self.request("session/close", serde_json::to_value(params).unwrap()).await;
        self.inner.sessions.lock().unwrap().remove(session_id);
        match outcome {
            Ok(_) => Ok(()),
            Err(err) if err.is_method_not_found() => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Closes the agent's stdin and waits briefly for it to exit, then
    /// kills it. Idempotent.
    pub async fn shutdown(&self) {
        self.inner.writer.lock().await.take();
        let child = self.inner.child.lock().await.take();
        if let Some(mut child) = child {
            match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
                Ok(Ok(status)) => self.inner.record_exit(format!("{status}")),
                _ => {
                    let _ = child.kill().await;
                    self.inner.record_exit("killed".into());
                }
            }
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        if let Some(exit) = self.exit_info() {
            return Err(AcpError::RuntimeExited { status: exit.status, stderr_tail: exit.stderr_tail });
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, (method.to_string(), tx));
        let message = serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(err) = self.inner.send(&message).await {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(err);
        }
        match rx.await {
            Ok(result) => result,
            Err(_) => {
                let exit = self.exit_info().unwrap_or(ExitInfo { status: "unknown".into(), stderr_tail: String::new() });
                Err(AcpError::RuntimeExited { status: exit.status, stderr_tail: exit.stderr_tail })
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), AcpError> {
        let message = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.inner.send(&message).await
    }
}

impl Inner {
    async fn send(&self, message: &Value) -> Result<(), AcpError> {
        let mut guard = self.writer.lock().await;
        let writer = guard.as_mut().ok_or_else(|| {
            let exit = self.exited.lock().unwrap().clone();
            match exit {
                Some(exit) => AcpError::RuntimeExited { status: exit.status, stderr_tail: exit.stderr_tail },
                None => AcpError::Io("agent stdin is closed".into()),
            }
        })?;
        let mut line = serde_json::to_vec(message).map_err(|err| AcpError::Protocol(err.to_string()))?;
        line.push(b'\n');
        writer.write_all(&line).await.map_err(|err| AcpError::Io(format!("writing to agent: {err}")))?;
        writer.flush().await.map_err(|err| AcpError::Io(format!("flushing to agent: {err}")))
    }

    async fn dispatch(&self, msg: Value) {
        let has_method = msg.get("method").is_some();
        let has_id = msg.get("id").map(|id| !id.is_null()).unwrap_or(false);
        match (has_method, has_id) {
            (false, true) => self.on_response(msg),
            (true, true) => self.on_request(msg).await,
            (true, false) => self.on_notification(msg),
            (false, false) => tracing::warn!("ignoring JSON-RPC message with neither method nor id: {}", truncate(&msg.to_string(), 200)),
        }
    }

    fn on_response(&self, msg: Value) {
        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
            tracing::warn!("ignoring response with a non-integer id: {}", truncate(&msg.to_string(), 200));
            return;
        };
        let Some((method, tx)) = self.pending.lock().unwrap().remove(&id) else {
            tracing::debug!("ignoring response to unknown request id {id}");
            return;
        };
        let outcome = if let Some(error) = msg.get("error") {
            Err(AcpError::Rpc {
                method,
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                data: error.get("data").cloned().map(Box::new),
            })
        } else {
            Ok(msg.get("result").cloned().unwrap_or(Value::Null))
        };
        let _ = tx.send(outcome);
    }

    async fn on_request(&self, msg: Value) {
        let id = msg["id"].clone();
        let method = msg["method"].as_str().unwrap_or("").to_string();
        let reply = match method.as_str() {
            "session/request_permission" => match serde_json::from_value::<PermissionRequest>(msg["params"].clone()) {
                Ok(request) => {
                    let decision = (self.permission_policy)(&request);
                    tracing::debug!("permission request in session {} answered with {decision:?}", request.session_id);
                    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": decision.to_result() })
                }
                Err(err) => serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32602, "message": format!("malformed permission request: {err}") },
                }),
            },
            other => {
                tracing::debug!("agent asked `{other}`, which pact does not implement; answering -32601");
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601, "message": format!("client does not implement {other}") },
                })
            }
        };
        if let Err(err) = self.send(&reply).await {
            tracing::warn!("could not answer agent request `{method}`: {err}");
        }
    }

    fn on_notification(&self, msg: Value) {
        let method = msg["method"].as_str().unwrap_or("");
        if method != "session/update" {
            tracing::debug!("ignoring agent notification `{method}`");
            return;
        }
        let params = &msg["params"];
        let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
            tracing::warn!("session/update without a sessionId");
            return;
        };
        let raw = params.get("update").cloned().unwrap_or(Value::Null);
        let kind = raw.get("sessionUpdate").and_then(Value::as_str).unwrap_or("").to_string();
        let update = SessionUpdate { session_id: session_id.to_string(), kind, raw };
        let sessions = self.sessions.lock().unwrap();
        match sessions.get(session_id) {
            Some(tx) => {
                let _ = tx.send(update);
            }
            None => tracing::debug!("update for unknown or closed session {session_id} dropped"),
        }
    }

    fn record_exit(&self, status: String) {
        let tail: Vec<String> = self.stderr_tail.lock().unwrap().iter().cloned().collect();
        let info = ExitInfo { status, stderr_tail: tail.join("\n") };
        let mut exited = self.exited.lock().unwrap();
        if exited.is_none() {
            *exited = Some(info);
        }
    }

    async fn on_exit(&self) {
        // During `shutdown`, the child was taken out and is being waited
        // on there; the status it records wins. Otherwise reap it here.
        let waited = {
            let mut child = self.child.lock().await;
            match child.as_mut() {
                Some(child) => Some(match child.wait().await {
                    Ok(status) => format!("{status}"),
                    Err(err) => format!("unknown ({err})"),
                }),
                None => None,
            }
        };
        if let Some(status) = waited {
            self.record_exit(status);
        }
        self.writer.lock().await.take();
        let exit = self
            .exited
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(ExitInfo { status: "exited".into(), stderr_tail: String::new() });
        let pending: Vec<_> = self.pending.lock().unwrap().drain().collect();
        for (_, (method, tx)) in pending {
            tracing::warn!("agent exited with `{method}` pending");
            let _ = tx.send(Err(AcpError::RuntimeExited { status: exit.status.clone(), stderr_tail: exit.stderr_tail.clone() }));
        }
        self.sessions.lock().unwrap().clear();
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(max).collect::<String>())
    }
}
