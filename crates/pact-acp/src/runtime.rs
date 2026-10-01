//! Blocking facade over [`AcpClient`] for pact-core's thread-per-lane
//! model (issue #330). One `AcpRuntime` owns one agent process and a
//! small tokio runtime on background threads; each lane thread calls
//! `new_session` / `prompt` / `close` synchronously, exactly as it calls
//! `Command::spawn` and reads stdout today, so nothing above this layer
//! has to become async.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::client::{AcpClient, AcpError, AgentSpec, ExitInfo, PermissionPolicy};
use crate::protocol::*;

pub struct AcpRuntime {
    runtime: tokio::runtime::Runtime,
    client: AcpClient,
    initialize: InitializeResult,
}

/// A lane's session: its id plus the update stream `prompt` drains.
#[derive(Debug)]
pub struct LaneSession {
    pub id: String,
    pub config_options: Vec<serde_json::Value>,
    updates: mpsc::UnboundedReceiver<SessionUpdate>,
}

impl AcpRuntime {
    /// Starts the agent, completes `initialize`, and returns. Fails if the
    /// agent cannot be spawned, exits before answering, or speaks another
    /// protocol major version.
    pub fn start(spec: AgentSpec, permission_policy: PermissionPolicy) -> Result<Self, AcpError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("pact-acp")
            .enable_all()
            .build()
            .map_err(|err| AcpError::Io(format!("starting the ACP runtime: {err}")))?;
        let client = runtime.block_on(AcpClient::spawn(spec, permission_policy))?;
        let initialize = match runtime.block_on(client.initialize()) {
            Ok(result) => result,
            Err(err) => {
                runtime.block_on(client.shutdown());
                return Err(err);
            }
        };
        Ok(AcpRuntime { runtime, client, initialize })
    }

    /// The default unattended policy: see [`allow_first_permission`].
    pub fn start_unattended(spec: AgentSpec) -> Result<Self, AcpError> {
        Self::start(spec, Arc::new(allow_first_permission))
    }

    pub fn initialize_result(&self) -> &InitializeResult {
        &self.initialize
    }

    pub fn pid(&self) -> u32 {
        self.client.pid()
    }

    pub fn exit_info(&self) -> Option<ExitInfo> {
        self.client.exit_info()
    }

    pub fn new_session(&self, cwd: &Path, mcp_servers: Vec<McpServer>) -> Result<LaneSession, AcpError> {
        let (result, updates) = self.runtime.block_on(self.client.new_session(cwd, mcp_servers))?;
        Ok(LaneSession { id: result.session_id, config_options: result.config_options, updates })
    }

    /// Sends `text` and blocks until the turn ends, calling `on_update`
    /// on the caller's thread for every `session/update` the agent sends
    /// for this session in the meantime. Updates that arrive between
    /// prompts are delivered at the start of the next one.
    pub fn prompt(
        &self,
        session: &mut LaneSession,
        text: &str,
        mut on_update: impl FnMut(SessionUpdate),
    ) -> Result<StopReason, AcpError> {
        let client = self.client.clone();
        let session_id = session.id.clone();
        let updates = &mut session.updates;
        self.runtime.block_on(async move {
            let turn = client.prompt(&session_id, text);
            tokio::pin!(turn);
            let mut stream_open = true;
            loop {
                tokio::select! {
                    biased;
                    update = updates.recv(), if stream_open => match update {
                        Some(update) => on_update(update),
                        None => stream_open = false,
                    },
                    outcome = &mut turn => {
                        // Deliver whatever the agent sent before the final
                        // response but after our last poll.
                        while let Ok(update) = updates.try_recv() {
                            on_update(update);
                        }
                        return outcome;
                    }
                }
            }
        })
    }

    pub fn cancel(&self, session: &LaneSession) -> Result<(), AcpError> {
        self.cancel_by_id(&session.id)
    }

    /// Cancels a session by id, for a watcher that cannot borrow the
    /// `LaneSession` while `prompt` holds it.
    pub fn cancel_by_id(&self, session_id: &str) -> Result<(), AcpError> {
        self.runtime.block_on(self.client.cancel(session_id))
    }

    pub fn close(&self, session: &LaneSession) -> Result<(), AcpError> {
        self.runtime.block_on(self.client.close(&session.id))
    }

    /// Ends the agent process (graceful, then kill) and the runtime.
    pub fn shutdown(self) {
        self.runtime.block_on(self.client.shutdown());
        self.runtime.shutdown_timeout(Duration::from_secs(2));
    }
}
