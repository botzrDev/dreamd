//! HTTP ingress contract tests and shared test fixtures.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::handlers::PREFERENCES_SIZE_CAP;
use super::{build_router, AppState, PeerUid};
use crate::config::Config;
use crate::coordinator::MemoryCoordinatorMsg;
use crate::layout::AgentRoot;
use crate::server::index_map::ProjectIndexMapConfig;
use crate::server::lifecycle::Supervisor;
use crate::server::{ProjectIndexMap, SupervisorMap};
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
        daemon_uid: nix::unistd::Uid::current().as_raw(),
        primary: None,
        supervisor_map: Arc::new(Mutex::new(SupervisorMap::with_defaults())),
    }
}

/// State backed by a real `MemoryCoordinator` for tests that need actual
/// append semantics (durability, dedup, 413 rejection).
fn make_real_state(project_dir: &std::path::Path, registry_path: PathBuf) -> AppState {
    let agent_root = AgentRoot::new(project_dir);
    let supervisor = Supervisor::start(&agent_root, 8, None).expect("start supervisor for test");
    AppState {
        registry_path,
        supervisor: Arc::new(supervisor),
        config: Arc::new(Config::default()),
        index_map: Arc::new(Mutex::new(ProjectIndexMap::new(
            ProjectIndexMapConfig::default(),
        ))),
        daemon_uid: nix::unistd::Uid::current().as_raw(),
        primary: None,
        supervisor_map: Arc::new(Mutex::new(SupervisorMap::with_defaults())),
    }
}

/// Create a TempDir + minimal AppState + Router.
/// Use for tests that only need HTTP routing without a registered project.
fn test_router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let state = make_test_state(dir.path().join("registry.toml"));
    let router = build_router(state);
    (dir, router)
}

/// Inject the current process UID as PeerUid extension.
/// Required because build_router now includes peer_uid_middleware which
/// expects PeerUid to be set (as serve_uds does at connection-accept time).
#[cfg(unix)]
fn with_peer_uid(mut req: axum::http::Request<Body>) -> axum::http::Request<Body> {
    let uid = nix::unistd::Uid::current().as_raw();
    req.extensions_mut().insert(PeerUid(uid));
    req
}

fn write_registry(registry_path: &std::path::Path, root: &str) {
    // Stored roots must be canonicalized to match `resolve_project`, which
    // canonicalizes the query path (registry.rs:53). On macOS a tempdir lives
    // under /var → /private/var (a symlink), so a raw tempdir path stored here
    // never matches the canonicalized query and every request 404s. This is
    // identity on Linux (tempdirs aren't symlinked), so it only changes macOS.
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| std::path::PathBuf::from(root));
    let root = canonical.to_string_lossy();
    std::fs::write(registry_path, format!("[[projects]]\nroot = \"{root}\"\n")).unwrap();
}

/// Create a TempDir, write a registry pointing at it, and return a mock-state
/// router + owned root path string. Keep the returned TempDir alive for the
/// duration of the test.
fn mock_router_with_dir() -> (tempfile::TempDir, String, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_string_lossy().into_owned();
    write_registry(&registry_path, &root_str);
    let state = make_test_state(registry_path);
    let router = build_router(state);
    (dir, root_str, router)
}

/// Like `mock_router_with_dir` but backed by a real `MemoryCoordinator`.
/// Use for tests that need actual append/dedup/durability semantics.
/// Returns `(dir, root_str, router)`; keep `dir` alive for the test.
fn real_router_with_dir() -> (tempfile::TempDir, String, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap().to_owned();
    write_registry(&registry_path, &root_str);
    let state = make_real_state(dir.path(), registry_path);
    let router = build_router(state);
    (dir, root_str, router)
}

