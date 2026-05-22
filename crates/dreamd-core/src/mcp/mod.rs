//! MCP server for dreamd (WEG-77).
//!
//! Exposes exactly two tools — `search_nodes` and `append_node` — whose names
//! are intentionally identical to the Anthropic reference memory server so that
//! MCP harnesses (Claude Code, Cursor, OpenCode) can treat dreamd as a drop-in
//! memory provider.
//!
//! Start-up logic:
//!   1. Try to connect to the dreamd daemon socket (Phase 2 bridge path).
//!      If the daemon is running, forward raw JSON-RPC stdio→socket→stdio.
//!   2. If the daemon is not running, fall back to an in-process MCP server
//!      that answers tool calls directly (Phase 1 fallback path).
//!
//! The Phase 1 tools are placeholders today; real recall/learn wiring lands
//! in WEG-78 and WEG-79 respectively.

use std::path::{Path, PathBuf};

use dreamd_protocol::{AgentLearning, EventId};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::coordinator::MemoryCoordinatorMsg;
use crate::layout::{DaemonHome, LayoutError};
use crate::privacy::DR413_DISCLOSURE;
use crate::server::{Supervisor, COORDINATOR_CHANNEL_CAPACITY};
use crate::AgentRoot;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors surfaced by [`run_mcp_server`].
#[derive(Debug, thiserror::Error)]
pub enum McpRunError {
    /// Underlying OS / file I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The MCP service itself returned an error string.
    #[error("MCP service error: {0}")]
    Service(String),
    /// Home directory could not be resolved.
    #[error("could not determine home directory")]
    NoHome,
}

// ---------------------------------------------------------------------------
// Tool parameter structs
// ---------------------------------------------------------------------------

/// Parameters for the `search_nodes` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchNodesParams {
    /// Free-text search query (BM25 × salience).
    pub query: String,
    /// Maximum number of results to return (default 5).
    pub k: Option<u32>,
}

/// Parameters for the `append_node` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppendNodeParams {
    /// The learning content to store.
    pub content: String,
    /// The agent harness that produced this learning (e.g. "claude-code").
    pub source_harness: String,
    /// Clustering key describing the skill or action domain (e.g. "rust/borrow-checker").
    pub skill_action: String,
    /// Pain score 0–10: how disruptive was this if not known?
    pub pain: Option<f64>,
    /// Importance score 0–10: how broadly applicable is this?
    pub importance: Option<f64>,
    /// Optional idempotency key. Duplicate calls with the same key return the
    /// cached id without a second write.
    pub client_dedup_key: Option<String>,
}

// ---------------------------------------------------------------------------
// MCP server struct
// ---------------------------------------------------------------------------

/// In-process MCP server (Phase 1 fallback when the daemon is not running).
///
/// When the daemon socket is reachable the server process instead acts as a
/// transparent bridge (Phase 2); this struct is only instantiated in the
/// fallback path.
#[derive(Clone)]
pub struct MemoryMcpServer {
    // Used by the #[tool_router] macro-generated dispatch code; the Rust
    // dead-code pass does not see macro-generated usage, hence the allow.
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<MemoryMcpServer>,
    agent_root: Option<AgentRoot>,
    coordinator_tx: Option<mpsc::Sender<MemoryCoordinatorMsg>>,
}

