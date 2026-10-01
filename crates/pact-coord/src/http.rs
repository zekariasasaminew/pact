//! Streamable HTTP mode (issue #329): one coordination server for a whole
//! batch, one route per lane.
//!
//! Copilot's ACP mode accepts per-session MCP servers only over HTTP/SSE
//! (its debug log: `Rejecting non-http/sse MCP server "pact-coord" from
//! client`), so the ACP lane runtime (#306) cannot hand each session a
//! `pact mcp-serve` command the way `--additional-mcp-config` does today.
//! Instead the orchestrating process serves pact-coord itself, in-process,
//! and gives every lane its own URL:
//!
//! ```text
//! http://127.0.0.1:<port>/lanes/<agent-id>
//! ```
//!
//! Each lane, registered with `add_lane` as its workspace comes into
//! existence, is an rmcp `StreamableHttpService` whose factory builds the
//! same `CoordServer` the stdio path uses, with that lane's agent id and
//! workspace root, so lane identity comes from the URL and nothing about
//! the tool contract or the operation log changes. See DESIGN.md
//! ("pact-coord > Streamable HTTP mode").

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, Response, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use crate::server::CoordServer;

/// One lane's route: the agent id its tools act as, and the directory its
/// globs resolve against.
#[derive(Debug, Clone)]
pub struct LaneRoute {
    pub agent_id: String,
    pub workspace_root: PathBuf,
}

type LaneService = StreamableHttpService<CoordServer, LocalSessionManager>;

/// Lanes are registered while the server runs, because `spawn-many`
/// creates each lane's workspace (and so learns its id) inside the lane's
/// own thread, after the server is already up.
#[derive(Clone)]
struct Registry {
    repo_root: PathBuf,
    cancel: CancellationToken,
    lanes: Arc<RwLock<HashMap<String, LaneService>>>,
}

/// A running HTTP coordination server. Dropping it does not stop the
/// server; call [`HttpCoordServer::shutdown`].
pub struct HttpCoordServer {
    local_addr: SocketAddr,
    registry: Registry,
    task: tokio::task::JoinHandle<()>,
}

impl HttpCoordServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The URL to hand the lane whose workspace id is `agent_id`.
    pub fn lane_url(&self, agent_id: &str) -> String {
        format!("http://{}{}", self.local_addr, lane_path(agent_id))
    }

    /// Registers a lane and returns its URL. Each connecting client gets
    /// its own `CoordServer` acting as this lane, with its own connection
    /// to the coordination database, exactly as a `pact mcp-serve` process
    /// would. Registering an id again replaces the previous route.
    pub fn add_lane(&self, lane: LaneRoute) -> String {
        let repo_root = self.registry.repo_root.clone();
        let agent_id = lane.agent_id.clone();
        let workspace_root = lane.workspace_root;
        let service = StreamableHttpService::new(
            move || {
                let conn = crate::db::open(&repo_root).map_err(|err| std::io::Error::other(format!("{err:#}")))?;
                Ok(CoordServer::new(conn, agent_id.clone(), workspace_root.clone()))
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig { cancellation_token: self.registry.cancel.child_token(), ..Default::default() },
        );
        self.registry.lanes.write().unwrap().insert(lane.agent_id.clone(), service);
        tracing::debug!("pact-coord lane route added: {}", lane.agent_id);
        self.lane_url(&lane.agent_id)
    }

    /// Unregisters a lane; later requests to its URL are 404. Open MCP
    /// sessions on it end when the server shuts down.
    pub fn remove_lane(&self, agent_id: &str) {
        self.registry.lanes.write().unwrap().remove(agent_id);
    }

    pub fn lane_count(&self) -> usize {
        self.registry.lanes.read().unwrap().len()
    }

    /// Stops accepting connections and ends every open session.
    pub async fn shutdown(self) {
        self.registry.cancel.cancel();
        let _ = self.task.await;
    }
}

const LANES_PREFIX: &str = "/lanes/";

fn lane_path(agent_id: &str) -> String {
    format!("{LANES_PREFIX}{agent_id}")
}

/// Every request is dispatched by the `/lanes/<id>` prefix to that lane's
/// rmcp service, which itself only looks at the HTTP method and headers,
/// never the path. Anything else is 404.
async fn dispatch(State(registry): State<Registry>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path();
    let lane_id = path.strip_prefix(LANES_PREFIX).map(|rest| rest.split('/').next().unwrap_or("")).unwrap_or("");
    let service = if lane_id.is_empty() { None } else { registry.lanes.read().unwrap().get(lane_id).cloned() };
    match service {
        Some(service) => {
            let response = service.handle(request).await;
            response.map(Body::new)
        }
        None => Response::builder().status(StatusCode::NOT_FOUND).body(Body::from("no such lane")).unwrap(),
    }
}

