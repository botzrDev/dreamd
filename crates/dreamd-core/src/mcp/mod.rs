//! MCP server for dreamd (WEG-77).
//!
//! Exposes exactly two tools — `search_nodes` and `append_node` — whose names
//! are intentionally identical to the Anthropic reference memory server so that
//! MCP harnesses (Claude Code, Cursor, OpenCode) can treat dreamd as a drop-in
//! memory provider.
//!
//! Start-up logic:
//!   1. If the dreamd daemon socket is reachable (Phase 2), serve the same MCP
//!      tool surface over stdio but back each tool call with an HTTP-over-UDS
//!      client to the daemon (`Backend::Remote`) so memory is shared across
//!      every harness pointed at the same daemon.
//!   2. If the daemon is not running, fall back to an in-process MCP server
//!      that answers tool calls directly (Phase 1 fallback, `Backend::Local`).
//!
//! Both paths expose identical tools: `search_nodes` performs recall (WEG-78)
//! and `append_node` writes the learning durably (WEG-79).

use std::path::{Path, PathBuf};

// Phase 2 Remote backend (Unix only) — HTTP-over-UDS client deps.
#[cfg(unix)]
use bytes::Bytes;
#[cfg(unix)]
use http_body_util::{BodyExt, Full};

use dreamd_protocol::{AgentLearning, EventId, SkillAction, RECORD_SCHEMA_VERSION};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::config::load_config;
use crate::coordinator::MemoryCoordinatorMsg;
use crate::layout::DaemonHome;
use crate::privacy::DR413_DISCLOSURE;
use crate::redaction::redact;
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
    /// `DREAMD_SOCK` was set to a relative path.
    #[error("DREAMD_SOCK is not an absolute path: {0}")]
    InvalidSockPath(PathBuf),
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
    /// Clustering key describing the skill or action domain (e.g. "rust::borrow_checker").
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

/// Backing store for a [`MemoryMcpServer`].
///
/// `Local` / `LocalReadOnly` / `Empty` are the Phase 1 in-process paths.
/// `Remote` is the Phase 2 path: each tool call is proxied to the running
/// daemon over an HTTP-over-UDS client (WEG-259) so memory is shared across
/// harnesses pointed at the same daemon.
#[derive(Clone)]
enum Backend {
    /// Phase 1: local in-process coordinator (durable writes + reads via the
    /// shared Tantivy handle).
    Local {
        agent_root: AgentRoot,
        coordinator_tx: mpsc::Sender<MemoryCoordinatorMsg>,
    },
    /// Phase 1 read-only: agent root known but no coordinator (search only).
    LocalReadOnly { agent_root: AgentRoot },
    /// No `.agent/` found — recall returns empty results; append errors.
    Empty,
    /// Phase 2: HTTP-over-UDS to the running daemon. `agent_root_header` is the
    /// canonicalized project-root string sent as the `X-Agent-Root` header
    /// (matches `resolve_project`'s server-side canonical lookup).
    #[cfg(unix)]
    Remote {
        sock_path: PathBuf,
        agent_root_header: String,
    },
}

/// MCP server exposing the `search_nodes` / `append_node` tool pair over an
/// in-process ([`Backend::Local`]) or daemon-backed ([`Backend::Remote`])
/// store.
#[derive(Clone)]
pub struct MemoryMcpServer {
    // Used by the #[tool_router] macro-generated dispatch code; the Rust
    // dead-code pass does not see macro-generated usage, hence the allow.
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<MemoryMcpServer>,
    backend: Backend,
}