#[tool_router]
impl MemoryMcpServer {
    /// Create a new in-process memory MCP server with no agent root.
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            agent_root: None,
            coordinator_tx: None,
        }
    }

    /// Create a new in-process memory MCP server bound to a specific agent root.
    pub fn with_agent_root(root: AgentRoot) -> Self {
        Self {
            tool_router: Self::tool_router(),
            agent_root: Some(root),
            coordinator_tx: None,
        }
    }

    /// Create a new in-process memory MCP server bound to an agent root and a
    /// live coordinator channel. Used by the Phase 1 fallback when the daemon
    /// is not running.
    pub fn with_coordinator(root: AgentRoot, tx: mpsc::Sender<MemoryCoordinatorMsg>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            agent_root: Some(root),
            coordinator_tx: Some(tx),
        }
    }

    /// Search episodic memory using BM25 × salience scoring.
    ///
    /// Returns a ranked list of matching learning entries.
    #[cfg(unix)]
    #[tool(description = "Search episodic memory for past learnings -- use when: recall, did we discuss, what did we decide, previously decided.")]
    async fn search_nodes(
        &self,
        Parameters(p): Parameters<SearchNodesParams>,
    ) -> Result<CallToolResult, McpError> {
        use crate::index::build_schema;
        use crate::server::http::{RecallMeta, RecallResponse, RecallResultJson};
        use crate::server::tantivy_handle::{DEFAULT_COMMIT_CADENCE, TantivyIndexHandle};

        let k = p.k.unwrap_or(5) as usize;
        let query = p.query;

        let root = match &self.agent_root {
            Some(r) => r.clone(),
            None => {
                // No agent root discovered — return empty results.
                let resp = RecallResponse { results: vec![] };
                let json = serde_json::to_string(&resp)
                    .unwrap_or_else(|_| r#"{"results":[]}"#.to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
        };

        // Open (or reuse) the Tantivy index for this agent root.
        let handle = match TantivyIndexHandle::open(&root, DEFAULT_COMMIT_CADENCE) {
            Ok(h) => h,
            Err(e) => {
                let msg = format!("index open failed: {e}");
                return Err(McpError::invalid_request(msg, None));
            }
        };

        let reader = handle.reader().clone();
        let (_, schema_fields) = build_schema();
        let now_sec = chrono::Utc::now().timestamp();

        match crate::recall(&reader, &schema_fields, &query, k, None, now_sec) {
            Ok(results) => {
                let json_results: Vec<RecallResultJson> = results
                    .into_iter()
                    .map(|r| RecallResultJson {
                        score: r.score,
                        bm25: r.bm25,
                        salience: r.salience,
                        source: format!("{:?}", r.layer).to_lowercase(),
                        content: r.content,
                        metadata: RecallMeta {
                            timestamp_sec: r.timestamp_sec,
                            pain: r.pain,
                            importance: r.importance,
                            recurrence: r.recurrence,
                        },
                    })
                    .collect();
                let resp = RecallResponse {
                    results: json_results,
                };
                let json = serde_json::to_string(&resp)
                    .unwrap_or_else(|_| r#"{"results":[]}"#.to_string());
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Err(e) => Err(McpError::invalid_request(format!("recall failed: {e}"), None)),
        }
    }

    /// Search episodic memory using BM25 × salience scoring (non-Unix stub).
    #[cfg(not(unix))]
    #[tool(description = "Search episodic memory for past learnings -- use when: recall, did we discuss, what did we decide, previously decided.")]
    async fn search_nodes(
        &self,
        Parameters(p): Parameters<SearchNodesParams>,
    ) -> Result<CallToolResult, McpError> {
        let _query = p.query;
        let _k = p.k.unwrap_or(5);
        Ok(CallToolResult::success(vec![Content::text(
            r#"{"results":[]}"#,
        )]))
    }

    /// Append a new learning node to episodic memory.
    ///
    /// The entry is durably fsynced before this call returns (DR-103).
    #[tool(description = "Append a new learning to episodic memory -- use when: note that, remember, log this, save this lesson.")]
    async fn append_node(
        &self,
        Parameters(p): Parameters<AppendNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let tx = match self.coordinator_tx.as_ref() {
            Some(tx) => tx,
            None => {
                return Err(McpError::internal_error(
                    "coordinator unavailable: no agent root found",
                    None,
                ));
            }
        };

        // Validate skill_action: trim → lowercase → collapse whitespace →
        // replace spaces with `_` → charset [a-z0-9_:.-] → ≤ 256 bytes.
        // Same rules as post_learn in server/http.rs.
        let lowercased = p.skill_action.trim().to_lowercase();
        let sa: String = lowercased.split_whitespace().collect::<Vec<_>>().join("_");
        if sa.is_empty() {
            return Err(McpError::invalid_request(
                "invalid skill_action: empty after normalisation",
                None,
            ));
        }
        if sa.len() > 256 {
            return Err(McpError::invalid_request(
                "invalid skill_action: exceeds 256 bytes",
                None,
            ));
        }
        if sa
            .bytes()
            .any(|b| !matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b':' | b'.' | b'-'))
        {
            return Err(McpError::invalid_request(
                "invalid skill_action: contains characters outside [a-z0-9_:.-]",
                None,
            ));
        }

        let timestamp = chrono::Utc::now();
        let learning = AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: EventId::parse("evt_00000000000000000000000000").unwrap(),
            timestamp,
            pain: p.pain.unwrap_or(5.0) as f32,
            importance: p.importance.unwrap_or(5.0) as f32,
            pinned: false,
            skill_action: sa,
            source_harness: p.source_harness,
            content: p.content,
        };

        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        tx.send(MemoryCoordinatorMsg::AppendLearning {
            learning,
            client_dedup_key: p.client_dedup_key,
            response_tx: resp_tx,
        })
        .await
        .map_err(|_| McpError::internal_error("coordinator unavailable", None))?;

        let outcome = resp_rx
            .await
            .map_err(|_| McpError::internal_error("coordinator dropped", None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let json = serde_json::json!({
            "id": outcome.id.to_string(),
            "timestamp": timestamp.to_rfc3339(),
            "deduplicated": outcome.deduplicated,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string(&json)
                .unwrap_or_else(|_| r#"{"error":"serialize failed"}"#.to_string()),
        )]))
    }
}

#[tool_handler]
impl ServerHandler for MemoryMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "dreamd-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "dreamd memory server — search and append episodic agent learnings.",
            )
    }
}

