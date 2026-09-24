//! One memory-store interface, two adapters (BZR-173).
//!
//! [`MemoryStore`] is the seam every MCP tool call goes through: `search_nodes`
//! calls [`MemoryStore::recall_json`], `append_node` calls
//! [`MemoryStore::append`]. Two adapters implement it:
//!
//! * [`InProcessStore`] — the Phase-1 path. Recall reads the project's Tantivy
//!   index through the session's shared `ProjectIndexMap` (one handle per root,
//!   never a per-call open; WEG-252). Append goes through
//!   [`LearnIngress::build_agent_learning`] and then `coordinator_tx.send().await`
//!   with no timeout.
//! * [`DaemonStore`] — the daemon proxy. HTTP-over-UDS on Unix (WEG-259),
//!   loopback TCP + `Authorization: Bearer` off Unix (AILAB-192). Same requests,
//!   same `X-Agent-Root` contract, same per-call connect on both transports.
//!
//! The HTTP handlers are not adapters. `GET /api/v1/recall` keeps its reader
//! pinned to the primary handle through `AppState::with_index_handle` and
//! shares only [`recall_to_json`]. `POST /api/v1/learn` keeps
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
use tantivy::IndexReader;
use tokio::sync::mpsc;

use crate::config::load_config;
use crate::coordinator::MemoryCoordinatorMsg;
use crate::ingress::{LearnIngress, LearnResponse, RecallIngress};
use crate::AgentRoot;

#[cfg(not(unix))]
use crate::daemon_client::{AuthToken, DaemonEndpoint};
#[cfg(unix)]
use crate::server::index_map::{ProjectIndexMap, ProjectIndexMapConfig};
#[cfg(unix)]
use crate::server::tantivy_handle::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};

/// The empty recall envelope, returned when there is nothing readable.
const EMPTY_RESULTS: &str = r#"{"results":[]}"#;

/// Errors a [`MemoryStore`] call can return.
///
/// Two kinds, mirroring the two MCP error codes the tool methods have always
/// surfaced: a caller mistake (`InvalidRequest`) and everything else
/// (`Internal`). The `Display` is the message alone, so the MCP mapping keeps
/// every pre-BZR-173 message byte-identical.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MemoryStoreError {
    /// Bad input, an unregistered project, or an index that cannot be opened.
    #[error("{0}")]
    InvalidRequest(String),
    /// Coordinator, transport, or daemon failure.
    #[error("{0}")]
    Internal(String),
}

impl MemoryStoreError {
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

/// Run a BM25 × salience recall on `reader` and serialise it to the canonical
/// `{"results":[...]}` string. The single recall body shared by the in-process
/// adapter and `GET /api/v1/recall`; each caller chooses its own reader.
pub(crate) fn recall_to_json(
    reader: &IndexReader,
    query: &str,
    k: u32,
    now_sec: i64,
) -> tantivy::Result<String> {
    let (_, schema_fields) = crate::index::build_schema();
    let results = crate::recall(reader, &schema_fields, query, k as usize, None, now_sec)?;
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
/// Off Unix recall is always `{"results":[]}`: `TantivyIndexHandle::open`
/// writes through [`crate::io::write_atomic`], which is `Unsupported` there.
pub struct InProcessStore {
    agent_root: Option<AgentRoot>,
    coordinator_tx: Option<mpsc::Sender<MemoryCoordinatorMsg>>,
    /// Shared with the owning MCP server. One IndexWriter per root per session
    /// (WEG-252); never open an index per recall.
    #[cfg(unix)]
    index_map: SharedIndexMap,
}

impl InProcessStore {
    /// Build an in-process store. `index_map` is the MCP session's map; pass
    /// one pre-seeded with a handle to share an `IndexWriter` with the
    /// coordinator's indexer (watch / Phase-1 topology).
    pub fn new(
        agent_root: Option<AgentRoot>,
        coordinator_tx: Option<mpsc::Sender<MemoryCoordinatorMsg>>,
        #[cfg(unix)] index_map: SharedIndexMap,
    ) -> Self {
        Self {
            agent_root,
            coordinator_tx,
            #[cfg(unix)]
            index_map,
        }
    }

    /// Synchronous half of Unix recall. The map lock is released before the
    /// search runs (the reader is an `Arc`-backed clone), and never lives
    /// across an `.await`.
    #[cfg(unix)]
    fn recall_local(
        &self,
        root: &AgentRoot,
        query: &str,
        k: u32,
    ) -> Result<String, MemoryStoreError> {
        let reader = {
            let mut map = self
                .index_map
                .lock()
                .map_err(|_| MemoryStoreError::internal("index map lock poisoned"))?;
            let handle = map
                .get_or_open(root.project_root(), |p| {
                    TantivyIndexHandle::open(
                        &AgentRoot::new(p.to_path_buf()),
                        DEFAULT_COMMIT_CADENCE,
                    )
                })
                .map_err(|e| MemoryStoreError::invalid(format!("index open failed: {e}")))?;
            handle.touch();
            handle.reader().clone()
        };
        let now_sec = chrono::Utc::now().timestamp();
        recall_to_json(&reader, query, k, now_sec)
            .map_err(|e| MemoryStoreError::invalid(format!("recall failed: {e}")))
    }
}

#[async_trait::async_trait]
impl MemoryStore for InProcessStore {
    async fn recall_json(&self, query: &str, k: u32) -> Result<String, MemoryStoreError> {
        let Some(root) = &self.agent_root else {
            return Ok(EMPTY_RESULTS.to_string());
        };
        #[cfg(unix)]
        {
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
        .map_err(|e| MemoryStoreError::invalid(e.to_string()))?;
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
            .map_err(|e| MemoryStoreError::internal(e.to_string()))?;

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
/// URL-encoding crate — WEG-259 deliberately adds only hyper / http-body-util
/// / bytes as new deps.
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
pub(crate) fn build_recall_request(
    query: &str,
    k: u32,
    agent_root_header: &str,
) -> Result<hyper::Request<Full<Bytes>>, MemoryStoreError> {
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
/// The daemon mints the real `id` and `timestamp`, and re-runs every
/// `LearnIngress` check (skill_action, score range, redaction) on arrival, so
/// the raw skill_action and content are forwarded as given. Pure: unit-tested
/// directly, and cfg-free — see [`build_recall_request`].
pub(crate) fn build_learn_request(
    req: &AppendRequest,
    agent_root_header: &str,
) -> Result<hyper::Request<Full<Bytes>>, MemoryStoreError> {
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

/// Map a non-success daemon HTTP status per the WEG-259 error table. Never
/// panics. Cfg-free: one status table for both transports, so a Windows
/// client and a Unix client explain the same 404 the same way.
fn map_remote_status_error(status: hyper::StatusCode, body: &Bytes) -> MemoryStoreError {
    let body_str = String::from_utf8_lossy(body);
    match status.as_u16() {
        400 => MemoryStoreError::invalid(format!("invalid X-Agent-Root: {body_str}")),
        404 => MemoryStoreError::invalid(format!(
            "project not registered with daemon — run dreamd init: {body_str}"
        )),
        413 => MemoryStoreError::invalid("learning exceeds size limit"),
        415 => MemoryStoreError::invalid("content-type rejected (defensive)"),
        _ => MemoryStoreError::internal(format!("daemon error {status}: {body_str}")),
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
/// collect the response into `(status, body)`. Per-call connect (no pooling) —
/// MCP tool calls are infrequent; the WEG-78-A "fresh handle per call"
/// rationale applies.
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
}
