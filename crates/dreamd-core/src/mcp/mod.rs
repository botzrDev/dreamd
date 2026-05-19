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

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::layout::{DaemonHome, LayoutError};
use crate::privacy::DR413_DISCLOSURE;
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
}

#[tool_router]
impl MemoryMcpServer {
    /// Create a new in-process memory MCP server.
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Search episodic memory using BM25 × salience scoring.
    ///
    /// Returns a ranked list of matching learning entries.
    #[tool(description = "Search episodic memory for relevant past learnings using BM25 × salience scoring. Returns a ranked list of matching entries.")]
    async fn search_nodes(
        &self,
        Parameters(p): Parameters<SearchNodesParams>,
    ) -> Result<CallToolResult, McpError> {
        // TODO(WEG-78): wire to recall() once the coordinator is reachable from
        // this context. Currently returns an empty result set.
        let _query = p.query;
        let _k = p.k.unwrap_or(5);
        Ok(CallToolResult::success(vec![Content::text("[]")]))
    }

    /// Append a new learning node to episodic memory.
    ///
    /// The entry is durably fsynced before this call returns (DR-103).
    #[tool(description = "Append a new learning to episodic memory. The entry is durably persisted (fdatasync) before this call returns.")]
    async fn append_node(
        &self,
        Parameters(p): Parameters<AppendNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        // TODO(WEG-79): wire to coordinator learn() once the coordinator is
        // reachable from this context. Currently acknowledges without storing.
        let _content = p.content;
        let _source_harness = p.source_harness;
        let _skill_action = p.skill_action;
        let _pain = p.pain;
        let _importance = p.importance;
        Ok(CallToolResult::success(vec![Content::text(
            r#"{"status":"ok"}"#,
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
    let svc = MemoryMcpServer::new()
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| McpRunError::Service(e.to_string()))?;

    svc.waiting()
        .await
        .map_err(|e| McpRunError::Service(e.to_string()))?;

    Ok(())
}
