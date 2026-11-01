//! One memory-store interface, two adapters (BZR-173).
//!
//! [`MemoryStore`] is the seam every MCP tool call goes through: `search_nodes`
//! calls [`MemoryStore::recall_json`], `append_node` calls
//! [`MemoryStore::append`]. Two adapters implement it:
//!
//! * [`InProcessStore`] — the in-process path. Recall reads the project's Tantivy
//!   index through the handle pinned at construction — the one the coordinator
//!   indexes into — or, with none pinned, the session's shared
//!   `ProjectIndexMap` (one handle per root, never a per-call open; WEG-252).
//!   Append goes through
//!   [`LearnIngress::build_agent_learning`] and then `coordinator_tx.send().await`
//!   with no timeout.
//! * [`DaemonStore`] — the daemon proxy. HTTP-over-UDS on Unix (WEG-259),
//!   loopback TCP + `Authorization: Bearer` off Unix (AILAB-192). Same requests,
//!   same `X-Agent-Root` contract, same per-call connect on both transports.
//!
//! The HTTP handlers are not adapters. `GET /api/v1/recall` keeps its reader
//! pinned to the primary handle through `AppState::with_index_handle` and
//! shares only `recall_to_json`. `POST /api/v1/learn` keeps
//! `Supervisor::try_send` and its 503 on a full inbox; it shares only
//! [`LearnIngress`] and [`LearnResponse::from_append_outcome`].
//!
//! Wire shapes stay single-sited in [`crate::ingress`]: the recall envelope is
//! [`RecallIngress::to_json`], the learn body is [`LearnResponse`], and the
//! default `k` is [`crate::ingress::DEFAULT_RECALL_K`].

#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;

use bytes::Bytes;
use http_body_util::Full;
use tokio::sync::mpsc;

use crate::config::load_config;
use crate::coordinator::{CoordinatorError, MemoryCoordinatorMsg, MAX_LEARNING_LINE_BYTES};
use crate::ingress::{
    query_too_long_message, LearnIngress, LearnResponse, RecallIngress, MAX_RECALL_QUERY_BYTES,
};
use crate::recurrence::RecallReader;
use crate::AgentRoot;

#[cfg(not(unix))]
use crate::daemon_client::{AuthToken, DaemonEndpoint};
#[cfg(unix)]
use crate::server::index_map::{ProjectIndexMap, ProjectIndexMapConfig};
#[cfg(unix)]
use crate::server::tantivy_handle::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};
#[cfg(unix)]
use crate::server::AppendMark;
#[cfg(unix)]
use crate::writer_lock::{WriterKind, WriterLock};

/// The empty recall envelope, returned when there is nothing readable.
const EMPTY_RESULTS: &str = r#"{"results":[]}"#;

/// Errors a [`MemoryStore`] call can return.
///
/// Three kinds, one per MCP error code the tool methods surface (BZR-1301):
/// arguments the caller can fix (`InvalidParams`, `-32602`), a request the
/// project's state refuses (`InvalidRequest`, `-32600`), and everything else
/// (`Internal`, `-32603`). The `Display` is the message alone, so the MCP
/// error carries it unchanged.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MemoryStoreError {
    /// The tool arguments are wrong: a bad `skill_action`, a score outside
    /// `0.0..=10.0`, a learning or query over its size limit, a query the
    /// parser rejects, or any other `400` the daemon explains. MCP `-32602`.
    #[error("{0}")]
    InvalidParams(String),
    /// An unregistered project, a rejected content type, or an index that
    /// cannot be opened or searched. MCP `-32600`.
    #[error("{0}")]
    InvalidRequest(String),
    /// Coordinator, transport, or daemon failure. MCP `-32603`.
    #[error("{0}")]
    Internal(String),
}

impl MemoryStoreError {
    fn params(msg: impl Into<String>) -> Self {
        Self::InvalidParams(msg.into())
    }

    fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidRequest(msg.into())
    }

    fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
}

/// One append, as either adapter receives it.
///
/// `pain` / `importance` are optional because MCP has always defaulted them
/// (to `5.0`, inside [`LearnIngress`]). `pinned` is carried for parity with
/// the HTTP wire body; MCP has no pinned argument and always sends `false`.
#[derive(Debug, Clone, PartialEq)]
pub struct AppendRequest {
    pub content: String,
    pub source_harness: String,
    pub skill_action: String,
    pub pain: Option<f64>,
    pub importance: Option<f64>,
    pub client_dedup_key: Option<String>,
    pub pinned: bool,
}

