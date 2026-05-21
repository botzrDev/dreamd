//! Axum 0.8 HTTP server — `AppState`, router, and request handlers.
//!
//! `AppState` — shared state cloned into every request.
//! `build_router` — mounts `/api/v1` routes with `X-Agent-Root` middleware.
//! `agent_root_middleware` — validates header + registry lookup on every request.
//! `post_learn` — WEG-68 / DR-402: validate → normalise → redact → dispatch → 201.
//!
//! Out of scope here: TraceLayer (WEG-144), SO_PEERCRED (WEG-72), TCP binding
//! (WEG-73), TantivyIndexHandle::reader (WEG-69).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{Extension, Json, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::IntoResponse;
use dreamd_protocol::AgentLearning;
use tokio::sync::oneshot;

use crate::config::Config;
use crate::coordinator::{CoordinatorError, MemoryCoordinatorMsg};
use crate::redaction::redact;
use crate::registry::ProjectEntry;
use crate::server::lifecycle::CoordinatorSendError;
use crate::server::{ProjectIndexMap, Supervisor, TantivyIndexHandle};

/// Shared application state cloned into every Axum request via
/// `State<AppState>`. All fields behind `Arc` so cloning is cheap.
///
/// `registry_path` — path to `~/.agent/registry.toml`. Per-request
/// middleware calls `resolve_project(&state.registry_path, agent_root)`.
///
/// `supervisor` — owned by the process entry point; handlers call
/// `state.supervisor.try_send(msg)` for coordinator dispatch.
///
/// `config` — layered runtime config loaded at startup.
///
/// `index_map` — lazy-opened per-project Tantivy handles. `Mutex` (not
/// `RwLock`) because `ProjectIndexMap::get_or_open` is `&mut self` even
/// for reads (mutates LRU ordering).
#[derive(Clone)]
pub struct AppState {
    pub registry_path: PathBuf,
    pub supervisor: Arc<Supervisor>,
    pub config: Arc<Config>,
    pub index_map: Arc<Mutex<ProjectIndexMap<TantivyIndexHandle>>>,
}

impl AppState {
    pub fn new(
        registry_path: PathBuf,
        supervisor: Supervisor,
        config: Config,
        index_map: ProjectIndexMap<TantivyIndexHandle>,
    ) -> Self {
        Self {
            registry_path,
            supervisor: Arc::new(supervisor),
            config: Arc::new(config),
            index_map: Arc::new(Mutex::new(index_map)),
        }
    }
}

/// Response body for a successful `POST /api/v1/learn`.
#[derive(serde::Serialize)]
struct LearnResponse {
    id: String,
    timestamp: String,
    deduplicated: bool,
}

/// Build the `/api/v1` router with `X-Agent-Root` validation middleware.
pub fn build_router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/api/v1/learn", axum::routing::post(post_learn))
        .route("/api/v1/recall", axum::routing::get(stub_handler))
        .route("/api/v1/dream", axum::routing::post(stub_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            agent_root_middleware,
        ))
        .with_state(state)
}

async fn stub_handler() -> axum::http::StatusCode {
    axum::http::StatusCode::NOT_IMPLEMENTED
}