fn sample_body(skill_action: &str, content: &str) -> serde_json::Value {
    serde_json::json!({
        "schema_version": "1.0.0",
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
        schema_version: "1.0.0".to_owned(),
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
    let (_dir, router) = test_router();

    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unregistered_agent_root_returns_404() {
    let (_dir, router) = test_router();

    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", _dir.path().to_str().unwrap())
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn registered_agent_root_passes_middleware() {
    let (_dir, root_str, router) = mock_router_with_dir();

    // Empty body without content-type: middleware passes (registered root),
    // Json extractor rejects with 415. Confirms middleware no longer
    // short-circuits with 501.
    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", &root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn stub_learn_endpoint_returns_501() {
    // Previously tested that the stub handler returned 501; now that
    // post_learn is wired, an empty body without content-type yields 415.
    let (_dir, root_str, router) = mock_router_with_dir();

    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", &root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

// ── 8 new tests (WEG-68) ──────────────────────────────────────────────────

#[tokio::test]
async fn learn_valid_body_returns_201() {
    let (_dir, root_str, router) = real_router_with_dir();

    let body = sample_body("rust::cargo::test", "learned something useful");
    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", &root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let json = body_json(resp).await;
    assert!(
        json["id"].as_str().unwrap().starts_with("evt_"),
        "id must be daemon-minted"
    );
    assert_eq!(
        json["timestamp"].as_str().unwrap(),
        "2026-05-20T12:00:00+00:00"
    );
    assert_eq!(json["deduplicated"], false);
}

#[tokio::test]
async fn learn_invalid_skill_action_returns_400() {
    let (_dir, root_str, router) = mock_router_with_dir();

    // `!` is outside [a-z0-9_]
    let body = sample_body("rust!invalid", "body");
    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn learn_skill_action_normalised() {
    // Uppercase letters and leading/trailing whitespace must be normalised
    // before validation. "  Rust::Cargo::TEST  " → "rust::cargo::test".
    let (dir, root_str, router) = real_router_with_dir();
    let agent_root = AgentRoot::new(dir.path());

    let body = sample_body("  Rust::Cargo::TEST  ", "normalisation test");
    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", &root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "normalised form is valid"
    );

    // Verify the normalised skill_action landed on disk.
    let jsonl = std::fs::read_to_string(agent_root.episodic_jsonl()).unwrap();
    let record: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(
        record["skill_action"].as_str().unwrap(),
        "rust::cargo::test"
    );
}

#[tokio::test]
async fn learn_missing_x_agent_root_returns_400() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_test_state(dir.path().join("registry.toml"));
    let router = build_router(state);

    let body = sample_body("rust.test", "body");
    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            // deliberately omitting x-agent-root
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    );

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

    let body = sample_body("rust::test", "dedup content");
    let body_bytes = serde_json::to_vec(&body).unwrap();

    let req1 = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .header("x-client-dedup-key", "idempotency-key-42")
            .body(Body::from(body_bytes.clone()))
            .unwrap(),
    );
    let req2 = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .header("x-client-dedup-key", "idempotency-key-42")
            .body(Body::from(body_bytes))
            .unwrap(),
    );

    let resp1 = router.clone().into_service().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::CREATED);
    let j1 = body_json(resp1).await;

    let resp2 = router.into_service().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::CREATED);
    let j2 = body_json(resp2).await;

    assert_eq!(
        j1["id"], j2["id"],
        "same dedup key must return same EventId"
    );
    assert_eq!(
        j2["deduplicated"], true,
        "second response must be flagged deduplicated"
    );
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
    let body = sample_body("rust::test", &format!("key is {secret}"));
    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let jsonl = std::fs::read_to_string(agent_root.episodic_jsonl()).unwrap();
    let record: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
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
        daemon_uid: nix::unistd::Uid::current().as_raw(),
        primary: None,
        supervisor_map: Arc::new(Mutex::new(SupervisorMap::with_defaults())),
    };
    let router = build_router(state);

    let body = sample_body("rust::test", "will not land");
    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    );

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
    let body = sample_body("rust::test", &"x".repeat(5 * 1024));
    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

// ── WEG-275: tightened charset, score range-check, server-stamped schema ──

/// The tightened charset rejects `.`, `-`, and `/` (all were previously
/// accepted as `[a-z0-9_:.-]`); now only `[a-z0-9_]` segments joined by `::`.
#[tokio::test]
async fn learn_dotted_or_slashed_skill_action_returns_400() {
    let (_dir, root_str, router) = mock_router_with_dir();

    for bad in ["rust.tokio", "rust/borrow-checker"] {
        let body = sample_body(bad, "body");
        let req = with_peer_uid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/learn")
                .header("x-agent-root", &root_str)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        );
        let resp = router.clone().into_service().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{bad:?} must be rejected by the tightened charset"
        );
    }
}

/// `pain` / `importance` outside `0.0..=10.0` are rejected (not clamped).
#[tokio::test]
async fn learn_score_out_of_range_returns_400() {
    let (_dir, root_str, router) = mock_router_with_dir();

    for (pain, importance) in [(-3.0, 5.0), (1e9, 5.0), (5.0, -1.0), (5.0, 11.0)] {
        let body = serde_json::json!({
            "schema_version": "1.0.0",
            "id": format!("evt_{SAMPLE_ULID}"),
            "timestamp": "2026-05-20T12:00:00Z",
            "pain": pain,
            "importance": importance,
            "skill_action": "rust::test",
            "source_harness": "test-harness",
            "content": "out of range"
        });
        let req = with_peer_uid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/learn")
                .header("x-agent-root", &root_str)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        );
        let resp = router.clone().into_service().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "pain={pain} importance={importance} must be rejected"
        );
    }
}

/// The coordinator server-stamps `schema_version`: a client-supplied value
/// is overwritten with `RECORD_SCHEMA_VERSION` on the durable write. This
/// closes the HTTP client-trust gap (post_learn never re-stamped).
#[tokio::test]
async fn learn_server_stamps_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    let state = make_real_state(dir.path(), registry_path);
    let agent_root = AgentRoot::new(dir.path());
    let router = build_router(state);

    let body = serde_json::json!({
        "schema_version": "9.9",
        "id": format!("evt_{SAMPLE_ULID}"),
        "timestamp": "2026-05-20T12:00:00Z",
        "pain": 5.0,
        "importance": 5.0,
        "skill_action": "rust::test",
        "source_harness": "test-harness",
        "content": "stamp me"
    });
    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/learn")
            .header("x-agent-root", root_str)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let jsonl = std::fs::read_to_string(agent_root.episodic_jsonl()).unwrap();
    let record: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(
        record["schema_version"].as_str().unwrap(),
        "1.0.0",
        "client-supplied schema_version must be server-stamped"
    );
}