/// Recall and append behind one interface.
#[async_trait::async_trait]
pub trait MemoryStore: Send + Sync {
    /// Recall up to `k` hits for `query`, as the canonical
    /// `{"results":[...]}` JSON string.
    async fn recall_json(&self, query: &str, k: u32) -> Result<String, MemoryStoreError>;

    /// Durably append one learning.
    async fn append(&self, req: AppendRequest) -> Result<LearnResponse, MemoryStoreError>;
}

/// Run a BM25 × salience recall on `reader` and serialize it to the canonical
/// `{"results":[...]}` string. The single recall body shared by the in-process
/// adapter and `GET /api/v1/recall`; each caller chooses its own reader.
pub(crate) fn recall_to_json(
    reader: &RecallReader,
    query: &str,
    k: u32,
    now_sec: i64,
) -> tantivy::Result<String> {
    let (_, schema_fields) = crate::index::build_schema();
    let results = crate::recall(reader, &schema_fields, query, k as usize, now_sec)?;
    Ok(RecallIngress::to_json(results))
}

// In-process adapter

/// The per-session Tantivy handle map an MCP server shares with its
/// [`InProcessStore`].
#[cfg(unix)]
pub(crate) type SharedIndexMap = Arc<Mutex<ProjectIndexMap<TantivyIndexHandle>>>;

/// A fresh, empty [`SharedIndexMap`].
#[cfg(unix)]
pub(crate) fn new_index_map() -> SharedIndexMap {
    Arc::new(Mutex::new(ProjectIndexMap::new(
        ProjectIndexMapConfig::default(),
    )))
}

/// In-process store: reads the on-disk index, writes through a local
/// coordinator.
///
/// | `agent_root` | `coordinator_tx` | recall            | append                 |
/// |--------------|------------------|-------------------|------------------------|
/// | `None`       | —                | `{"results":[]}`  | "no agent root" error  |
/// | `Some`       | `None`           | index read        | "no agent root" error  |
/// | `Some`       | `Some`           | index read        | `send().await`         |
///
/// With a [`PinnedIndex`], recall first commits the appends its coordinator
/// has made that the index has not committed, waiting at most one second
/// (BZR-1437). So a search finds every append acked before it.
///
/// The pinned index and the index map are Unix fields. Windows `dreamd mcp`
/// does not construct this store; it proxies to `dreamd watch`.
pub struct InProcessStore {
    agent_root: Option<AgentRoot>,
    coordinator_tx: Option<mpsc::Sender<MemoryCoordinatorMsg>>,
    /// The handle the coordinator indexes into, when there is one. Recall reads
    /// it before `index_map` and it is never evicted — the `AppState::primary`
    /// pattern.
    #[cfg(unix)]
    pinned: Option<PinnedIndex>,
    /// Shared with the owning MCP server. One IndexWriter per root per session
    /// (WEG-252); never open an index per recall. Used only with no `pinned`.
    #[cfg(unix)]
    index_map: SharedIndexMap,
}

/// The index a coordinator feeds, with that coordinator's append mark
/// (BZR-1437). Built by `MemoryMcpServer::with_coordinator_and_index`.
#[cfg(unix)]
pub struct PinnedIndex {
    pub(crate) index: Arc<TantivyIndexHandle>,
    /// What the coordinator has appended, counted before each append is acked.
    pub(crate) appends: Arc<Mutex<AppendMark>>,
}

impl InProcessStore {
    /// Build an in-process store. `pinned` is the index the coordinator's
    /// indexer writes to (`dreamd mcp`'s topology); with `None`, recall opens a
    /// handle lazily in `index_map`, the MCP session's map.
    pub fn new(
        agent_root: Option<AgentRoot>,
        coordinator_tx: Option<mpsc::Sender<MemoryCoordinatorMsg>>,
        #[cfg(unix)] pinned: Option<PinnedIndex>,
        #[cfg(unix)] index_map: SharedIndexMap,
    ) -> Self {
        Self {
            agent_root,
            coordinator_tx,
            #[cfg(unix)]
            pinned,
            #[cfg(unix)]
            index_map,
        }
    }