/// Binds `bind` (use port 0 for an ephemeral port) and serves pact-coord
/// for `repo_root` until [`HttpCoordServer::shutdown`]. Lanes are added
/// with [`HttpCoordServer::add_lane`].
pub async fn serve(repo_root: &Path, bind: SocketAddr) -> Result<HttpCoordServer> {
    let registry = Registry { repo_root: repo_root.to_path_buf(), cancel: CancellationToken::new(), lanes: Arc::default() };
    let router = axum::Router::new().fallback(dispatch).with_state(registry.clone());

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding the coordination HTTP server to {bind}"))?;
    let local_addr = listener.local_addr().context("reading the bound address")?;
    let shutdown = registry.cancel.clone();
    let task = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
        {
            tracing::warn!("coordination HTTP server stopped with an error: {err}");
        }
    });
    tracing::info!("pact-coord serving lanes at http://{local_addr}{LANES_PREFIX}<id>");
    Ok(HttpCoordServer { local_addr, registry, task })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::ServiceExt;

    fn temp_repo(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pact-coord-http-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn cleanup(repo_root: &Path) {
        let _ = std::fs::remove_dir_all(crate::db::db_path(repo_root).unwrap().parent().unwrap());
        let _ = std::fs::remove_dir_all(repo_root);
    }

    async fn claim_through(url: &str, glob: &str) -> String {
        let transport = StreamableHttpClientTransport::from_uri(url);
        let client = ().serve(transport).await.expect("MCP handshake over HTTP");
        let tools = client.list_tools(Default::default()).await.unwrap();
        assert!(tools.tools.iter().any(|t| t.name == "claim_files"), "tools: {:?}", tools.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>());
        let result = client
            .call_tool(CallToolRequestParams {
                meta: None,
                name: "claim_files".into(),
                arguments: serde_json::json!({ "globs": [glob] }).as_object().cloned(),
                task: None,
            })
            .await
            .unwrap();
        let text = result.content.iter().filter_map(|c| c.as_text().map(|t| t.text.clone())).collect::<Vec<_>>().join("");
        client.cancel().await.unwrap();
        text
    }

    /// Two lanes, one server: each lane's claim lands under its own agent
    /// id, taken from the URL, and both show up as connected. This is the
    /// property the ACP runtime depends on (#306): identity by route.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn each_lane_route_acts_as_its_own_agent() {
        let repo_root = temp_repo("two-lanes");
        let server = serve(&repo_root, "127.0.0.1:0".parse().unwrap()).await.unwrap();
        assert_ne!(server.local_addr().port(), 0, "an ephemeral port must be reported");
        // Lanes join after the server is up, as spawn-many's threads will.
        let url_a = server.add_lane(LaneRoute { agent_id: "lane-a".into(), workspace_root: repo_root.clone() });
        let url_b = server.add_lane(LaneRoute { agent_id: "lane-b".into(), workspace_root: repo_root.clone() });
        assert_eq!(url_a, server.lane_url("lane-a"));
        assert!(url_a.ends_with("/lanes/lane-a"), "url: {url_a}");
        assert_eq!(server.lane_count(), 2);

        let a = claim_through(&url_a, "a.txt").await;
        let b = claim_through(&url_b, "b.txt").await;
        assert!(a.contains("accepted"), "lane-a claim result: {a}");
        assert!(b.contains("accepted"), "lane-b claim result: {b}");

        let snapshot = crate::status(&repo_root).unwrap();
        let mut holders: Vec<(String, String)> =
            snapshot.active_leases.iter().map(|l| (l.holder.clone(), l.pattern.clone())).collect();
        holders.sort();
        assert_eq!(holders, vec![("lane-a".to_string(), "a.txt".to_string()), ("lane-b".to_string(), "b.txt".to_string())]);
        assert_eq!(
            snapshot.connected_agent_ids,
            std::collections::HashSet::from(["lane-a".to_string(), "lane-b".to_string()]),
            "the initialize handshake over HTTP must log coord_connect per lane"
        );

        server.shutdown().await;
        cleanup(&repo_root);
    }

    /// A route that was never registered, or was removed, is not a lane:
    /// plain 404, no server created, nothing logged.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unknown_or_removed_lane_path_is_not_found() {
        let repo_root = temp_repo("unknown");
        let server = serve(&repo_root, "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let url_a = server.add_lane(LaneRoute { agent_id: "lane-a".into(), workspace_root: repo_root.clone() });

        let transport = StreamableHttpClientTransport::from_uri(server.lane_url("nobody"));
        assert!(().serve(transport).await.is_err(), "the handshake must fail against an unregistered lane route");
        assert!(crate::status(&repo_root).unwrap().connected_agent_ids.is_empty());

        server.remove_lane("lane-a");
        assert_eq!(server.lane_count(), 0);
        let transport = StreamableHttpClientTransport::from_uri(url_a);
        assert!(().serve(transport).await.is_err(), "a removed lane's route must be gone");
        assert!(crate::status(&repo_root).unwrap().connected_agent_ids.is_empty());

        server.shutdown().await;
        cleanup(&repo_root);
    }
}