// ── WEG-69: GET /api/v1/recall tests ─────────────────────────────────────

#[tokio::test]
async fn recall_missing_q_param_returns_400() {
    // Axum's Query extractor rejects a missing required field with 400.
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    let state = make_test_state(registry_path);
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("GET")
            .uri("/api/v1/recall")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn health_reports_fresh_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    let state = make_test_state(registry_path);
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("GET")
            .uri("/api/v1/health")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["index"]["stale"], serde_json::json!(false));
    assert_eq!(json["index"]["unindexed_count"], serde_json::json!(0));
}

#[tokio::test]
async fn recall_empty_index_returns_empty_results() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    let state = make_test_state(registry_path);
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("GET")
            .uri("/api/v1/recall?q=test&k=5")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(
        json["results"],
        serde_json::json!([]),
        "empty index must return empty results array"
    );
}

#[tokio::test]
async fn recall_returns_200_for_registered_root() {
    // Smoke test: registered root + valid q param → 200 with results array.
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    let state = make_test_state(registry_path);
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("GET")
            .uri("/api/v1/recall?q=axum&k=3")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(
        json["results"].is_array(),
        "response must contain a results array; got: {json:?}"
    );
}

// ── WEG-72: SO_PEERCRED middleware tests ─────────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn peer_uid_middleware_allows_matching_uid() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_test_state(dir.path().join("registry.toml"));
    let router = build_router(state);

    // Inject the correct UID — middleware should pass through.
    // agent_root_middleware will reject with 400 (no X-Agent-Root), which is fine.
    let req = with_peer_uid(
        Request::builder()
            .method("GET")
            .uri("/api/v1/recall?q=test")
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    // peer_uid check passes; agent_root check fails with 400 (no header)
    assert_ne!(resp.status(), StatusCode::FORBIDDEN);
}

#[cfg(unix)]
#[tokio::test]
async fn peer_uid_middleware_rejects_wrong_uid() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_test_state(dir.path().join("registry.toml"));
    let router = build_router(state);

    // Inject a UID that doesn't match daemon_uid.
    let wrong_uid = nix::unistd::Uid::current().as_raw().wrapping_add(1);
    let mut req = Request::builder()
        .method("GET")
        .uri("/api/v1/recall?q=test")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(PeerUid(wrong_uid));

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[cfg(unix)]
#[tokio::test]
async fn peer_uid_middleware_rejects_missing_peer_uid() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_test_state(dir.path().join("registry.toml"));
    let router = build_router(state);

    // No PeerUid extension — must be rejected.
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/recall?q=test")
        .body(Body::empty())
        .unwrap();

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[cfg(unix)]
#[tokio::test]
async fn serve_uds_smoke_test() {
    use crate::server::uds_server::{bind_api_socket, serve_uds};

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("smoke.sock");
    let registry_path = dir.path().join("registry.toml");

    let state = make_test_state(registry_path);
    let router = build_router(state);

    let listener = bind_api_socket(&sock_path).expect("bind");
    tokio::spawn(async move {
        serve_uds(listener, router).await.ok();
    });

    // Give the accept loop a moment to start.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Connect from the same process (same UID = daemon_uid).
    let stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .expect("connect");

    // Send a minimal HTTP/1.1 request. No X-Agent-Root → 400 after peer-uid passes.
    // The exact status just confirms the accept loop is wired end-to-end.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = stream;
    stream
        .write_all(
            b"GET /api/v1/recall?q=test HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("write request");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let response_str = String::from_utf8_lossy(&response);
    // peer_uid passes (same process UID), agent_root_middleware rejects with 400
    assert!(
        response_str.contains("400") || response_str.contains("404"),
        "expected 400 or 404, got: {response_str}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bind_api_socket_creates_0600_socket() {
    use crate::server::uds_server::bind_api_socket;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("api.sock");

    let _listener = bind_api_socket(&sock_path).expect("bind_api_socket should succeed");

    let mode = std::fs::metadata(&sock_path)
        .expect("socket file must exist")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "socket perms should be 0600, got {:o}", mode);
}

#[cfg(unix)]
#[tokio::test]
async fn serve_uds_rejects_mismatched_daemon_uid() {
    use crate::server::uds_server::{bind_api_socket, serve_uds};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("reject.sock");
    let registry_path = dir.path().join("registry.toml");

    // daemon_uid deliberately wrong — current process sends its real UID via
    // SO_PEERCRED; the middleware compares against this value and must return 403.
    let wrong_uid = nix::unistd::Uid::current().as_raw().wrapping_add(1);
    let state = AppState {
        registry_path,
        supervisor: Arc::new(Supervisor::for_backpressure_test().0),
        config: Arc::new(Config::default()),
        index_map: Arc::new(Mutex::new(ProjectIndexMap::new(
            ProjectIndexMapConfig::default(),
        ))),
        daemon_uid: wrong_uid,
        primary: None,
        supervisor_map: Arc::new(Mutex::new(SupervisorMap::with_defaults())),
    };
    let router = build_router(state);

    let listener = bind_api_socket(&sock_path).expect("bind");
    tokio::spawn(async move {
        serve_uds(listener, router).await.ok();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .expect("connect");
    stream
        .write_all(
            b"GET /api/v1/recall?q=test HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("write request");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let response_str = String::from_utf8_lossy(&response);
    assert!(
        response_str.contains("403"),
        "expected 403 for mismatched UID, got: {response_str}"
    );
}

// ── WEG-82: GET /api/v1/preferences tests (DR-115) ───────────────────────

/// T1: Absent PREFERENCES.md → 200, empty body, no truncation headers.
#[tokio::test]
async fn preferences_absent_returns_200_empty() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    // Deliberately do NOT write PREFERENCES.md.
    let state = make_test_state(registry_path);
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("GET")
            .uri("/api/v1/preferences")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("x-dreamd-truncated").is_none(),
        "absent file must not set X-Dreamd-Truncated"
    );
    let json = body_json(resp).await;
    assert_eq!(
        json["body"],
        serde_json::json!(""),
        "body must be empty string"
    );
    assert_eq!(
        json["last_modified"],
        serde_json::json!(null),
        "last_modified must be null"
    );
}

/// T2: Present PREFERENCES.md ≤ 16 KB → full body returned, no truncation headers.
#[tokio::test]
async fn preferences_small_file_returns_full_body() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    // Create .agent/personal/ and write a small PREFERENCES.md.
    let agent_root = crate::layout::AgentRoot::new(dir.path());
    let pref_path = agent_root.preferences_md();
    std::fs::create_dir_all(pref_path.parent().unwrap()).unwrap();
    let content = "# My Preferences\n\nI prefer concise answers.\n";
    std::fs::write(&pref_path, content).unwrap();

    let state = make_test_state(registry_path);
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("GET")
            .uri("/api/v1/preferences")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("x-dreamd-truncated").is_none(),
        "small file must not set X-Dreamd-Truncated"
    );
    let json = body_json(resp).await;
    assert_eq!(
        json["body"].as_str().unwrap(),
        content,
        "body must match file contents exactly"
    );
    assert!(
        json["last_modified"].as_str().is_some(),
        "last_modified must be a non-null RFC 3339 string"
    );
}