    /// Synchronous half of Unix recall. Reads the pinned handle when there is
    /// one. Otherwise the map lock is released before the search runs (the
    /// reader is an `Arc`-backed clone), and never lives across an `.await`; a
    /// map miss takes its own project writer lock for the open.
    #[cfg(unix)]
    fn recall_local(
        &self,
        root: &AgentRoot,
        query: &str,
        k: u32,
    ) -> Result<String, MemoryStoreError> {
        let reader = if let Some(pinned) = &self.pinned {
            pinned.index.reader().clone()
        } else {
            let mut map = self
                .index_map
                .lock()
                .map_err(|_| MemoryStoreError::internal("index map lock poisoned"))?;
            let handle = map
                .get_or_open(root.project_root(), |p| {
                    let root = AgentRoot::new(p.to_path_buf());
                    let lock = WriterLock::acquire(&root, WriterKind::Mcp).map_err(|e| {
                        crate::server::index_map::IndexError::WriterLock(e.to_string())
                    })?;
                    TantivyIndexHandle::open(&root, &lock, DEFAULT_COMMIT_CADENCE)
                })
                .map_err(|e| MemoryStoreError::invalid(format!("index open failed: {e}")))?;
            handle.touch();
            handle.reader().clone()
        };
        let now_sec = chrono::Utc::now().timestamp();
        recall_to_json(&reader, query, k, now_sec).map_err(|e| match e {
            // tantivy turns every query-parser error into `InvalidArgument`:
            // the caller's to fix, as `get_recall`'s 400 says in daemon mode.
            e @ tantivy::TantivyError::InvalidArgument(_) => {
                MemoryStoreError::params(format!("recall failed: {e}"))
            }
            e => MemoryStoreError::invalid(format!("recall failed: {e}")),
        })
    }
}

#[async_trait::async_trait]
impl MemoryStore for InProcessStore {
    async fn recall_json(&self, query: &str, k: u32) -> Result<String, MemoryStoreError> {
        // The same limit the daemon and its client apply (BZR-1301), so one
        // query gets one answer in both modes.
        if query.len() > MAX_RECALL_QUERY_BYTES {
            return Err(MemoryStoreError::params(query_too_long_message()));
        }
        let Some(root) = &self.agent_root else {
            return Ok(EMPTY_RESULTS.to_string());
        };
        #[cfg(unix)]
        {
            // BZR-1437: commit the appends acked before this recall, if the
            // index has not, so the search sees them. Bounded; past the wait
            // it searches the last commit.
            if let Some(pinned) = &self.pinned {
                let appended = pinned
                    .appends
                    .lock()
                    .map_err(|_| MemoryStoreError::internal("append mark lock poisoned"))?
                    .appended;
                pinned.index.recall_flusher().commit_pending(appended).await;
            }
            self.recall_local(root, query, k)
        }
        #[cfg(not(unix))]
        {
            let _ = (root, query, k);
            Ok(EMPTY_RESULTS.to_string())
        }
    }

    async fn append(&self, req: AppendRequest) -> Result<LearnResponse, MemoryStoreError> {
        let (Some(agent_root), Some(coordinator_tx)) = (&self.agent_root, &self.coordinator_tx)
        else {
            return Err(MemoryStoreError::internal(
                "coordinator unavailable: no agent root found",
            ));
        };

        let config = load_config(agent_root.project_root())
            .map_err(|e| MemoryStoreError::internal(format!("config load failed: {e}")))?;
        // Sole skill_action gate for the in-process path (ARCHITECTURE.md §9).
        let mut learning = LearnIngress::build_agent_learning(
            &req.content,
            &req.source_harness,
            &req.skill_action,
            req.pain,
            req.importance,
            config.redaction,
        )
        .map_err(|e| MemoryStoreError::params(e.to_string()))?;
        learning.pinned = req.pinned;

        // `send().await`, never `Supervisor::try_send`: the in-process path
        // waits for inbox room rather than shedding with a 503.
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        coordinator_tx
            .send(MemoryCoordinatorMsg::AppendLearning {
                learning,
                client_dedup_key: req.client_dedup_key,
                response_tx: resp_tx,
            })
            .await
            .map_err(|_| MemoryStoreError::internal("coordinator unavailable"))?;

        let outcome = resp_rx
            .await
            .map_err(|_| MemoryStoreError::internal("coordinator dropped"))?
            .map_err(|e| match e {
                CoordinatorError::PayloadTooLarge { .. } => {
                    MemoryStoreError::params(learning_too_large_message())
                }
                other => MemoryStoreError::internal(other.to_string()),
            })?;

        Ok(LearnResponse::from_append_outcome(&outcome))
    }
}