/// `POST /api/v1/learn` — DR-402.
///
/// Flow: validate `X-Agent-Root` (middleware) → extract dedup key →
/// normalise + validate `skill_action` → redact content → dispatch to
/// the coordinator → 201 on durable write, or the appropriate error code.
async fn post_learn(
    State(state): State<AppState>,
    Extension(project): Extension<ProjectEntry>,
    headers: HeaderMap,
    Json(mut learning): Json<AgentLearning>,
) -> axum::response::Response {
    tracing::debug!(project_root = %project.root, "POST /api/v1/learn");

    // Step 1 — extract optional idempotency key.
    let client_dedup_key = headers
        .get("x-client-dedup-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());

    // Step 2 — normalise + validate skill_action.
    // Order: trim → lowercase → collapse interior whitespace → replace spaces
    // with `_` → validate charset [a-z0-9_:.-] and ≤ 256 bytes.
    let lowercased = learning.skill_action.trim().to_lowercase();
    let sa: String = lowercased.split_whitespace().collect::<Vec<_>>().join("_");
    if sa.is_empty() {
        return error_400("invalid skill_action: empty after normalisation");
    }
    if sa.len() > 256 {
        return error_400("invalid skill_action: exceeds 256 bytes");
    }
    if sa
        .bytes()
        .any(|b| !matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b':' | b'.' | b'-'))
    {
        return error_400("invalid skill_action: contains characters outside [a-z0-9_:.-]");
    }
    learning.skill_action = sa;

    // Step 3 — redact content. opt-out is Config.redaction only (D2).
    learning.content = redact(&learning.content, state.config.redaction);

    // Step 4 — capture timestamp before `learning` is moved (Option A).
    let timestamp = learning.timestamp.to_rfc3339();

    // Step 5 — build and dispatch.
    let (resp_tx, resp_rx) = oneshot::channel();
    let msg = MemoryCoordinatorMsg::AppendLearning {
        learning,
        client_dedup_key,
        response_tx: resp_tx,
    };

    match state.supervisor.try_send(msg).await {
        Ok(()) => {}
        Err(CoordinatorSendError::Full) => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, HeaderValue::from_static("1"))],
                axum::Json(serde_json::json!({ "error": "coordinator busy, retry" })),
            )
                .into_response();
        }
        Err(CoordinatorSendError::Closed) => {
            return error_500("coordinator unavailable");
        }
    }

    // Step 6 — await durable write outcome.
    match resp_rx.await {
        Ok(Ok(outcome)) => (
            axum::http::StatusCode::CREATED,
            axum::Json(LearnResponse {
                id: outcome.id.as_str().to_owned(),
                timestamp,
                deduplicated: outcome.deduplicated,
            }),
        )
            .into_response(),
        Ok(Err(CoordinatorError::PayloadTooLarge { .. })) => (
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            axum::Json(serde_json::json!({ "error": "payload too large" })),
        )
            .into_response(),
        Ok(Err(_)) | Err(_) => error_500("coordinator error"),
    }
}

/// Extract and validate `X-Agent-Root` on every request under `/api/v1`.
///
/// * Missing or non-UTF-8 header → 400 Bad Request, JSON error body.
/// * `resolve_project` returns `Err` (malformed registry TOML) → 500.
/// * `resolve_project` returns `Ok(None)` (root not registered) → 404.
/// * `resolve_project` returns `Ok(Some(entry))` → inserts
///   `Extension(entry)` and calls `next.run(req)`.
pub async fn agent_root_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let agent_root_str = match req
        .headers()
        .get("x-agent-root")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s.to_owned(),
        None => {
            return error_400("missing or non-UTF-8 X-Agent-Root header");
        }
    };

    let agent_root_path = std::path::Path::new(&agent_root_str);

    match crate::registry::resolve_project(&state.registry_path, agent_root_path) {
        Ok(Some(entry)) => {
            req.extensions_mut().insert(entry);
            next.run(req).await
        }
        Ok(None) => error_404("agent root not registered"),
        Err(_) => error_500("registry read failed"),
    }
}

fn error_400(msg: &str) -> axum::response::Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