/// T3: Present PREFERENCES.md > 16 KB → truncated body, truncation headers set.
#[tokio::test]
async fn preferences_large_file_returns_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    let agent_root = crate::layout::AgentRoot::new(dir.path());
    let pref_path = agent_root.preferences_md();
    std::fs::create_dir_all(pref_path.parent().unwrap()).unwrap();

    // Write exactly PREFERENCES_SIZE_CAP + 1 bytes (all ASCII 'x').
    let oversized = vec![b'x'; PREFERENCES_SIZE_CAP + 1];
    std::fs::write(&pref_path, &oversized).unwrap();

    let state = make_test_state(registry_path);
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("GET")
            .uri("/api/v1/preferences")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        resp.headers()
            .get("x-dreamd-truncated")
            .and_then(|v| v.to_str().ok()),
        Some("true"),
        "X-Dreamd-Truncated must be 'true'"
    );
    assert_eq!(
        resp.headers()
            .get("x-dreamd-original-size")
            .and_then(|v| v.to_str().ok()),
        Some((PREFERENCES_SIZE_CAP + 1).to_string().as_str()),
        "X-Dreamd-Original-Size must equal the full file size"
    );

    let json = body_json(resp).await;
    assert_eq!(
        json["body"].as_str().unwrap().len(),
        PREFERENCES_SIZE_CAP,
        "truncated body must be exactly PREFERENCES_SIZE_CAP bytes"
    );
}

/// T4: Missing X-Agent-Root header → 400.
#[tokio::test]
async fn preferences_missing_agent_root_header_400() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_test_state(dir.path().join("registry.toml"));
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("GET")
            .uri("/api/v1/preferences")
            // deliberately omitting x-agent-root
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// T5: X-Agent-Root present but path not in registry → 404.
#[tokio::test]
async fn preferences_unregistered_root_404() {
    let dir = tempfile::tempdir().unwrap();
    // Registry file is absent entirely — resolve_project returns Ok(None).
    let state = make_test_state(dir.path().join("registry.toml"));
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("GET")
            .uri("/api/v1/preferences")
            .header("x-agent-root", dir.path().to_str().unwrap())
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── WEG-70: POST /api/v1/dream tests (DR-404) ────────────────────────────

/// T1 — Happy path: non-empty JSONL, POST /api/v1/dream → 200 {"status":"ok"}.
#[tokio::test]
async fn dream_happy_path_returns_200() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    // Scaffold minimal .agent/ with one learning in the episodic JSONL.
    let agent_root = AgentRoot::new(dir.path());
    let jsonl_path = agent_root.episodic_jsonl();
    std::fs::create_dir_all(jsonl_path.parent().unwrap()).unwrap();
    // Also create the .dreamd/ dir so WAL can write state.json.
    std::fs::create_dir_all(agent_root.dreamd_dir()).unwrap();
    std::fs::write(
            &jsonl_path,
            b"{\"schema_version\":\"1.0\",\"id\":\"evt_01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"pain\":5.0,\"importance\":5.0,\"pinned\":false,\"skill_action\":\"rust.test\",\"source_harness\":\"test\",\"content\":\"test content\",\"recurrence\":0}\n",
        )
        .unwrap();

    // Real coordinator: the dream cycle now routes through it (WEG-271),
    // so the mock backpressure supervisor would reject the dispatch.
    let state = make_real_state(dir.path(), registry_path);
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/dream")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], serde_json::json!("ok"));
}