impl Default for MemoryMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Phase 2 bridge (Unix only — daemon socket is UDS)
// ---------------------------------------------------------------------------

/// Resolve the path to the daemon Unix socket.
///
/// Priority:
/// 1. `$DREAMD_SOCK` env var (override for testing / custom installs).
/// 2. `DaemonHome::new(~/.agent).socket_path()`.
fn resolve_sock_path() -> Result<PathBuf, McpRunError> {
    if let Some(v) = std::env::var_os("DREAMD_SOCK") {
        return Ok(PathBuf::from(v));
    }
    let home = dirs::home_dir().ok_or(McpRunError::NoHome)?;
    let daemon_home = DaemonHome::new(home.join(".agent"));
    Ok(daemon_home.socket_path())
}

/// Forward JSON-RPC stdio → daemon socket → stdout (Phase 2 bridge).
///
/// Wires stdin → socket-write half and socket-read half → stdout using
/// two concurrent copy tasks. Runs until either direction reaches EOF.
///
/// The agent root (found via `AgentRoot::discover`) is logged to stderr so
/// harness logs capture which project store is in use.
///
/// NOTE: X-Agent-Root injection into JSON-RPC params is deferred.
// TODO(WEG-77): inject X-Agent-Root into forwarded JSON-RPC messages by
// parsing each line, adding `"_meta": {"x-agent-root": "<path>"}` to params,
// and re-serialising before forwarding to the socket. For now we do a raw
// byte-copy pass-through.
#[cfg(unix)]
async fn run_bridge(
    stream: tokio::net::UnixStream,
    cwd: &Path,
) -> Result<(), McpRunError> {
    // Log which agent root (if any) this bridge session is bound to.
    match AgentRoot::discover(cwd) {
        Ok(root) => {
            eprintln!(
                "dreamd mcp: bridge connected — agent root: {}",
                root.project_root().display()
            );
        }
        Err(LayoutError::NotFound) => {
            eprintln!(
                "dreamd mcp: bridge connected — no .agent/ found in ancestry of {}",
                cwd.display()
            );
        }
    }

    let (mut sock_read, mut sock_write) = tokio::io::split(stream);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    // Drive both copy directions concurrently; exit when either side closes.
    tokio::select! {
        r = tokio::io::copy(&mut stdin, &mut sock_write) => {
            r?;
        }
        r = tokio::io::copy(&mut sock_read, &mut stdout) => {
            r?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Start the MCP server process.
///
/// Phase 2 (bridge, Unix only): if the daemon socket is reachable, forward
/// stdio transparently to the daemon and return when the session ends.
///
/// Phase 1 (fallback): if the daemon is not running (or on Windows where
/// UDS bridging is deferred to DR-121), serve MCP tool calls in-process
/// using [`MemoryMcpServer`].
///
/// Privacy disclosure ([`DR413_DISCLOSURE`]) is printed to stderr when this
/// is the first invocation in a directory that has no `.agent/` store yet.
pub async fn run_mcp_server(cwd: &Path) -> Result<(), McpRunError> {
    // Emit privacy disclosure to stderr if no .agent/ store is found.
    // This is the "first run" signal for MCP harness users.
    if AgentRoot::discover(cwd).is_err() {
        eprintln!("{DR413_DISCLOSURE}");
    }

    // Resolve daemon socket path (used on Unix; on Windows the Phase 2 bridge
    // is deferred so we still need the path for the log message).
    let sock_path = resolve_sock_path()?;

    // Phase 2 (Unix only): try to connect to the running daemon over UDS.
    // The daemon socket is a Unix domain socket; this path is intentionally
    // absent on Windows until DR-121 (TCP fallback) ships.
    #[cfg(unix)]
    match tokio::net::UnixStream::connect(&sock_path).await {
        Ok(stream) => {
            // Daemon is running — act as a transparent bridge.
            return run_bridge(stream, cwd).await;
        }
        Err(_) => {
            // Daemon not running — fall through to Phase 1 in-process server.
            eprintln!(
                "dreamd mcp: daemon not found at {} — running in-process (Phase 1 fallback)",
                sock_path.display()
            );
        }
    }

    // Suppress unused-variable warning on non-Unix targets where Phase 2 is
    // not compiled in.
    #[cfg(not(unix))]
    let _ = &sock_path;

    // Phase 1: run in-process MCP server over stdio.
    // When an agent root is found, boot a MemoryCoordinator via Supervisor so
    // append_node dispatches durably. `supervisor` is bound here and must
    // outlive the serve call; it drops after svc.waiting() returns.
    match AgentRoot::discover(cwd) {
        Ok(root) => {
            let supervisor = Supervisor::start(&root, COORDINATOR_CHANNEL_CAPACITY, None)
                .map_err(|e| McpRunError::Service(e.to_string()))?;
            let tx = supervisor.sender();
            let svc = MemoryMcpServer::with_coordinator(root, tx)
                .serve(rmcp::transport::stdio())
                .await
                .map_err(|e| McpRunError::Service(e.to_string()))?;
            svc.waiting()
                .await
                .map_err(|e| McpRunError::Service(e.to_string()))?;
            // supervisor drops here, after serve completes
        }
        Err(_) => {
            let svc = MemoryMcpServer::new()
                .serve(rmcp::transport::stdio())
                .await
                .map_err(|e| McpRunError::Service(e.to_string()))?;
            svc.waiting()
                .await
                .map_err(|e| McpRunError::Service(e.to_string()))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a tempdir with the minimal `.agent/episodic/` layout so that
    /// `AgentRoot::discover` and `TantivyIndexHandle::open` both succeed.
    fn setup_agent_root(dir: &std::path::Path) -> AgentRoot {
        let root = AgentRoot::new(dir);
        std::fs::create_dir_all(root.episodic_dir()).expect("create episodic dir");
        root
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn search_nodes_empty_store_returns_empty() {
        use rmcp::handler::server::wrapper::Parameters;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = setup_agent_root(dir.path());

        let server = MemoryMcpServer::with_agent_root(root);
        let result = server
            .search_nodes(Parameters(SearchNodesParams {
                query: "rust".to_string(),
                k: Some(3),
            }))
            .await
            .expect("search_nodes ok");

        // Extract the text content from the result.
        let text = result
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.as_str()))
            .expect("text content present");

        let parsed: serde_json::Value =
            serde_json::from_str(text).expect("valid json");
        let results = parsed["results"].as_array().expect("results array");
        assert!(
            results.is_empty(),
            "empty index must return empty results; got: {parsed:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn search_nodes_seeded_index_returns_results() {
        use crate::server::index_map::IndexHandle;
        use crate::server::tantivy_handle::{
            DEFAULT_COMMIT_CADENCE, IndexerMsg, TantivyIndexHandle,
        };
        use chrono::{DateTime, Utc};
        use dreamd_protocol::{AgentLearning, EventId};
        use rmcp::handler::server::wrapper::Parameters;
        use tokio::sync::oneshot;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = setup_agent_root(dir.path());

        // Seed two learnings via the indexer so they are committed before recall.
        {
            let handle =
                TantivyIndexHandle::open(&root, DEFAULT_COMMIT_CADENCE).expect("open handle");
            let tx = handle.sender();

            for (suffix, content) in [('0', "rust async tokio channel"), ('1', "rust borrow checker lifetime")] {
                let raw_id = format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}");
                let id = EventId::parse(&raw_id).expect("valid event id");
                let learning = AgentLearning {
                    schema_version: "1.0".to_string(),
                    id: id.clone(),
                    timestamp: DateTime::parse_from_rfc3339("2026-05-22T10:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                    pain: 7.0,
                    importance: 8.0,
                    pinned: false,
                    skill_action: "rust.tokio".to_string(),
                    source_harness: "test-harness".to_string(),
                    content: content.to_string(),
                };
                tx.send(IndexerMsg::Append {
                    event_id: id,
                    learning,
                })
                .await
                .expect("send append");
            }

            // Flush to ensure the commit lands before we query.
            let (ack_tx, ack_rx) = oneshot::channel();
            tx.send(IndexerMsg::Flush { ack: ack_tx })
                .await
                .expect("send flush");
            ack_rx.await.expect("flush ack recv").expect("flush ok");

            drop(tx);
            handle.shutdown().await.expect("shutdown");
        }

        // Now query via search_nodes.
        let server = MemoryMcpServer::with_agent_root(root);
        let result = server
            .search_nodes(Parameters(SearchNodesParams {
                query: "rust tokio async".to_string(),
                k: Some(5),
            }))
            .await
            .expect("search_nodes ok");

        let text = result
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.clone()))
            .expect("text content present");

        let parsed: serde_json::Value =
            serde_json::from_str(&text).expect("valid json");
        let results = parsed["results"].as_array().expect("results array");

        assert!(
            !results.is_empty(),
            "seeded index must return at least one result; got: {parsed:?}"
        );
        let score = results[0]["score"].as_f64().expect("score field");
        assert!(score > 0.0, "top result must have positive score; got {score}");
    }

    #[tokio::test]
    async fn append_node_no_coordinator_returns_error() {
        use rmcp::handler::server::wrapper::Parameters;

        let server = MemoryMcpServer::new();
        let result = server
            .append_node(Parameters(AppendNodeParams {
                content: "test content".to_string(),
                source_harness: "test".to_string(),
                skill_action: "rust.test".to_string(),
                pain: None,
                importance: None,
                client_dedup_key: None,
            }))
            .await;

        assert!(
            result.is_err(),
            "append_node with no coordinator must return an error"
        );
    }

    #[tokio::test]
    async fn append_node_with_coordinator_returns_outcome() {
        use crate::server::{Supervisor, COORDINATOR_CHANNEL_CAPACITY};
        use rmcp::handler::server::wrapper::Parameters;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = setup_agent_root(dir.path());

        let supervisor =
            Supervisor::start(&root, COORDINATOR_CHANNEL_CAPACITY, None).expect("start supervisor");
        let tx = supervisor.sender();
        let server = MemoryMcpServer::with_coordinator(root, tx);

        let result = server
            .append_node(Parameters(AppendNodeParams {
                content: "test content".to_string(),
                source_harness: "test-harness".to_string(),
                skill_action: "rust.test".to_string(),
                pain: Some(6.0),
                importance: Some(7.0),
                client_dedup_key: None,
            }))
            .await
            .expect("append_node ok");

        let text = result
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.as_str()))
            .expect("text content present");

        let parsed: serde_json::Value = serde_json::from_str(text).expect("valid json");
        assert!(
            parsed["id"].as_str().unwrap_or("").starts_with("evt_"),
            "id must be daemon-minted; got: {parsed:?}"
        );
        assert!(
            parsed["timestamp"].as_str().is_some(),
            "timestamp must be present; got: {parsed:?}"
        );
        assert_eq!(
            parsed["deduplicated"],
            serde_json::json!(false),
            "first append must not be deduplicated"
        );
    }
}