fn error_404(msg: &str) -> axum::response::Response {
    (
        axum::http::StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

fn error_500(msg: &str) -> axum::response::Response {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::MemoryCoordinatorMsg;
    use crate::layout::AgentRoot;
    use crate::server::index_map::ProjectIndexMapConfig;
    use crate::server::lifecycle::Supervisor;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use chrono::{DateTime, Utc};
    use dreamd_protocol::{AgentLearning, EventId};
    use tokio::sync::oneshot;
    use tower::ServiceExt;

    const SAMPLE_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn make_test_state(registry_path: PathBuf) -> AppState {
        let (supervisor, _rx) = Supervisor::for_backpressure_test();
        let config = Config::default();
        let index_map = ProjectIndexMap::new(ProjectIndexMapConfig::default());
        AppState {
            registry_path,
            supervisor: Arc::new(supervisor),
            config: Arc::new(config),
            index_map: Arc::new(Mutex::new(index_map)),
        }
    }

    /// State backed by a real `MemoryCoordinator` for tests that need actual
    /// append semantics (durability, dedup, 413 rejection).
    fn make_real_state(project_dir: &std::path::Path, registry_path: PathBuf) -> AppState {
        let agent_root = AgentRoot::new(project_dir);
        let supervisor =
            Supervisor::start(&agent_root, 8, None).expect("start supervisor for test");
        AppState {
            registry_path,
            supervisor: Arc::new(supervisor),
            config: Arc::new(Config::default()),
            index_map: Arc::new(Mutex::new(ProjectIndexMap::new(
                ProjectIndexMapConfig::default(),
            ))),
        }
    }

    fn write_registry(registry_path: &std::path::Path, root: &str) {
        std::fs::write(
            registry_path,
            format!("[[projects]]\nroot = \"{root}\"\n"),
        )
        .unwrap();
    }

    fn sample_body(skill_action: &str, content: &str) -> serde_json::Value {
        serde_json::json!({
            "schema_version": "1.0",
            "id": format!("evt_{SAMPLE_ULID}"),
            "timestamp": "2026-05-20T12:00:00Z",
            "pain": 5.0,
            "importance": 5.0,
            "skill_action": skill_action,
            "source_harness": "test-harness",
            "content": content
        })
    }

    fn placeholder_learning(skill_action: &str) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0".to_owned(),
            id: EventId::parse(&format!("evt_{SAMPLE_ULID}")).unwrap(),
            timestamp: DateTime::parse_from_rfc3339("2026-05-20T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            pain: 5.0,
            importance: 5.0,
            pinned: false,
            skill_action: skill_action.to_owned(),
            source_harness: "test-harness".to_owned(),
            content: "test content".to_owned(),
        }
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ── existing middleware tests (unchanged assertions) ──────────────────────

    #[tokio::test]
    async fn missing_x_agent_root_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let state = make_test_state(dir.path().join("registry.toml"));
        let router = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .body(Body::empty())
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unregistered_agent_root_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let state = make_test_state(dir.path().join("registry.toml"));
        let router = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", dir.path().to_str().unwrap())
            .body(Body::empty())
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn registered_agent_root_passes_middleware() {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("registry.toml");
        let root_str = dir.path().to_str().unwrap();
        write_registry(&registry_path, root_str);

        let state = make_test_state(registry_path);
        let router = build_router(state);

        // Empty body without content-type: middleware passes (registered root),
        // Json extractor rejects with 415. Confirms middleware no longer
        // short-circuits with 501.
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn stub_learn_endpoint_returns_501() {
        // Previously tested that the stub handler returned 501; now that
        // post_learn is wired, an empty body without content-type yields 415.
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("registry.toml");
        let root_str = dir.path().to_str().unwrap();
        write_registry(&registry_path, root_str);

        let state = make_test_state(registry_path);
        let router = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    // ── 8 new tests (WEG-68) ──────────────────────────────────────────────────

    #[tokio::test]
    async fn learn_valid_body_returns_201() {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("registry.toml");
        let root_str = dir.path().to_str().unwrap();
        write_registry(&registry_path, root_str);

        let state = make_real_state(dir.path(), registry_path);
        let router = build_router(state);

        let body = sample_body("rust.cargo.test", "learned something useful");
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let json = body_json(resp).await;
        assert!(
            json["id"].as_str().unwrap().starts_with("evt_"),
            "id must be daemon-minted"
        );
        assert_eq!(json["timestamp"].as_str().unwrap(), "2026-05-20T12:00:00+00:00");
        assert_eq!(json["deduplicated"], false);
    }

    #[tokio::test]
    async fn learn_invalid_skill_action_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("registry.toml");
        let root_str = dir.path().to_str().unwrap();
        write_registry(&registry_path, root_str);

        let state = make_test_state(registry_path);
        let router = build_router(state);

        // `!` is outside [a-z0-9_:.-]
        let body = sample_body("rust!invalid", "body");
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn learn_skill_action_normalised() {
        // Uppercase letters and leading/trailing whitespace must be normalised
        // before validation. "  Rust.Cargo.TEST  " → "rust.cargo.test".
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("registry.toml");
        let root_str = dir.path().to_str().unwrap();
        write_registry(&registry_path, root_str);

        let state = make_real_state(dir.path(), registry_path.clone());
        let agent_root = AgentRoot::new(dir.path());
        let router = build_router(state);

        let body = sample_body("  Rust.Cargo.TEST  ", "normalisation test");
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "normalised form is valid");

        // Verify the normalised skill_action landed on disk.
        let jsonl = std::fs::read_to_string(agent_root.episodic_jsonl()).unwrap();
        let record: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
        assert_eq!(record["skill_action"].as_str().unwrap(), "rust.cargo.test");
    }

    #[tokio::test]
    async fn learn_missing_x_agent_root_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let state = make_test_state(dir.path().join("registry.toml"));
        let router = build_router(state);

        let body = sample_body("rust.test", "body");
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            // deliberately omitting x-agent-root
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn learn_dedup_key_returns_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("registry.toml");
        let root_str = dir.path().to_str().unwrap();
        write_registry(&registry_path, root_str);

        let state = make_real_state(dir.path(), registry_path);
        let router = build_router(state);

        let body = sample_body("rust.test", "dedup content");
        let body_bytes = serde_json::to_vec(&body).unwrap();

        let req1 = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .header("x-client-dedup-key", "idempotency-key-42")
            .body(Body::from(body_bytes.clone()))
            .unwrap();
        let req2 = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .header("x-client-dedup-key", "idempotency-key-42")
            .body(Body::from(body_bytes))
            .unwrap();

        let resp1 = router.clone().into_service().oneshot(req1).await.unwrap();
        assert_eq!(resp1.status(), StatusCode::CREATED);
        let j1 = body_json(resp1).await;

        let resp2 = router.into_service().oneshot(req2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::CREATED);
        let j2 = body_json(resp2).await;

        assert_eq!(j1["id"], j2["id"], "same dedup key must return same EventId");
        assert_eq!(j2["deduplicated"], true, "second response must be flagged deduplicated");
    }

    #[tokio::test]
    async fn learn_content_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("registry.toml");
        let root_str = dir.path().to_str().unwrap();
        write_registry(&registry_path, root_str);

        let state = make_real_state(dir.path(), registry_path);
        let agent_root = AgentRoot::new(dir.path());
        let router = build_router(state);

        let secret = "AKIAIOSFODNN7EXAMPLE";
        let body = sample_body("rust.test", &format!("key is {secret}"));
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let jsonl = std::fs::read_to_string(agent_root.episodic_jsonl()).unwrap();
        let record: serde_json::Value =
            serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
        assert!(
            !record["content"].as_str().unwrap().contains(secret),
            "secret must not appear on disk after redaction"
        );
        assert!(
            record["content"].as_str().unwrap().contains("[REDACTED]"),
            "REDACTED marker must be present"
        );
    }

    #[tokio::test]
    async fn learn_channel_full_returns_503() {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("registry.toml");
        let root_str = dir.path().to_str().unwrap();
        write_registry(&registry_path, root_str);

        // Capacity-1 channel; pre-fill so the handler's try_send times out.
        let (supervisor, _drain_rx) = Supervisor::for_backpressure_test();
        let (fill_tx, _fill_rx) = oneshot::channel();
        supervisor
            .sender()
            .try_send(MemoryCoordinatorMsg::AppendLearning {
                learning: placeholder_learning("rust.fill"),
                client_dedup_key: None,
                response_tx: fill_tx,
            })
            .expect("pre-fill on empty channel");

        let state = AppState {
            registry_path,
            supervisor: Arc::new(supervisor),
            config: Arc::new(Config::default()),
            index_map: Arc::new(Mutex::new(ProjectIndexMap::new(
                ProjectIndexMapConfig::default(),
            ))),
        };
        let router = build_router(state);

        let body = sample_body("rust.test", "will not land");
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            Some("1"),
            "503 must carry Retry-After: 1"
        );
    }

    #[tokio::test]
    async fn learn_oversized_content_returns_413() {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("registry.toml");
        let root_str = dir.path().to_str().unwrap();
        write_registry(&registry_path, root_str);

        let state = make_real_state(dir.path(), registry_path);
        let router = build_router(state);

        // 5 KiB content guarantees the serialized line exceeds MAX_LEARNING_LINE_BYTES.
        let body = sample_body("rust.test", &"x".repeat(5 * 1024));
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = router.into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