/// T2 — 409 guard: state.json with "in_progress" → 409.
#[tokio::test]
async fn dream_in_progress_returns_409() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    // Write state.json with "in_progress" status to simulate a running cycle.
    let agent_root = AgentRoot::new(dir.path());
    std::fs::create_dir_all(agent_root.dreamd_dir()).unwrap();
    std::fs::write(
        agent_root.state_json(),
        b"{\"schema_version\":\"1.0\",\"last_dream_cycle_status\":\"in_progress\"}\n",
    )
    .unwrap();

    let state = make_test_state(registry_path);
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/dream")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

/// T3 — Empty JSONL: POST /api/v1/dream → 200, LESSONS.md NOT created.
///
/// The consolidation cluster engine early-returns when the JSONL is empty
/// (no promoted clusters), so LESSONS.md is never written.
#[tokio::test]
async fn dream_empty_jsonl_returns_200_no_lessons_md() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    // Scaffold .agent/ with an empty JSONL.
    let agent_root = AgentRoot::new(dir.path());
    let jsonl_path = agent_root.episodic_jsonl();
    std::fs::create_dir_all(jsonl_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(agent_root.dreamd_dir()).unwrap();
    std::fs::write(&jsonl_path, b"").unwrap();

    // Real coordinator: the dream cycle now routes through it (WEG-271).
    let state = make_real_state(dir.path(), registry_path);
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/dream")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], serde_json::json!("ok"));

    // LESSONS.md must NOT have been created for an empty JSONL.
    let lessons_path = agent_root.semantic_dir().join("LESSONS.md");
    assert!(
        !lessons_path.exists(),
        "LESSONS.md must not be created when JSONL is empty"
    );
}

/// T4 — Regression (WEG-271): a dream cycle must not orphan the
/// coordinator's append fd. After the cycle replaces AGENT_LEARNINGS.jsonl
/// by atomic rename, a subsequent POST /learn must land on the live inode
/// (the file at the path), not the unlinked inode the stale fd points at.
///
/// Learning #1 is dated > 90 days in the past so the decay pruner archives
/// it and rewrites the JSONL via rename — that rename is precisely what
/// orphaned the fd pre-fix. (A single fresh learning would NOT decay and
/// would trigger no rename, so the bug would not reproduce.) Learning #2 is
/// appended after the cycle; pre-fix it was written to the orphaned inode
/// and never appeared in the on-disk file.
#[tokio::test]
async fn dream_cycle_does_not_orphan_coordinator_append_fd() {
    let (dir, root_str, router) = real_router_with_dir();
    let agent_root = AgentRoot::new(dir.path());
    // The dream cycle's WAL writes under .agent/.dreamd/.
    std::fs::create_dir_all(agent_root.dreamd_dir()).unwrap();

    // Learning #1 — timestamp > 90 days old so the decay pruner archives it,
    // forcing the rewrite-by-rename that orphans the coordinator fd.
    let body1 = serde_json::json!({
        "schema_version": "1.0.0",
        "id": format!("evt_{SAMPLE_ULID}"),
        "timestamp": "2020-01-01T00:00:00Z",
        "pain": 5.0,
        "importance": 5.0,
        "skill_action": "rust::regression",
        "source_harness": "test-harness",
        "content": "first-learning-pre-dream"
    });
    let resp1 = router
        .clone()
        .into_service()
        .oneshot(with_peer_uid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/learn")
                .header("x-agent-root", &root_str)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body1).unwrap()))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::CREATED);

    // Dream cycle — decay archives learning #1 and renames the JSONL.
    let dream_resp = router
        .clone()
        .into_service()
        .oneshot(with_peer_uid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/dream")
                .header("x-agent-root", &root_str)
                .body(Body::empty())
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(dream_resp.status(), StatusCode::OK);

    // Confirm a rename actually happened (so this test really exercises the
    // bug): the decayed record was archived to a snapshot file.
    let snapshots: Vec<_> = std::fs::read_dir(agent_root.snapshots_dir())
        .expect("snapshots dir exists after decay")
        .filter_map(Result::ok)
        .collect();
    assert!(
        !snapshots.is_empty(),
        "decay must have archived learning #1 (proves the JSONL was renamed)"
    );

    // Learning #2 — appended AFTER the cycle. The marker is distinctive and
    // redaction-safe.
    const MARKER: &str = "second-learning-after-dream-marker";
    let body2 = serde_json::json!({
        "schema_version": "1.0.0",
        "id": format!("evt_{SAMPLE_ULID}"),
        "timestamp": "2026-06-19T12:00:00Z",
        "pain": 5.0,
        "importance": 5.0,
        "skill_action": "rust::regression",
        "source_harness": "test-harness",
        "content": MARKER
    });
    let resp2 = router
        .into_service()
        .oneshot(with_peer_uid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/learn")
                .header("x-agent-root", &root_str)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body2).unwrap()))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::CREATED);

    // The on-disk JSONL (live inode at the path) must contain learning #2.
    // Pre-fix the coordinator's stale fd wrote it to the orphaned inode, so
    // the path's file did not contain the marker.
    let on_disk = std::fs::read_to_string(agent_root.episodic_jsonl()).expect("read live JSONL");
    assert!(
        on_disk.contains(MARKER),
        "learning #2 must be in the live JSONL after a dream cycle (fd not orphaned); \
             on-disk contents: {on_disk:?}"
    );
}