#[tool_router]
impl MemoryMcpServer {
    /// Create a new in-process memory MCP server with no agent root
    /// ([`Backend::Empty`]).
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            backend: Backend::Empty,
        }
    }

    /// Create a read-only in-process server bound to a specific agent root
    /// ([`Backend::LocalReadOnly`]).
    pub fn with_agent_root(root: AgentRoot) -> Self {
        Self {
            tool_router: Self::tool_router(),
            backend: Backend::LocalReadOnly { agent_root: root },
        }
    }

    /// Create an in-process server bound to an agent root and a live
    /// coordinator channel ([`Backend::Local`]). Used by the Phase 1 fallback
    /// when the daemon is not running.
    pub fn with_coordinator(root: AgentRoot, tx: mpsc::Sender<MemoryCoordinatorMsg>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            backend: Backend::Local {
                agent_root: root,
                coordinator_tx: tx,
            },
        }
    }

    /// Create a daemon-backed server ([`Backend::Remote`]): tool calls are
    /// proxied to the running daemon over HTTP-over-UDS. `agent_root_header`
    /// must be the canonicalized project-root string (see `run_mcp_server`).
    #[cfg(unix)]
    pub fn with_remote(sock_path: PathBuf, agent_root_header: String) -> Self {
        Self {
            tool_router: Self::tool_router(),
            backend: Backend::Remote {
                sock_path,
                agent_root_header,
            },
        }
    }

    /// Search episodic memory using BM25 × salience scoring.
    ///
    /// Returns a ranked list of matching learning entries.
    #[cfg(unix)]
    #[tool(
        description = "Search episodic memory for past learnings -- use when: recall, did we discuss, what did we decide, previously decided."
    )]
    async fn search_nodes(
        &self,
        Parameters(p): Parameters<SearchNodesParams>,
    ) -> Result<CallToolResult, McpError> {
        use crate::index::build_schema;
        use crate::server::http::{RecallMeta, RecallResponse, RecallResultJson};
        use crate::server::tantivy_handle::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};

        let k = p.k.unwrap_or(5);
        let query = p.query;

        // Resolve the read path. Local/LocalReadOnly read the on-disk Tantivy
        // index; Empty short-circuits to empty results; Remote proxies to the
        // daemon over HTTP-over-UDS.
        let root = match &self.backend {
            Backend::Local { agent_root, .. } | Backend::LocalReadOnly { agent_root } => {
                agent_root.clone()
            }
            Backend::Empty => {
                let resp = RecallResponse { results: vec![] };
                let json = serde_json::to_string(&resp)
                    .unwrap_or_else(|_| r#"{"results":[]}"#.to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
            Backend::Remote {
                sock_path,
                agent_root_header,
            } => {
                let req = build_recall_request(&query, k, agent_root_header)?;
                let (status, body) = send_remote(sock_path, req).await?;
                if status.is_success() {
                    // The daemon already returns the canonical {"results":[...]}
                    // shape; forward it verbatim.
                    let text = String::from_utf8_lossy(&body).into_owned();
                    return Ok(CallToolResult::success(vec![Content::text(text)]));
                }
                return Err(map_remote_status_error(status, &body));
            }
        };

        // Local read path. Open (or reuse) the Tantivy index for this root.
        let k = k as usize;
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

        let results = crate::recall(&reader, &schema_fields, &query, k, None, now_sec)
            .map_err(|e| McpError::invalid_request(format!("recall failed: {e}"), None))?;
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
        let json = serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"results":[]}"#.to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Search episodic memory using BM25 × salience scoring (non-Unix stub).
    #[cfg(not(unix))]
    #[tool(
        description = "Search episodic memory for past learnings -- use when: recall, did we discuss, what did we decide, previously decided."
    )]
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
    #[tool(
        description = "Append a new learning to episodic memory -- use when: note that, remember, log this, save this lesson."
    )]
    async fn append_node(
        &self,
        Parameters(p): Parameters<AppendNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        match &self.backend {
            Backend::Local {
                agent_root,
                coordinator_tx,
            } => {
                // Validate skill_action via the single SkillAction parser
                // (same rules as post_learn in server/http.rs).
                let skill_action = SkillAction::parse(&p.skill_action)
                    .map_err(|e| McpError::invalid_request(e.to_string(), None))?;

                // Range-check scores BEFORE unwrap_or default (parse, don't clamp).
                if let Some(pain) = p.pain {
                    if !(0.0..=10.0).contains(&pain) {
                        return Err(McpError::invalid_request(
                            "pain must be in 0.0..=10.0",
                            None,
                        ));
                    }
                }
                if let Some(importance) = p.importance {
                    if !(0.0..=10.0).contains(&importance) {
                        return Err(McpError::invalid_request(
                            "importance must be in 0.0..=10.0",
                            None,
                        ));
                    }
                }

                let config = load_config(agent_root.project_root()).map_err(|e| {
                    McpError::internal_error(format!("config load failed: {e}"), None)
                })?;
                let content = redact(&p.content, config.redaction);

                let timestamp = chrono::Utc::now();
                let learning = AgentLearning {
                    schema_version: RECORD_SCHEMA_VERSION.to_string(),
                    id: EventId::parse("evt_00000000000000000000000000").unwrap(),
                    timestamp,
                    pain: p.pain.unwrap_or(5.0) as f32,
                    importance: p.importance.unwrap_or(5.0) as f32,
                    pinned: false,
                    skill_action: skill_action.into_string(),
                    source_harness: p.source_harness,
                    content,
                };

                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                coordinator_tx
                    .send(MemoryCoordinatorMsg::AppendLearning {
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
            Backend::LocalReadOnly { .. } | Backend::Empty => Err(McpError::internal_error(
                "coordinator unavailable: no agent root found",
                None,
            )),
            #[cfg(unix)]
            Backend::Remote {
                sock_path,
                agent_root_header,
            } => {
                let req = build_learn_request(&p, agent_root_header)?;
                let (status, body) = send_remote(sock_path, req).await?;
                if status.is_success() {
                    // The daemon already returns {"id","timestamp","deduplicated"};
                    // forward it verbatim.
                    let text = String::from_utf8_lossy(&body).into_owned();
                    Ok(CallToolResult::success(vec![Content::text(text)]))
                } else {
                    Err(map_remote_status_error(status, &body))
                }
            }
        }
    }
}

#[tool_handler]
impl ServerHandler for MemoryMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("dreamd-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions("dreamd memory server — search and append episodic agent learnings.")
    }
}

impl Default for MemoryMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Phase 2 Remote backend (Unix only — daemon socket is UDS)
// ---------------------------------------------------------------------------

/// Resolve the path to the daemon Unix socket.
///
/// Priority:
/// 1. `$DREAMD_SOCK` env var (override for testing / custom installs).
/// 2. `DaemonHome::new(~/.agent).socket_path()`.
fn resolve_sock_path() -> Result<PathBuf, McpRunError> {
    if let Some(v) = std::env::var_os("DREAMD_SOCK") {
        let path = PathBuf::from(v);
        if !path.is_absolute() {
            return Err(McpRunError::InvalidSockPath(path));
        }
        return Ok(path);
    }
    let home = dirs::home_dir().ok_or(McpRunError::NoHome)?;
    let daemon_home = DaemonHome::new(home.join(".agent"));
    Ok(daemon_home.socket_path())
}

/// Percent-encode a query-string value (RFC 3986 unreserved set passes
/// through; everything else becomes `%XX`). Inlined rather than pulling a
/// URL-encoding crate — WEG-259 deliberately adds only hyper / http-body-util
/// / bytes as new deps.
#[cfg(unix)]
fn percent_encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build `GET /api/v1/recall?q=<url-encoded>&k=<k>` with the `X-Agent-Root`
/// header. Pure (no socket, no async) so it can be unit-tested directly.
#[cfg(unix)]
fn build_recall_request(
    query: &str,
    k: u32,
    agent_root_header: &str,
) -> Result<hyper::Request<Full<Bytes>>, McpError> {
    let uri = format!("/api/v1/recall?q={}&k={k}", percent_encode_query(query));
    hyper::Request::builder()
        .method(hyper::Method::GET)
        .uri(uri)
        .header(hyper::header::HOST, "localhost")
        .header("x-agent-root", agent_root_header)
        .body(Full::new(Bytes::new()))
        .map_err(|e| McpError::internal_error(format!("build recall request failed: {e}"), None))
}

/// Build `POST /api/v1/learn` with `X-Agent-Root`, `Content-Type:
/// application/json`, an optional `X-Client-Dedup-Key`, and a JSON body that
/// deserializes as [`AgentLearning`] (the shape `post_learn` expects). The
/// daemon mints the real `id` and re-normalises `skill_action`, so we send a
/// placeholder id and the raw skill_action. Pure: unit-tested directly.
#[cfg(unix)]
fn build_learn_request(
    params: &AppendNodeParams,
    agent_root_header: &str,
) -> Result<hyper::Request<Full<Bytes>>, McpError> {
    let learning = AgentLearning {
        schema_version: "1.0.0".to_string(),
        id: EventId::parse("evt_00000000000000000000000000")
            .expect("static placeholder EventId is valid"),
        timestamp: chrono::Utc::now(),
        pain: params.pain.unwrap_or(5.0) as f32,
        importance: params.importance.unwrap_or(5.0) as f32,
        pinned: false,
        skill_action: params.skill_action.clone(),
        source_harness: params.source_harness.clone(),
        content: params.content.clone(),
    };
    let body = serde_json::to_vec(&learning)
        .map_err(|e| McpError::internal_error(format!("serialize learning failed: {e}"), None))?;

    let mut builder = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri("/api/v1/learn")
        .header(hyper::header::HOST, "localhost")
        .header("x-agent-root", agent_root_header)
        .header(hyper::header::CONTENT_TYPE, "application/json");
    if let Some(key) = params.client_dedup_key.as_deref() {
        builder = builder.header("x-client-dedup-key", key);
    }
    builder
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| McpError::internal_error(format!("build learn request failed: {e}"), None))
}