// Daemon adapter

/// Daemon-proxy store: every call is one HTTP request to the running daemon.
///
/// On Unix the transport is HTTP-over-UDS to `sock_path`, authenticated by
/// `SO_PEERCRED` on the server side. Off Unix it is HTTP/1 over loopback TCP
/// to an `endpoint` that `read_server_json` has already proven loopback, with
/// a bearer `token` the transport inserts. The token is never formatted into
/// an error message.
///
/// `agent_root_header` is the canonicalized project-root string sent as
/// `X-Agent-Root` (matches `resolve_project`'s server-side lookup).
pub struct DaemonStore {
    #[cfg(unix)]
    sock_path: PathBuf,
    #[cfg(not(unix))]
    endpoint: DaemonEndpoint,
    #[cfg(not(unix))]
    token: AuthToken,
    agent_root_header: String,
}

impl DaemonStore {
    /// Unix: proxy over the daemon's UDS socket.
    #[cfg(unix)]
    pub fn uds(sock_path: PathBuf, agent_root_header: String) -> Self {
        Self {
            sock_path,
            agent_root_header,
        }
    }

    /// Off Unix: proxy over loopback TCP with a bearer credential.
    #[cfg(not(unix))]
    pub fn tcp(endpoint: DaemonEndpoint, token: AuthToken, agent_root_header: String) -> Self {
        Self {
            endpoint,
            token,
            agent_root_header,
        }
    }

    async fn send(
        &self,
        req: hyper::Request<Full<Bytes>>,
    ) -> Result<(hyper::StatusCode, Bytes), MemoryStoreError> {
        #[cfg(unix)]
        {
            send_remote(&self.sock_path, req).await
        }
        #[cfg(not(unix))]
        {
            send_remote_tcp(&self.endpoint, &self.token, req).await
        }
    }
}

#[async_trait::async_trait]
impl MemoryStore for DaemonStore {
    async fn recall_json(&self, query: &str, k: u32) -> Result<String, MemoryStoreError> {
        let req = build_recall_request(query, k, &self.agent_root_header)?;
        let (status, body) = self.send(req).await?;
        if status.is_success() {
            // The daemon already returns the canonical {"results":[...]} shape;
            // forward it verbatim.
            Ok(String::from_utf8_lossy(&body).into_owned())
        } else {
            Err(map_remote_status_error(status, &body))
        }
    }

    async fn append(&self, req: AppendRequest) -> Result<LearnResponse, MemoryStoreError> {
        let http_req = build_learn_request(&req, &self.agent_root_header)?;
        let (status, body) = self.send(http_req).await?;
        if status.is_success() {
            // The daemon serialized this with `LearnResponse` too, so the
            // round trip is lossless.
            serde_json::from_slice(&body).map_err(|e| {
                MemoryStoreError::internal(format!("daemon learn response parse failed: {e}"))
            })
        } else {
            Err(map_remote_status_error(status, &body))
        }
    }
}

/// Percent-encode a query-string value (RFC 3986 unreserved set passes
/// through; everything else becomes `%XX`). Inlined rather than pulling a
/// URL-encoding crate: the daemon proxy depends only on hyper, http-body-util
/// and bytes.
///
/// Cfg-free: byte arithmetic on a string, shared by both remote transports.
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
/// header. Pure (no socket, no async) so it can be unit-tested directly, and
/// cfg-free so the UDS and loopback-TCP transports send byte-identical requests.
///
/// A query over [`MAX_RECALL_QUERY_BYTES`] is refused before anything is
/// built, with the daemon's `414` message (BZR-1301). Below that limit the
/// encoded URI always fits the `http` crate's 65,534 bytes.
pub(crate) fn build_recall_request(
    query: &str,
    k: u32,
    agent_root_header: &str,
) -> Result<hyper::Request<Full<Bytes>>, MemoryStoreError> {
    if query.len() > MAX_RECALL_QUERY_BYTES {
        return Err(MemoryStoreError::params(query_too_long_message()));
    }
    let uri = format!("/api/v1/recall?q={}&k={k}", percent_encode_query(query));
    hyper::Request::builder()
        .method(hyper::Method::GET)
        .uri(uri)
        .header(hyper::header::HOST, "localhost")
        .header("x-agent-root", agent_root_header)
        .body(Full::new(Bytes::new()))
        .map_err(|e| MemoryStoreError::internal(format!("build recall request failed: {e}")))
}