// -----------------------------------------------------------------------
// WEG-264 Defect 2 — daemon shares one index handle between append + recall
// -----------------------------------------------------------------------

/// The daemon wires the coordinator's `indexer_tx` to the *same*
/// `TantivyIndexHandle` that recall reads (the pinned primary). A learning
/// appended through the coordinator therefore becomes visible to recall
/// within the commit-cadence window (WEG-201 C13) — no second handle, no
/// stale empty reader. Before WEG-264 Defect 2, `run_watch` booted the
/// coordinator with `indexer_tx = None` and recall opened a *separate*
/// handle via `index_map`, so live appends never reached the recall reader.
#[tokio::test]
async fn daemon_primary_handle_shares_index_between_append_and_recall() {
    use crate::server::tantivy_handle::TantivyIndexHandle;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let agent_root = AgentRoot::new(dir.path());
    std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();
    let canonical_root = std::fs::canonicalize(dir.path()).unwrap();

    // Short cadence so the cadence commit fires well inside the poll window
    // (production uses 5 s; the read-after-write window is a feature, not a
    // bug — WEG-201 C13).
    let primary = Arc::new(
        TantivyIndexHandle::open(&agent_root, Duration::from_millis(100))
            .expect("open primary handle"),
    );
    // Coordinator is wired to the SAME handle's indexer.
    let supervisor = Supervisor::start(&agent_root, 8, Some(primary.sender()))
        .expect("start supervisor with indexer");

    let registry_path = dir.path().join("registry.toml");
    write_registry(&registry_path, &canonical_root.to_string_lossy());
    let state = AppState::new(
        registry_path,
        supervisor,
        Config::default(),
        ProjectIndexMap::new(ProjectIndexMapConfig::default()),
        nix::unistd::Uid::current().as_raw(),
    )
    .with_primary(canonical_root.clone(), primary);

    // Append a learning through the coordinator (durable JSONL + indexer).
    let learning = AgentLearning {
        schema_version: "1.0".to_string(),
        id: EventId::parse(&format!("evt_{SAMPLE_ULID}")).unwrap(),
        timestamp: DateTime::parse_from_rfc3339("2026-06-04T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        pain: 7.0,
        importance: 8.0,
        pinned: false,
        skill_action: "rust.build.zlorp".to_string(),
        source_harness: "claude-code".to_string(),
        content: "zlorp aarch64 needs the ring-prebuilt feature".to_string(),
    };
    let (tx, rx) = oneshot::channel();
    state
        .supervisor
        .try_send(MemoryCoordinatorMsg::AppendLearning {
            learning,
            client_dedup_key: None,
            response_tx: tx,
        })
        .await
        .expect("send append");
    rx.await.expect("recv append").expect("append ok");

    // Recall via the pinned primary handle must surface the row within the
    // cadence window. Poll — do NOT assert instantaneously (WEG-201 C13).
    let (_, schema_fields) = crate::index::build_schema();
    let now_sec = chrono::Utc::now().timestamp();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let reader = state
            .with_index_handle(&canonical_root, |h| h.reader().clone())
            .expect("resolve primary handle");
        let results = crate::recall(
            &reader,
            &schema_fields,
            "zlorp aarch64 ring-prebuilt",
            5,
            None,
            now_sec,
        )
        .expect("recall");
        if let Some(top) = results.first() {
            assert!(
                top.content.contains("ring-prebuilt"),
                "recall returned a row but not the expected content: {:?}",
                top.content
            );
            return; // success
        }
        if Instant::now() >= deadline {
            panic!("recall did not surface the appended row within the cadence window");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// -----------------------------------------------------------------------
// WEG-272 — per-project Supervisor routing (stop misfiling B into A)
// -----------------------------------------------------------------------

/// Build a daemon-shaped state with TWO registered projects: the pinned
/// boot project A (real coordinator + pinned primary index, short commit
/// cadence) and a second project B reachable only via `supervisor_map`
/// routing. Mirrors how `run_watch` composes the daemon. Returns
/// `(dir_a, dir_b, canonical_a, canonical_b, state)`; keep both dirs alive.
fn make_routing_state() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    PathBuf,
    PathBuf,
    AppState,
) {
    use crate::server::tantivy_handle::TantivyIndexHandle;
    use std::time::Duration;

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let canonical_a = std::fs::canonicalize(dir_a.path()).unwrap();
    let canonical_b = std::fs::canonicalize(dir_b.path()).unwrap();

    // Registry holds BOTH canonical roots (canonicalized to match the
    // middleware's resolve_project lookup).
    let registry_path = dir_a.path().join("registry.toml");
    let body = format!(
        "[[projects]]\nroot = \"{}\"\n[[projects]]\nroot = \"{}\"\n",
        canonical_a.to_string_lossy(),
        canonical_b.to_string_lossy(),
    );
    std::fs::write(&registry_path, body).unwrap();

    // Boot project A: pinned primary handle (short cadence so the daemon's
    // own recall path stays fast) + a real coordinator wired to it.
    let agent_root_a = AgentRoot::new(canonical_a.clone());
    let primary = Arc::new(
        TantivyIndexHandle::open(&agent_root_a, Duration::from_millis(100))
            .expect("open primary handle for A"),
    );
    let supervisor =
        Supervisor::start(&agent_root_a, 8, Some(primary.sender())).expect("start A coordinator");

    let state = AppState::new(
        registry_path,
        supervisor,
        Config::default(),
        ProjectIndexMap::new(ProjectIndexMapConfig::default()),
        nix::unistd::Uid::current().as_raw(),
    )
    .with_primary(canonical_a.clone(), primary);

    (dir_a, dir_b, canonical_a, canonical_b, state)
}

/// Plant a mid-cycle crash on `project_root` (WAL + partial tmp, no Commit).
fn plant_mid_cycle_crash(project_root: &std::path::Path) -> AgentRoot {
    use crate::wal::{DreamWal, WalIntent};

    std::fs::create_dir_all(project_root.join(".agent/.dreamd")).unwrap();
    let root = AgentRoot::new(project_root);

    let tmp_path = root.episodic_jsonl().with_extension("tmp");
    std::fs::create_dir_all(tmp_path.parent().unwrap()).unwrap();
    std::fs::write(&tmp_path, b"partial write\n").unwrap();

    let wal = DreamWal {
        schema_version: "1.0".to_string(),
        intents: vec![WalIntent::PruneEpisodicMemory {
            temp_file_path: tmp_path.to_string_lossy().into_owned(),
        }],
    };
    std::fs::write(
        root.wal_path(),
        serde_json::to_string_pretty(&wal).unwrap().as_bytes(),
    )
    .unwrap();

    let state = serde_json::json!({
        "schema_version": "1.0",
        "last_dream_cycle_status": "in_progress",
        "last_dream_cycle_at": null,
    });
    std::fs::write(
        root.state_json(),
        serde_json::to_string_pretty(&state).unwrap(),
    )
    .unwrap();

    root
}

/// WEG-60: lazy per-project open must recover a stale WAL before starting
/// the coordinator for a non-boot project.
#[tokio::test]
async fn lazy_resolve_recovers_mid_cycle_wal_on_project_b() {
    let (_dir_a, _dir_b, _canonical_a, canonical_b, state) = make_routing_state();
    let root_b = plant_mid_cycle_crash(&canonical_b);

    assert!(root_b.wal_path().exists(), "precondition: WAL must exist");

    state
        .resolve_supervisor(&canonical_b)
        .expect("resolve_supervisor must recover and start B");

    assert!(
        !root_b.wal_path().exists(),
        "WAL must be removed after lazy recovery"
    );
    assert!(
        !root_b.episodic_jsonl().with_extension("tmp").exists(),
        ".jsonl.tmp must be removed after lazy recovery"
    );
    let recovered: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root_b.state_json()).unwrap()).unwrap();
    assert_eq!(recovered["last_dream_cycle_status"], "failed");
}