/// Map a non-success daemon HTTP status to an [`McpError`] per the WEG-259
/// error table. Never panics.
#[cfg(unix)]
fn map_remote_status_error(status: hyper::StatusCode, body: &Bytes) -> McpError {
    let body_str = String::from_utf8_lossy(body);
    match status.as_u16() {
        400 => McpError::invalid_request(format!("invalid X-Agent-Root: {body_str}"), None),
        404 => McpError::invalid_request(
            format!("project not registered with daemon — run dreamd init: {body_str}"),
            None,
        ),
        413 => McpError::invalid_request("learning exceeds size limit", None),
        415 => McpError::invalid_request("content-type rejected (defensive)", None),
        _ => McpError::internal_error(format!("daemon error {status}: {body_str}"), None),
    }
}

/// Open a fresh HTTP-over-UDS connection to the daemon, send one request, and
/// collect the response into `(status, body)`. Per-call connect (no pooling) —
/// MCP tool calls are infrequent; the WEG-78-A "fresh handle per call"
/// rationale applies. Connection / handshake / I/O failures map to
/// `internal_error`.
#[cfg(unix)]
async fn send_remote(
    sock_path: &Path,
    req: hyper::Request<Full<Bytes>>,
) -> Result<(hyper::StatusCode, Bytes), McpError> {
    use hyper::client::conn::http1;
    use hyper_util::rt::TokioIo;

    let stream = tokio::net::UnixStream::connect(sock_path)
        .await
        .map_err(|e| McpError::internal_error(format!("daemon connect failed: {e}"), None))?;
    // TokioIo wrap is required: a raw UnixStream does not implement hyper's IO
    // trait (WEG-72-B drift entry).
    let io = TokioIo::new(stream);
    let (mut sender, conn) = http1::handshake(io)
        .await
        .map_err(|e| McpError::internal_error(format!("daemon handshake failed: {e}"), None))?;
    // Drive the connection in the background until the stream closes.
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| McpError::internal_error(format!("daemon request failed: {e}"), None))?;
    let status = resp.status();
    let body = resp
        .collect()
        .await
        .map_err(|e| McpError::internal_error(format!("daemon response read failed: {e}"), None))?
        .to_bytes();
    Ok((status, body))
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Start the MCP server process.
///
/// Phase 2 (Remote, Unix only): if the daemon socket is reachable, serve the
/// MCP tool surface over stdio backed by an HTTP-over-UDS client to the daemon
/// ([`MemoryMcpServer::with_remote`]) and return when the session ends.
///
/// Phase 1 (fallback): if the daemon is not running (or on Windows where the
/// UDS path is deferred to DR-121), serve MCP tool calls in-process using
/// [`MemoryMcpServer`].
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

    // Phase 2 (Unix only): if the daemon is reachable over UDS, serve the MCP
    // tool surface over stdio backed by an HTTP-over-UDS Remote client. The
    // daemon socket is a Unix domain socket; this path is intentionally absent
    // on Windows until DR-121 (TCP fallback) ships.
    #[cfg(unix)]
    match tokio::net::UnixStream::connect(&sock_path).await {
        Ok(_) => {
            // The connect only probes reachability; each tool call opens its
            // own short-lived connection. Derive the X-Agent-Root header from
            // the discovered project root, canonicalized to match
            // resolve_project's server-side lookup (registry roots are stored
            // canonicalized — see registry.rs). No `.agent/` in ancestry → send
            // the cwd; Remote calls will 404, which is the correct
            // "not registered" signal.
            let agent_root_header: String = match AgentRoot::discover(cwd) {
                Ok(root) => std::fs::canonicalize(root.project_root())
                    .unwrap_or_else(|_| root.project_root().to_path_buf())
                    .to_string_lossy()
                    .into_owned(),
                Err(_) => cwd.to_string_lossy().into_owned(),
            };
            eprintln!(
                "dreamd mcp: daemon reachable at {} — serving Phase 2 (Remote backend), agent root: {agent_root_header}",
                sock_path.display()
            );
            let svc = MemoryMcpServer::with_remote(sock_path, agent_root_header)
                .serve(rmcp::transport::stdio())
                .await
                .map_err(|e| McpRunError::Service(e.to_string()))?;
            svc.waiting()
                .await
                .map_err(|e| McpRunError::Service(e.to_string()))?;
            return Ok(());
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

    /// Serializes the two `resolve_sock_path` tests, which both mutate the
    /// process-global `DREAMD_SOCK` env var. Without this, Rust's parallel
    /// in-process test runner races them.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn resolve_sock_path_relative_env_returns_error() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("DREAMD_SOCK", "relative/path.sock");
        let result = resolve_sock_path();
        std::env::remove_var("DREAMD_SOCK");
        assert!(matches!(result, Err(McpRunError::InvalidSockPath(_))));
    }

    #[test]
    fn resolve_sock_path_absolute_env_passes() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("DREAMD_SOCK", "/tmp/test.sock");
        let result = resolve_sock_path();
        std::env::remove_var("DREAMD_SOCK");
        assert_eq!(result.unwrap(), PathBuf::from("/tmp/test.sock"));
    }

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

        let parsed: serde_json::Value = serde_json::from_str(text).expect("valid json");
        let results = parsed["results"].as_array().expect("results array");
        assert!(
            results.is_empty(),
            "empty index must return empty results; got: {parsed:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn search_nodes_seeded_index_returns_results() {
        use crate::server::tantivy_handle::{
            IndexerMsg, TantivyIndexHandle, DEFAULT_COMMIT_CADENCE,
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

            for (suffix, content) in [
                ('0', "rust async tokio channel"),
                ('1', "rust borrow checker lifetime"),
            ] {
                let raw_id = format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}");
                let id = EventId::parse(&raw_id).expect("valid event id");
                let learning = AgentLearning {
                    schema_version: "1.0.0".to_string(),
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

        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let results = parsed["results"].as_array().expect("results array");

        assert!(
            !results.is_empty(),
            "seeded index must return at least one result; got: {parsed:?}"
        );
        let score = results[0]["score"].as_f64().expect("score field");
        assert!(
            score > 0.0,
            "top result must have positive score; got {score}"
        );
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
                skill_action: "rust::test".to_string(),
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

    /// MCP Phase 1 `append_node` must redact secrets before persist (mirrors
    /// `server/http.rs::learn_content_redacted`).
    #[tokio::test]
    async fn append_node_content_redacted() {
        use crate::server::{Supervisor, COORDINATOR_CHANNEL_CAPACITY};
        use rmcp::handler::server::wrapper::Parameters;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = setup_agent_root(dir.path());
        let supervisor =
            Supervisor::start(&root, COORDINATOR_CHANNEL_CAPACITY, None).expect("start supervisor");
        let server = MemoryMcpServer::with_coordinator(root.clone(), supervisor.sender());

        let secret = "AKIAIOSFODNN7EXAMPLE";
        server
            .append_node(Parameters(AppendNodeParams {
                content: format!("key is {secret}"),
                source_harness: "test-harness".to_string(),
                skill_action: "rust::test".to_string(),
                pain: None,
                importance: None,
                client_dedup_key: None,
            }))
            .await
            .expect("append_node ok");

        let jsonl = std::fs::read_to_string(root.episodic_jsonl()).expect("read jsonl");
        let record: serde_json::Value =
            serde_json::from_str(jsonl.lines().next().expect("one line")).expect("parse record");
        assert!(
            !record["content"].as_str().unwrap().contains(secret),
            "secret must not appear on disk after redaction"
        );
        assert!(
            record["content"].as_str().unwrap().contains("[REDACTED]"),
            "REDACTED marker must be present"
        );
    }

    /// WEG-275: the `Backend::Local` ingress rejects a dotted/slashed
    /// `skill_action` (tightened charset) and an out-of-range score via
    /// `invalid_request` — both validated before the coordinator is touched.
    #[tokio::test]
    async fn append_node_rejects_invalid_skill_action_and_score() {
        use crate::server::{Supervisor, COORDINATOR_CHANNEL_CAPACITY};
        use rmcp::handler::server::wrapper::Parameters;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = setup_agent_root(dir.path());
        let supervisor =
            Supervisor::start(&root, COORDINATOR_CHANNEL_CAPACITY, None).expect("start supervisor");
        let server = MemoryMcpServer::with_coordinator(root, supervisor.sender());

        // Dotted skill_action — rejected by the tightened charset.
        let dotted = server
            .append_node(Parameters(AppendNodeParams {
                content: "c".to_string(),
                source_harness: "test".to_string(),
                skill_action: "rust.tokio".to_string(),
                pain: None,
                importance: None,
                client_dedup_key: None,
            }))
            .await;
        assert!(dotted.is_err(), "dotted skill_action must be rejected");

        // Out-of-range pain — rejected before the unwrap_or default.
        let bad_pain = server
            .append_node(Parameters(AppendNodeParams {
                content: "c".to_string(),
                source_harness: "test".to_string(),
                skill_action: "rust::tokio".to_string(),
                pain: Some(1e9),
                importance: None,
                client_dedup_key: None,
            }))
            .await;
        assert!(bad_pain.is_err(), "out-of-range pain must be rejected");
    }

    // ── WEG-259: Phase 2 Remote backend (HTTP-over-UDS) ──────────────────────

    /// Unit: `build_recall_request` sets GET, the percent-encoded URI, and the
    /// `X-Agent-Root` header. Pure — no socket, no async.
    #[cfg(unix)]
    #[test]
    fn test_build_recall_request_sets_correct_uri_method_and_header() {
        let req = build_recall_request("rust tokio", 5, "/home/u/project").expect("build");
        assert_eq!(req.method(), hyper::Method::GET);
        assert_eq!(
            req.uri().to_string(),
            "/api/v1/recall?q=rust%20tokio&k=5",
            "query must be percent-encoded"
        );
        assert_eq!(
            req.headers()
                .get("x-agent-root")
                .and_then(|v| v.to_str().ok()),
            Some("/home/u/project")
        );
    }

    /// Unit: `build_learn_request` sets POST, `Content-Type: application/json`,
    /// `X-Agent-Root`, `X-Client-Dedup-Key`, and a body that round-trips
    /// through `AgentLearning`. `#[tokio::test]` only to collect the in-memory
    /// body — there is no socket.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_build_learn_request_sets_correct_method_content_type_and_header() {
        let params = AppendNodeParams {
            content: "body content".to_string(),
            source_harness: "claude-code".to_string(),
            skill_action: "rust::cargo::test".to_string(),
            pain: Some(7.0),
            importance: Some(8.0),
            client_dedup_key: Some("dedup-1".to_string()),
        };
        let req = build_learn_request(&params, "/home/u/project").expect("build");

        assert_eq!(req.method(), hyper::Method::POST);
        assert_eq!(req.uri().to_string(), "/api/v1/learn");
        assert_eq!(
            req.headers()
                .get(hyper::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(
            req.headers()
                .get("x-agent-root")
                .and_then(|v| v.to_str().ok()),
            Some("/home/u/project")
        );
        assert_eq!(
            req.headers()
                .get("x-client-dedup-key")
                .and_then(|v| v.to_str().ok()),
            Some("dedup-1")
        );

        let body = req.into_body().collect().await.expect("collect").to_bytes();
        let parsed: AgentLearning =
            serde_json::from_slice(&body).expect("body round-trips as AgentLearning");
        assert_eq!(parsed.skill_action, "rust::cargo::test");
        assert_eq!(parsed.source_harness, "claude-code");
        assert_eq!(parsed.content, "body content");
    }

    /// Spawn an in-process daemon over UDS for the Remote integration tests.
    /// `with_coordinator` controls whether append writes are durable (real
    /// coordinator) or whether only read paths are exercised.
    #[cfg(unix)]
    async fn spawn_test_daemon(
        agent_root: &AgentRoot,
        registry_path: std::path::PathBuf,
        sock_path: &std::path::Path,
        with_coordinator: bool,
    ) {
        use crate::config::Config;
        use crate::server::index_map::{ProjectIndexMap, ProjectIndexMapConfig};
        use crate::server::uds_server::{bind_api_socket, serve_uds};
        use crate::server::{build_router, AppState};

        let supervisor = if with_coordinator {
            Supervisor::start(agent_root, COORDINATOR_CHANNEL_CAPACITY, None)
                .expect("start supervisor")
        } else {
            Supervisor::for_backpressure_test().0
        };
        let state = AppState::new(
            registry_path,
            supervisor,
            Config::default(),
            ProjectIndexMap::new(ProjectIndexMapConfig::default()),
            nix::unistd::Uid::current().as_raw(),
        );
        let router = build_router(state);
        let listener = bind_api_socket(sock_path).expect("bind api socket");
        tokio::spawn(async move {
            serve_uds(listener, router).await.ok();
        });
        // Let the accept loop start before the first client connect.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    /// Write a single-project registry pointing at `canonical_root`.
    #[cfg(unix)]
    fn write_registry(registry_path: &std::path::Path, canonical_root: &str) {
        std::fs::write(
            registry_path,
            format!("[[projects]]\nroot = \"{canonical_root}\"\n"),
        )
        .expect("write registry");
    }

    /// Integration: search_nodes over the Remote backend returns daemon results.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_remote_search_nodes_round_trips_via_uds() {
        use crate::server::tantivy_handle::{
            IndexerMsg, TantivyIndexHandle, DEFAULT_COMMIT_CADENCE,
        };
        use chrono::{DateTime, Utc};
        use tokio::sync::oneshot;

        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let root = setup_agent_root(&canonical);

        // Seed one learning into the on-disk Tantivy index, then release the
        // writer so the daemon's index_map can open its own handle.
        {
            let handle = TantivyIndexHandle::open(&root, DEFAULT_COMMIT_CADENCE).expect("open");
            let tx = handle.sender();
            let id = EventId::parse("evt_01ARZ3NDEKTSV4RRFFQ69G5FA0").expect("id");
            let learning = AgentLearning {
                schema_version: "1.0.0".to_string(),
                id: id.clone(),
                timestamp: DateTime::parse_from_rfc3339("2026-05-22T10:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                pain: 7.0,
                importance: 8.0,
                pinned: false,
                skill_action: "rust.tokio".to_string(),
                source_harness: "test-harness".to_string(),
                content: "rust async tokio channel".to_string(),
            };
            tx.send(IndexerMsg::Append {
                event_id: id,
                learning,
            })
            .await
            .expect("append");
            let (ack_tx, ack_rx) = oneshot::channel();
            tx.send(IndexerMsg::Flush { ack: ack_tx })
                .await
                .expect("flush send");
            ack_rx.await.expect("flush ack").expect("flush ok");
            drop(tx);
            handle.shutdown().await.expect("shutdown");
        }

        let canonical_str = canonical.to_string_lossy().into_owned();
        let registry_path = canonical.join("registry.toml");
        let sock_path = canonical.join("daemon.sock");
        write_registry(&registry_path, &canonical_str);
        spawn_test_daemon(&root, registry_path, &sock_path, false).await;

        let server = MemoryMcpServer::with_remote(sock_path, canonical_str);
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
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let results = parsed["results"].as_array().expect("results array");
        assert!(
            !results.is_empty(),
            "remote search must return results; got: {parsed:?}"
        );
        assert!(
            results[0]["content"]
                .as_str()
                .unwrap_or("")
                .contains("tokio"),
            "top result content must match seeded learning; got: {parsed:?}"
        );
    }

    /// Integration: append_node over the Remote backend writes durably and the
    /// daemon mints the EventId.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_remote_append_node_round_trips_via_uds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let root = setup_agent_root(&canonical);

        let canonical_str = canonical.to_string_lossy().into_owned();
        let registry_path = canonical.join("registry.toml");
        let sock_path = canonical.join("daemon.sock");
        write_registry(&registry_path, &canonical_str);
        spawn_test_daemon(&root, registry_path, &sock_path, true).await;

        let server = MemoryMcpServer::with_remote(sock_path, canonical_str);
        let result = server
            .append_node(Parameters(AppendNodeParams {
                content: "remote append content".to_string(),
                source_harness: "test-harness".to_string(),
                skill_action: "rust::remote".to_string(),
                pain: Some(6.0),
                importance: Some(7.0),
                client_dedup_key: None,
            }))
            .await
            .expect("append_node ok");

        let text = result
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.clone()))
            .expect("text content present");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid json");
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

        // The learning must be durably on disk in the episodic JSONL.
        let jsonl = std::fs::read_to_string(root.episodic_jsonl()).expect("read jsonl");
        let lines: Vec<&str> = jsonl.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            1,
            "exactly one learning persisted; got: {jsonl:?}"
        );
        let record: serde_json::Value =
            serde_json::from_str(lines[0]).expect("record is valid json");
        assert_eq!(record["content"].as_str().unwrap(), "remote append content");
        assert_eq!(record["skill_action"].as_str().unwrap(), "rust::remote");
    }

    /// Integration: an unregistered agent root yields a 404 from the daemon,
    /// which the Remote backend surfaces as `invalid_request` (not a dropped
    /// connection).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_remote_unregistered_root_returns_invalid_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let root = setup_agent_root(&canonical);

        // registry.toml is never written → resolve_project returns Ok(None) → 404.
        let registry_path = canonical.join("registry.toml");
        let sock_path = canonical.join("daemon.sock");
        spawn_test_daemon(&root, registry_path, &sock_path, false).await;

        let server =
            MemoryMcpServer::with_remote(sock_path, canonical.to_string_lossy().into_owned());
        let err = server
            .search_nodes(Parameters(SearchNodesParams {
                query: "anything".to_string(),
                k: Some(3),
            }))
            .await
            .expect_err("unregistered root must error");

        assert_eq!(
            err.code,
            rmcp::model::ErrorCode::INVALID_REQUEST,
            "404 must map to invalid_request; got: {err:?}"
        );
        assert!(
            err.message.contains("not registered"),
            "message must mention the project is not registered; got: {}",
            err.message
        );
    }
}