/// Build `POST /api/v1/learn` with `X-Agent-Root`, `Content-Type:
/// application/json`, an optional `X-Client-Dedup-Key`, and a JSON body built
/// by [`LearnIngress::placeholder_learning`] — the same placeholder
/// constructor the in-process path uses, so `schema_version` is
/// `RECORD_SCHEMA_VERSION` and the `EventId` is the shared placeholder.
///
/// `pain` and `importance` are checked here first, with the shared
/// `LearnIngress` validators, because the placeholder stores them as `f32`:
/// the cast would turn `1e40` into infinity, which serializes as `null`, and
/// round `10.0000001` to `10.0` (BZR-1301). The daemon mints the real `id`
/// and `timestamp`, and re-runs every `LearnIngress` check (skill_action,
/// score range, redaction) on arrival, so the raw skill_action and content
/// are forwarded as given. Pure: unit-tested directly, and cfg-free — see
/// [`build_recall_request`].
pub(crate) fn build_learn_request(
    req: &AppendRequest,
    agent_root_header: &str,
) -> Result<hyper::Request<Full<Bytes>>, MemoryStoreError> {
    LearnIngress::validate_optional_pain(req.pain)
        .and_then(|()| LearnIngress::validate_optional_importance(req.importance))
        .map_err(|e| MemoryStoreError::params(e.to_string()))?;
    let mut learning = LearnIngress::placeholder_learning(
        &req.content,
        &req.source_harness,
        &req.skill_action,
        req.pain,
        req.importance,
    );
    learning.pinned = req.pinned;
    let body = serde_json::to_vec(&learning)
        .map_err(|e| MemoryStoreError::internal(format!("serialize learning failed: {e}")))?;

    let mut builder = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri("/api/v1/learn")
        .header(hyper::header::HOST, "localhost")
        .header("x-agent-root", agent_root_header)
        .header(hyper::header::CONTENT_TYPE, "application/json");
    if let Some(key) = req.client_dedup_key.as_deref() {
        builder = builder.header("x-client-dedup-key", key);
    }
    builder
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| MemoryStoreError::internal(format!("build learn request failed: {e}")))
}

/// Label for a `400` whose body is not the daemon's `{"error": …}` envelope.
const DAEMON_REJECTED_400: &str = "daemon rejected the request (HTTP 400)";

/// The `404` message. The daemon's only `404` on these routes is a registry
/// miss.
const NOT_REGISTERED: &str = "project not registered with daemon; run dreamd init";

/// The one message for a learning over the JSONL line cap, in both modes
/// (BZR-1301): the daemon client's `413` arm and the in-process store's
/// `PayloadTooLarge`.
pub(crate) fn learning_too_large_message() -> String {
    format!("learning exceeds {MAX_LEARNING_LINE_BYTES} bytes as a JSONL line; shorten content")
}

/// The daemon's own message: `Some` only when `body` is a JSON object whose
/// `error` member is a string that is not blank. The string is returned as
/// is. Parsed through `Value` because a derived struct would also accept a
/// JSON array.
fn daemon_error_message(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let msg = value.get("error")?.as_str()?;
    (!msg.trim().is_empty()).then(|| msg.to_owned())
}

/// Map a non-success daemon HTTP status to a [`MemoryStoreError`]. Never
/// panics. Cfg-free: one status table for both transports, so a Windows
/// client and a Unix client explain the same 404 the same way (BZR-1301).
///
/// | Status | `message` | Kind |
/// |---|---|---|
/// | 400 | the daemon's `error` string, or [`DAEMON_REJECTED_400`] for any other body | `InvalidParams` |
/// | 404 | [`NOT_REGISTERED`] | `InvalidRequest` |
/// | 413 | [`learning_too_large_message`] | `InvalidParams` |
/// | 414 | [`query_too_long_message`] | `InvalidParams` |
/// | 415 | `content-type rejected (defensive)` | `InvalidRequest` |
/// | other | `daemon error {status}: {body}` | `Internal` |
///
/// 413 comes only from learn and 414 only from recall, and hyper's own `414`
/// has an empty body, so both ignore the body.
fn map_remote_status_error(status: hyper::StatusCode, body: &Bytes) -> MemoryStoreError {
    match status.as_u16() {
        400 => MemoryStoreError::params(
            daemon_error_message(body).unwrap_or_else(|| DAEMON_REJECTED_400.to_owned()),
        ),
        404 => MemoryStoreError::invalid(NOT_REGISTERED),
        413 => MemoryStoreError::params(learning_too_large_message()),
        414 => MemoryStoreError::params(query_too_long_message()),
        415 => MemoryStoreError::invalid("content-type rejected (defensive)"),
        _ => MemoryStoreError::internal(format!(
            "daemon error {status}: {}",
            String::from_utf8_lossy(body)
        )),
    }
}