/// Headline AC: a `POST /learn` for project B appends to B's JSONL, leaves
/// the boot project A's JSONL untouched, and recall(B) surfaces it.
/// Before WEG-272 the append misfiled into A's JSONL (the live data-loss
/// bug: dispatch went to the boot coordinator regardless of `entry.root`).
#[tokio::test]
async fn learn_routes_to_owning_project_not_boot() {
    use crate::server::tantivy_handle::IndexerMsg;

    let (_dir_a, _dir_b, canonical_a, canonical_b, state) = make_routing_state();
    let router = build_router(state.clone());

    const MARKER: &str = "routed distinctive marker for project bee";
    let body = sample_body("rust::routing", MARKER);
    let resp = router
        .into_service()
        .oneshot(with_peer_uid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/learn")
                .header("x-agent-root", canonical_b.to_str().unwrap())
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "append to B must succeed"
    );

    // (1) The record landed in B's JSONL.
    let b_jsonl = std::fs::read_to_string(AgentRoot::new(canonical_b.clone()).episodic_jsonl())
        .expect("B's JSONL must exist after a routed append");
    assert!(
        b_jsonl.contains(MARKER),
        "B's JSONL must contain the routed learning; got: {b_jsonl:?}"
    );

    // (2) Project A's JSONL is untouched — this is the misfiling bug's tell.
    let a_jsonl = std::fs::read_to_string(AgentRoot::new(canonical_a.clone()).episodic_jsonl())
        .unwrap_or_default();
    assert!(
        !a_jsonl.contains(MARKER),
        "the boot project A's JSONL must NOT contain B's learning; got: {a_jsonl:?}"
    );

    // (3) recall(B) surfaces it. The coordinator forwarded IndexerMsg::Append
    // before the 201, so a Flush on B's indexer is FIFO-ordered after it;
    // flush + reload makes the assertion deterministic (no cadence wait).
    let b_sender = state
        .with_index_handle(&canonical_b, |h| h.sender())
        .expect("resolve B index sender");
    let (ack_tx, ack_rx) = oneshot::channel();
    b_sender
        .send(IndexerMsg::Flush { ack: ack_tx })
        .await
        .expect("send flush to B indexer");
    ack_rx.await.expect("flush ack").expect("flush ok");

    let b_reader = state
        .with_index_handle(&canonical_b, |h| h.reader().clone())
        .expect("resolve B reader");
    b_reader.reload().expect("reload B reader to latest commit");

    let (_, schema_fields) = crate::index::build_schema();
    let now_sec = chrono::Utc::now().timestamp();
    let results =
        crate::recall(&b_reader, &schema_fields, MARKER, 5, None, now_sec).expect("recall B");
    assert!(
        results
            .iter()
            .any(|r| r.content.contains("distinctive marker")),
        "recall(B) must return the routed learning; got {} results",
        results.len()
    );
}

/// A `POST /learn` for the pinned boot project A still routes to the boot
/// coordinator (`state.supervisor`) and lands in A's JSONL — and must NOT
/// spin up a `supervisor_map` entry. This preserves the single-coordinator
/// behaviour every pre-WEG-272 test relied on.
#[tokio::test]
async fn learn_to_boot_root_uses_primary_supervisor() {
    let (_dir_a, _dir_b, canonical_a, _canonical_b, state) = make_routing_state();
    let router = build_router(state.clone());

    const MARKER: &str = "boot project alpha learning marker";
    let body = sample_body("rust::boot", MARKER);
    let resp = router
        .into_service()
        .oneshot(with_peer_uid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/learn")
                .header("x-agent-root", canonical_a.to_str().unwrap())
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let a_jsonl = std::fs::read_to_string(AgentRoot::new(canonical_a.clone()).episodic_jsonl())
        .expect("A's JSONL must exist");
    assert!(
        a_jsonl.contains(MARKER),
        "boot append must land in A's JSONL"
    );

    // The boot project uses state.supervisor — routing must NOT create a
    // per-root coordinator for it.
    assert_eq!(
        state.supervisor_map.lock().unwrap().len(),
        0,
        "boot-project routing must not populate supervisor_map"
    );
}

/// Idempotency is per-coordinator (each owns its own LRU keyed by its
/// canonical root), so the SAME dedup key sent to A and to B yields two
/// distinct records — no cross-project collision — while a repeat on B is
/// still deduplicated within B.
#[tokio::test]
async fn dedup_key_does_not_collide_across_projects() {
    let (_dir_a, _dir_b, canonical_a, canonical_b, state) = make_routing_state();
    let router = build_router(state);

    async fn post_with_key(
        router: &axum::Router,
        root: &str,
        key: &str,
        content: &str,
    ) -> serde_json::Value {
        let body = serde_json::json!({
            "schema_version": "1.0.0",
            "id": format!("evt_{SAMPLE_ULID}"),
            "timestamp": "2026-05-20T12:00:00Z",
            "pain": 5.0,
            "importance": 5.0,
            "skill_action": "rust::dedup",
            "source_harness": "test-harness",
            "content": content
        });
        let resp = router
            .clone()
            .into_service()
            .oneshot(with_peer_uid(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/learn")
                    .header("x-agent-root", root)
                    .header("content-type", "application/json")
                    .header("x-client-dedup-key", key)
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        body_json(resp).await
    }

    let a = post_with_key(&router, canonical_a.to_str().unwrap(), "shared-key", "to A").await;
    let b = post_with_key(&router, canonical_b.to_str().unwrap(), "shared-key", "to B").await;
    let b2 = post_with_key(
        &router,
        canonical_b.to_str().unwrap(),
        "shared-key",
        "to B again",
    )
    .await;

    assert_ne!(
        a["id"], b["id"],
        "same dedup key on different projects must NOT collide (per-coordinator LRU)"
    );
    assert_eq!(a["deduplicated"], false, "first A append is fresh");
    assert_eq!(b["deduplicated"], false, "first B append is fresh");
    assert_eq!(b["id"], b2["id"], "repeat on B must dedup within B");
    assert_eq!(
        b2["deduplicated"], true,
        "second B append is flagged deduplicated"
    );
}