/// Map a transport failure to the stage that failed. Never includes a token.
fn map_transport_error(e: crate::daemon_client::DaemonTransportError) -> MemoryStoreError {
    use crate::daemon_client::DaemonTransportError;
    let stage = match e {
        DaemonTransportError::Unreachable | DaemonTransportError::Connect(_) => "connect",
        DaemonTransportError::Handshake(_) => "handshake",
        DaemonTransportError::Send(_) => "request",
        DaemonTransportError::ReadBody(_) => "response read",
    };
    MemoryStoreError::internal(format!("daemon {stage} failed: {e:?}"))
}

/// Open a fresh HTTP-over-UDS connection to the daemon, send one request, and
/// collect the response into `(status, body)`. Per-call connect (no pooling):
/// MCP tool calls are infrequent, so a pool would hold idle connections for
/// nothing (WEG-78-A).
#[cfg(unix)]
async fn send_remote(
    sock_path: &Path,
    req: hyper::Request<Full<Bytes>>,
) -> Result<(hyper::StatusCode, Bytes), MemoryStoreError> {
    crate::daemon_client::send_one(sock_path, req)
        .await
        .map_err(map_transport_error)
}

/// Loopback-TCP sibling of `send_remote` (AILAB-192). The credential is
/// inserted by [`crate::daemon_client::send_one_tcp`], not here, so no
/// request-building site can forget it.
#[cfg(not(unix))]
async fn send_remote_tcp(
    endpoint: &DaemonEndpoint,
    token: &AuthToken,
    req: hyper::Request<Full<Bytes>>,
) -> Result<(hyper::StatusCode, Bytes), MemoryStoreError> {
    crate::daemon_client::send_one_tcp(endpoint, token, req)
        .await
        .map_err(map_transport_error)
}

/// A store behind an `Arc`, as the MCP server holds it.
pub type SharedMemoryStore = Arc<dyn MemoryStore>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what it was given and answers with canned values. No daemon,
    /// no index, no coordinator.
    struct StubStore {
        recall_body: String,
        append_response: (String, String, bool),
        seen_query: std::sync::Mutex<Option<(String, u32)>>,
        seen_append: std::sync::Mutex<Option<AppendRequest>>,
    }

    #[async_trait::async_trait]
    impl MemoryStore for StubStore {
        async fn recall_json(&self, query: &str, k: u32) -> Result<String, MemoryStoreError> {
            *self.seen_query.lock().unwrap() = Some((query.to_string(), k));
            Ok(self.recall_body.clone())
        }

        async fn append(&self, req: AppendRequest) -> Result<LearnResponse, MemoryStoreError> {
            *self.seen_append.lock().unwrap() = Some(req);
            let (id, timestamp, deduplicated) = self.append_response.clone();
            Ok(LearnResponse {
                id,
                timestamp,
                deduplicated,
            })
        }
    }

    #[tokio::test]
    async fn stub_store_round_trips_recall_and_append_through_the_trait() {
        let stub = Arc::new(StubStore {
            recall_body: r#"{"results":[{"content":"stub hit"}]}"#.to_string(),
            append_response: (
                "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
                "2026-09-24T00:00:00+00:00".to_string(),
                false,
            ),
            seen_query: std::sync::Mutex::new(None),
            seen_append: std::sync::Mutex::new(None),
        });
        let store: SharedMemoryStore = stub.clone();

        let body = store.recall_json("rust tokio", 3).await.expect("recall");
        assert_eq!(body, r#"{"results":[{"content":"stub hit"}]}"#);
        assert_eq!(
            *stub.seen_query.lock().unwrap(),
            Some(("rust tokio".to_string(), 3))
        );

        let req = AppendRequest {
            content: "stub content".to_string(),
            source_harness: "test-harness".to_string(),
            skill_action: "rust::test".to_string(),
            pain: Some(6.0),
            importance: None,
            client_dedup_key: Some("dedup-1".to_string()),
            pinned: false,
        };
        let resp = store.append(req.clone()).await.expect("append");
        assert_eq!(resp.id, "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(resp.timestamp, "2026-09-24T00:00:00+00:00");
        assert!(!resp.deduplicated);
        assert_eq!(*stub.seen_append.lock().unwrap(), Some(req));
    }

    // ── BZR-1301: daemon status map and the client's checks ─────────────────

    fn status(code: u16) -> hyper::StatusCode {
        hyper::StatusCode::from_u16(code).expect("valid status")
    }

    fn map(code: u16, body: &[u8]) -> MemoryStoreError {
        map_remote_status_error(status(code), &Bytes::copy_from_slice(body))
    }

    /// Test 1: a 400 carries the daemon's own `error` string, unchanged.
    #[test]
    fn status_400_forwards_the_daemon_error_string() {
        assert_eq!(
            map(
                400,
                br#"{"error":"invalid skill_action: segment contains characters outside [a-z0-9_]"}"#
            ),
            MemoryStoreError::InvalidParams(
                "invalid skill_action: segment contains characters outside [a-z0-9_]".to_string()
            )
        );
        assert_eq!(
            map(400, br#"{"error":" padded "}"#),
            MemoryStoreError::InvalidParams(" padded ".to_string()),
            "the string is returned as is, not trimmed"
        );
    }

    /// Test 2: the score and header 400s are forwarded the same way.
    #[test]
    fn status_400_forwards_score_and_header_strings() {
        for msg in [
            "pain must be in 0.0..=10.0",
            "missing or non-UTF-8 X-Agent-Root header",
        ] {
            let body = serde_json::json!({ "error": msg }).to_string();
            assert_eq!(
                map(400, body.as_bytes()),
                MemoryStoreError::InvalidParams(msg.to_string())
            );
        }
    }

    /// Test 3: any other 400 body gets the generic label, which never names
    /// the header.
    #[test]
    fn status_400_other_bodies_get_the_generic_label() {
        let bodies: [&[u8]; 8] = [
            b"",
            b"not json",
            br#"{"error":5}"#,
            br#"{"error":""}"#,
            br#"{"error":"   "}"#,
            br#"["error"]"#,
            br#"{"message":"x"}"#,
            b"Failed to parse the request body as JSON: expected value at line 1 column 1",
        ];
        for body in bodies {
            let err = map(400, body);
            assert_eq!(
                err,
                MemoryStoreError::InvalidParams(
                    "daemon rejected the request (HTTP 400)".to_string()
                ),
                "body {:?}",
                String::from_utf8_lossy(body)
            );
            assert!(!err.to_string().contains("X-Agent-Root"));
        }
    }

    /// Test 4: a 404 keeps the `dreamd init` hint and drops the raw body.
    #[test]
    fn status_404_keeps_the_init_hint() {
        for body in [br#"{"error":"agent root not registered"}"#.as_slice(), b""] {
            assert_eq!(
                map(404, body),
                MemoryStoreError::InvalidRequest(
                    "project not registered with daemon; run dreamd init".to_string()
                )
            );
        }
    }

    /// Test 5: a 413, from the coordinator or from axum's body limit, is one
    /// fixed message. The only test that spells the line cap.
    #[test]
    fn status_413_is_the_learning_too_large_message() {
        for body in [
            br#"{"error":"payload too large"}"#.as_slice(),
            b"Failed to buffer the request body: length limit exceeded",
        ] {
            assert_eq!(
                map(413, body),
                MemoryStoreError::InvalidParams(learning_too_large_message())
            );
        }
        assert_eq!(
            learning_too_large_message(),
            "learning exceeds 4096 bytes as a JSONL line; shorten content"
        );
    }

    /// Test 6: a 414, with hyper's empty body or the daemon's JSON, is the
    /// shared query message.
    #[test]
    fn status_414_is_the_query_too_long_message() {
        let daemon_body = serde_json::json!({ "error": query_too_long_message() }).to_string();
        for body in [b"".as_slice(), daemon_body.as_bytes()] {
            assert_eq!(
                map(414, body),
                MemoryStoreError::InvalidParams(query_too_long_message())
            );
        }
    }

    /// Test 7: the arms this ticket does not change.
    #[test]
    fn unchanged_arms_keep_their_kind() {
        assert_eq!(
            map(415, b""),
            MemoryStoreError::InvalidRequest("content-type rejected (defensive)".to_string())
        );
        for code in [401, 403, 422, 500, 503] {
            assert!(
                matches!(map(code, b"x"), MemoryStoreError::Internal(_)),
                "{code} must stay Internal"
            );
        }
        assert_eq!(
            map(500, br#"{"error":"recall failed: x"}"#),
            MemoryStoreError::Internal(
                r#"daemon error 500 Internal Server Error: {"error":"recall failed: x"}"#
                    .to_string()
            )
        );
    }

    /// Test 8: a query at the limit builds, and its URI fits the `http`
    /// crate's 65,534 bytes even when every byte is percent-encoded.
    #[test]
    fn build_recall_request_at_the_limit() {
        let ascii = "a".repeat(MAX_RECALL_QUERY_BYTES);
        let encoded = "é".repeat(MAX_RECALL_QUERY_BYTES / 2);
        assert_eq!(encoded.len(), MAX_RECALL_QUERY_BYTES);
        for query in [ascii, encoded] {
            let req = build_recall_request(&query, u32::MAX, "/home/u/project")
                .expect("a query at the limit builds");
            assert!(req.uri().to_string().len() <= 65_534);
        }
    }

    /// Test 9: a query over the limit is refused before anything is built,
    /// and never with the `http` crate's "uri too long".
    #[test]
    fn build_recall_request_over_the_limit() {
        let over_chars = "é".repeat(MAX_RECALL_QUERY_BYTES / 2 + 1);
        assert_eq!(over_chars.len(), MAX_RECALL_QUERY_BYTES + 2);
        for query in [
            "a".repeat(MAX_RECALL_QUERY_BYTES + 1),
            over_chars,
            "word ".repeat(20_480),
        ] {
            let err = build_recall_request(&query, 5, "/home/u/project")
                .expect_err("a query over the limit is refused");
            assert_eq!(
                err,
                MemoryStoreError::InvalidParams(query_too_long_message())
            );
            assert!(!err.to_string().to_lowercase().contains("uri too long"));
        }

        // A builder failure that is not the caller's input stays Internal.
        match build_recall_request("q", 5, "/tmp/a\nb") {
            Err(MemoryStoreError::Internal(m)) => {
                assert!(m.starts_with("build recall request failed: "), "{m}")
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    fn learn(skill_action: &str, pain: Option<f64>, importance: Option<f64>) -> AppendRequest {
        AppendRequest {
            content: "c".to_string(),
            source_harness: "test".to_string(),
            skill_action: skill_action.to_string(),
            pain,
            importance,
            client_dedup_key: None,
            pinned: false,
        }
    }

    /// Test 10: scores are checked before the `f32` cast; skill_action is
    /// still forwarded raw for the daemon to check.
    #[tokio::test]
    async fn build_learn_request_checks_scores_before_the_cast() {
        use http_body_util::BodyExt;

        for pain in [1e9, 1e40, -0.5, 10.000_000_1] {
            assert_eq!(
                build_learn_request(&learn("rust::x", Some(pain), None), "/r")
                    .expect_err("pain out of range"),
                MemoryStoreError::InvalidParams("pain must be in 0.0..=10.0".to_string()),
                "pain {pain}"
            );
        }
        for importance in [11.0, f64::INFINITY] {
            assert_eq!(
                build_learn_request(&learn("rust::x", None, Some(importance)), "/r")
                    .expect_err("importance out of range"),
                MemoryStoreError::InvalidParams("importance must be in 0.0..=10.0".to_string()),
                "importance {importance}"
            );
        }
        build_learn_request(&learn("rust::x", Some(10.0), Some(0.0)), "/r").expect("in range");
        build_learn_request(&learn("rust::x", None, Some(0.0)), "/r").expect("pain omitted");

        let req = build_learn_request(&learn("BAD::UPPER.dots", None, None), "/r")
            .expect("skill_action is the daemon's to check");
        let body = req.into_body().collect().await.expect("body").to_bytes();
        let sent: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(sent["skill_action"], "BAD::UPPER.dots");
    }
}
