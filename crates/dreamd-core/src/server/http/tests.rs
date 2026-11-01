//! HTTP ingress contract tests and shared test fixtures.
//!
//! Every Unix `AppState` built here sets `auth_json: None`: Unix auth is the
//! peer UID, so `build_router` mounts no bearer layer and never reads that
//! path (AILAB-192).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::handlers::PREFERENCES_SIZE_CAP;
use super::router::{bearer_auth_middleware, request_trace_fields};
use super::{build_router, AppState, PeerUid};
use crate::config::Config;
use crate::coordinator::MemoryCoordinatorMsg;
use crate::layout::AgentRoot;
use crate::registry::RegistryCache;
use crate::server::lifecycle::Supervisor;
use crate::server::ProjectResourceMap;
use crate::writer_lock::{WriterKind, WriterLock};
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
    AppState {
        registry: Arc::new(RegistryCache::new(registry_path.clone())),
        registry_path,
        supervisor: Arc::new(supervisor),
        config: Arc::new(config),
        runtimes: Arc::new(Mutex::new(ProjectResourceMap::with_defaults())),
        daemon_uid: nix::unistd::Uid::current().as_raw(),
        primary: None,
        auth_json: None,
    }
}

/// One project writer token per root per test; every open and start on that
/// root in the test borrows it.
fn writer_lock(agent_root: &AgentRoot) -> WriterLock {
    WriterLock::acquire(agent_root, WriterKind::Watch).expect("project writer lock")
}

/// State backed by a real `MemoryCoordinator` for tests that need actual
/// append semantics (durability, dedup, 413 rejection). Booted the way
/// `run_watch` boots its project: `.agent/` exists, the project writer lock,
/// the index pinned as `primary`, and a coordinator wired to it. The lock lives
/// on in the index and coordinator tasks.
fn make_real_state(project_dir: &std::path::Path, registry_path: PathBuf) -> AppState {
    make_real_state_with_cadence(
        project_dir,
        registry_path,
        crate::server::DEFAULT_COMMIT_CADENCE,
    )
}

/// [`make_real_state`] with the primary index committing every `cadence`.
fn make_real_state_with_cadence(
    project_dir: &std::path::Path,
    registry_path: PathBuf,
    cadence: std::time::Duration,
) -> AppState {
    let canonical = std::fs::canonicalize(project_dir).expect("canonicalize project dir");
    let agent_root = AgentRoot::new(&canonical);
    std::fs::create_dir_all(agent_root.episodic_dir()).expect("episodic dir");
    let lock = writer_lock(&agent_root);
    let primary = Arc::new(
        crate::server::TantivyIndexHandle::open(&agent_root, &lock, cadence)
            .expect("open primary index for test"),
    );
    let supervisor = Supervisor::start(&agent_root, &lock, 8, Some(primary.sender()))
        .expect("start supervisor for test");
    AppState {
        registry: Arc::new(RegistryCache::new(registry_path.clone())),
        registry_path,
        supervisor: Arc::new(supervisor),
        config: Arc::new(Config::default()),
        runtimes: Arc::new(Mutex::new(ProjectResourceMap::with_defaults())),
        daemon_uid: nix::unistd::Uid::current().as_raw(),
        primary: Some((canonical, primary)),
        auth_json: None,
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
/// Required because build_router includes peer_uid_middleware, which
/// expects PeerUid to be set (as serve_uds does at connection-accept time).
#[cfg(unix)]
fn with_peer_uid(mut req: axum::http::Request<Body>) -> axum::http::Request<Body> {
    let uid = nix::unistd::Uid::current().as_raw();
    req.extensions_mut().insert(PeerUid(uid));
    req
}

fn write_registry(registry_path: &std::path::Path, root: &str) {
    // Stored roots must be canonicalized to match `RegistryCache::resolve`,
    // which canonicalizes the query path. On macOS a tempdir lives
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

// ── Middleware ───────────────────────────────────────────────────────────────

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
    // and the Json extractor rejects with 415.
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
async fn learn_without_content_type_returns_415() {
    // `post_learn` is wired, so an empty body without a content type fails
    // the Json extractor with 415.
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

// ── POST /api/v1/learn (WEG-68) ──────────────────────────────────────────────

#[tokio::test]
async fn learn_valid_body_returns_201() {
    let (dir, root_str, router) = real_router_with_dir();
    let agent_root = AgentRoot::new(dir.path());

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
    let resp_ts = json["timestamp"].as_str().unwrap();
    assert_ne!(
        resp_ts, "2026-05-20T12:00:00+00:00",
        "response timestamp must not echo the client-supplied value"
    );
    assert_eq!(json["deduplicated"], false);

    let jsonl = std::fs::read_to_string(agent_root.episodic_jsonl()).unwrap();
    let record: AgentLearning = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(
        record.timestamp.to_rfc3339(),
        resp_ts,
        "on-disk timestamp must match the learn response"
    );
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
    // Uppercase letters and leading/trailing whitespace must be normalized
    // before validation. "  Rust::Cargo::TEST  " → "rust::cargo::test".
    let (dir, root_str, router) = real_router_with_dir();
    let agent_root = AgentRoot::new(dir.path());

    let body = sample_body("  Rust::Cargo::TEST  ", "normalization test");
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
        "normalized form is valid"
    );

    // Verify the normalized skill_action landed on disk.
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
        registry: Arc::new(RegistryCache::new(registry_path.clone())),
        registry_path,
        supervisor: Arc::new(supervisor),
        config: Arc::new(Config::default()),
        runtimes: Arc::new(Mutex::new(ProjectResourceMap::with_defaults())),
        daemon_uid: nix::unistd::Uid::current().as_raw(),
        primary: None,
        auth_json: None,
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

// ── Learn validation: charset, score range, server stamps (WEG-275) ──────────

/// The charset rejects `.`, `-`, and `/`: a `skill_action` is `[a-z0-9_]`
/// segments joined by `::`.
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
/// is overwritten with `RECORD_SCHEMA_VERSION` on the durable write, so an
/// HTTP client cannot choose the schema version its record is stored under.
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

/// AILAB-175: the wire body carries no `id` / `schema_version` / `timestamp`,
/// so a body omitting all three is valid and the daemon mints the id. Before
/// the `AppendLearningRequest` DTO these were required fields and their absence
/// 400'd at the axum extractor, before the handler body ever ran.
#[tokio::test]
async fn learn_without_id_returns_201_and_mints_id() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    let state = make_real_state(dir.path(), registry_path);
    let router = build_router(state);

    let body = serde_json::json!({
        "pain": 5.0,
        "importance": 5.0,
        "skill_action": "rust::test",
        "source_harness": "test-harness",
        "content": "no id, no schema_version, no timestamp"
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

    let json = body_json(resp).await;
    let id = json["id"].as_str().unwrap();
    assert!(id.starts_with("evt_"), "daemon-minted id, got {id:?}");
    EventId::parse(id).expect("minted id must parse as an EventId");
}

/// A client-sent `id` is **ignored, not rejected** (AILAB-175), even one that
/// would fail `EventId::parse` (which requires the `evt_` prefix + 26
/// Crockford chars). The wire DTO has no `id` field, and serde's default
/// ignore-unknown behavior on it is what makes this pass — see the type's
/// doc-comment for why it must stay that way.
#[tokio::test]
async fn learn_ignores_bogus_client_id() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    let state = make_real_state(dir.path(), registry_path);
    let router = build_router(state);

    let body = serde_json::json!({
        "id": "not-a-ulid",
        "pain": 5.0,
        "importance": 5.0,
        "skill_action": "rust::test",
        "source_harness": "test-harness",
        "content": "bogus client id"
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
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "a malformed client id must be ignored, not rejected"
    );

    let json = body_json(resp).await;
    let id = json["id"].as_str().unwrap();
    assert_ne!(id, "not-a-ulid", "client id must not be echoed");
    EventId::parse(id).expect("minted id must parse as an EventId");
}

/// Regression guard (AILAB-175): `build_agent_learning` hardcodes
/// `pinned: false` (MCP has no such param), so routing HTTP through it would
/// silently drop a client's `"pinned": true`. The handler re-applies it; this
/// test fails if that line is ever removed.
#[tokio::test]
async fn learn_client_pinned_true_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    let state = make_real_state(dir.path(), registry_path);
    let agent_root = AgentRoot::new(dir.path());
    let router = build_router(state);

    let body = serde_json::json!({
        "pain": 5.0,
        "importance": 5.0,
        "pinned": true,
        "skill_action": "rust::test",
        "source_harness": "test-harness",
        "content": "pin me"
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
    let record: AgentLearning = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert!(
        record.pinned,
        "client-supplied pinned=true must survive the build_agent_learning mapping"
    );
}

/// The coordinator server-stamps `timestamp`: a client-supplied value is
/// overwritten at durable write and echoed in the learn response.
#[tokio::test]
async fn learn_server_stamps_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    let state = make_real_state(dir.path(), registry_path);
    let agent_root = AgentRoot::new(dir.path());
    let router = build_router(state);

    let body = serde_json::json!({
        "schema_version": "1.0.0",
        "id": format!("evt_{SAMPLE_ULID}"),
        "timestamp": "1999-01-01T00:00:00Z",
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
    let json = body_json(resp).await;
    assert_ne!(
        json["timestamp"].as_str().unwrap(),
        "1999-01-01T00:00:00+00:00",
        "client-supplied timestamp must not be echoed"
    );

    let jsonl = std::fs::read_to_string(agent_root.episodic_jsonl()).unwrap();
    let record: AgentLearning = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(
        record.timestamp.to_rfc3339(),
        json["timestamp"].as_str().unwrap(),
        "on-disk timestamp must match the learn response"
    );
}

// ── GET /api/v1/recall (WEG-69) ──────────────────────────────────────────────

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
    // Health opens the project's runtime, which needs `.agent/` (BZR-1243).
    std::fs::create_dir_all(AgentRoot::new(dir.path()).episodic_dir()).unwrap();

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

// ── AILAB-189: per-request trace fields ──────────────────────────────────────

/// The span's field extraction, asserted directly. `Span` does not expose its
/// recorded values, so `request_trace_fields` is the seam that makes the rule
/// testable without a capturing subscriber.
#[test]
fn request_trace_fields_reads_peer_uid_and_request_id() {
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/v1/learn?ignored=1")
        .header("x-request-id", "abc")
        .body(Body::empty())
        .unwrap();
    #[cfg(unix)]
    req.extensions_mut().insert(PeerUid(42));

    let fields = request_trace_fields(&req);

    assert_eq!(fields.method, axum::http::Method::POST);
    // Path only — the query string is deliberately not in the span, so a
    // recall `q=` never lands in the log.
    assert_eq!(fields.path, "/api/v1/learn");
    assert_eq!(fields.request_id, "abc");
    #[cfg(unix)]
    assert_eq!(fields.peer_uid, Some(42));
}

/// No `x-request-id` header and no `PeerUid` extension: both degrade rather
/// than panic. `"-"` is what a router exercised without `SetRequestIdLayer`
/// would record.
#[test]
fn request_trace_fields_defaults_when_header_and_extension_absent() {
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/health")
        .body(Body::empty())
        .unwrap();

    let fields = request_trace_fields(&req);

    assert_eq!(fields.request_id, "-");
    #[cfg(unix)]
    assert_eq!(fields.peer_uid, None);
}

/// End-to-end through the real layer stack: `SetRequestIdLayer` mints a UUID on
/// a request that carries no id, and `PropagateRequestIdLayer` puts it on the
/// response. Two calls must not share an id, or the generator is not running.
///
/// Shaped after `health_reports_fresh_empty_store`: a naked `/api/v1/health`
/// with no `x-agent-root` is a 400 and would never reach the trace stack.
#[tokio::test]
async fn health_response_carries_generated_request_id() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);
    // Health opens the project's runtime, which needs `.agent/` (BZR-1243).
    std::fs::create_dir_all(AgentRoot::new(dir.path()).episodic_dir()).unwrap();

    let state = make_test_state(registry_path);
    let router = build_router(state);

    let mut ids = Vec::new();
    for _ in 0..2 {
        let req = with_peer_uid(
            Request::builder()
                .method("GET")
                .uri("/api/v1/health")
                .header("x-agent-root", root_str)
                .body(Body::empty())
                .unwrap(),
        );

        let resp = router.clone().into_service().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let id = resp
            .headers()
            .get("x-request-id")
            .expect("x-request-id on response")
            .to_str()
            .unwrap()
            .to_owned();
        // Hyphenated UUID v4: 8-4-4-4-12.
        assert_eq!(id.len(), 36, "not a hyphenated UUID: {id}");
        assert_eq!(id.matches('-').count(), 4, "not a hyphenated UUID: {id}");
        ids.push(id);
    }

    assert_ne!(ids[0], ids[1], "request ids must be generated per request");
}

#[tokio::test]
async fn recall_empty_index_returns_empty_results() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    // An initialized project: the index opens under the project writer lock,
    // which never scaffolds a missing `.agent/`.
    std::fs::create_dir_all(dir.path().join(".agent")).unwrap();
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

// ── GET /api/v1/observability/salience (BZR-195) ─────────────────────────────

#[tokio::test]
async fn salience_report_empty_index_writes_today_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    // An initialized project: the index opens under the project writer lock,
    // which never scaffolds a missing `.agent/`.
    std::fs::create_dir_all(dir.path().join(".agent")).unwrap();
    let state = make_test_state(registry_path);
    let router = build_router(state);

    let req = with_peer_uid(
        Request::builder()
            .method("GET")
            .uri("/api/v1/observability/salience")
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["schema_version"], "observability/1.0");
    assert_eq!(json["total"], 0);
    assert_eq!(
        json["histogram"],
        serde_json::json!([0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
    );
    assert_eq!(json["drift"], serde_json::Value::Null);

    let date = json["date"].as_str().unwrap();
    let snapshot = crate::layout::AgentRoot::new(dir.path())
        .dreamd_dir()
        .join("observability")
        .join(format!("salience-{date}.json"));
    assert!(snapshot.exists(), "missing {}", snapshot.display());
}

#[tokio::test]
async fn recall_returns_200_for_registered_root() {
    // Smoke test: registered root + valid q param → 200 with results array.
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    // An initialized project: the index opens under the project writer lock,
    // which never scaffolds a missing `.agent/`.
    std::fs::create_dir_all(dir.path().join(".agent")).unwrap();
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

/// `GET /api/v1/recall?{query}` against an initialized, registered project.
async fn recall_with_query(query: &str) -> axum::response::Response {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);
    std::fs::create_dir_all(dir.path().join(".agent")).unwrap();
    let router = build_router(make_test_state(registry_path));

    let req = with_peer_uid(
        Request::builder()
            .method("GET")
            .uri(format!("/api/v1/recall?{query}"))
            .header("x-agent-root", root_str)
            .body(Body::empty())
            .unwrap(),
    );
    router.into_service().oneshot(req).await.unwrap()
}

/// BZR-1301 test 19: the daemon answers 414 above the shared limit, counted
/// on the decoded `q`, with the shared message as a JSON error.
#[tokio::test]
async fn recall_refuses_a_query_over_the_shared_limit() {
    use crate::ingress::{query_too_long_message, MAX_RECALL_QUERY_BYTES};

    let resp = recall_with_query(&format!("q={}", "a".repeat(MAX_RECALL_QUERY_BYTES + 1))).await;
    assert_eq!(resp.status(), StatusCode::URI_TOO_LONG);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        body_json(resp).await,
        serde_json::json!({ "error": query_too_long_message() })
    );

    let resp = recall_with_query(&format!("q={}", "a".repeat(MAX_RECALL_QUERY_BYTES))).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a query at the limit is searched"
    );

    // MAX_RECALL_QUERY_BYTES decoded, three times that on the wire.
    let encoded = "%C3%A9".repeat(MAX_RECALL_QUERY_BYTES / 2);
    let resp = recall_with_query(&format!("q={encoded}")).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the limit counts decoded bytes, not the encoded URI"
    );
}

/// BZR-1301 test 21 (D1): a query the parser rejects is a 400 with a JSON
/// error, not a 500.
#[tokio::test]
async fn recall_parser_rejection_is_400() {
    for query in ["q=rust%3A%3Atokio", "q=%22unbalanced"] {
        let resp = recall_with_query(query).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{query}");
        let json = body_json(resp).await;
        let error = json["error"].as_str().expect("JSON error string");
        assert!(error.starts_with("recall failed: "), "{query}: {error}");
    }
}

/// BZR-1301 test 20: above 65,534 URI bytes hyper refuses the request
/// before routing, with an empty-bodied 414. Unchanged by this ticket; the
/// client's 414 arm relies on it.
#[cfg(unix)]
#[tokio::test]
async fn hyper_refuses_a_request_target_over_its_uri_limit() {
    use crate::server::uds_server::{bind_api_socket, serve_uds};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("uri.sock");
    let router = build_router(make_test_state(dir.path().join("registry.toml")));
    let listener = bind_api_socket(&sock_path).expect("bind");
    tokio::spawn(async move {
        serve_uds(listener, router).await.ok();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .expect("connect");
    let request = format!(
        "GET /api/v1/recall?q={} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        "a".repeat(70_000)
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let response = String::from_utf8_lossy(&response);
    assert!(response.starts_with("HTTP/1.1 414"), "got: {response}");
    let (_, body) = response.split_once("\r\n\r\n").expect("head and body");
    assert!(body.is_empty(), "hyper's 414 has no body; got: {body:?}");
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
        registry: Arc::new(RegistryCache::new(registry_path.clone())),
        registry_path,
        supervisor: Arc::new(Supervisor::for_backpressure_test().0),
        config: Arc::new(Config::default()),
        runtimes: Arc::new(Mutex::new(ProjectResourceMap::with_defaults())),
        daemon_uid: wrong_uid,
        primary: None,
        auth_json: None,
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

// ── GET /api/v1/preferences (WEG-82) ─────────────────────────────────────────

/// Absent PREFERENCES.md → 200, empty body, no truncation headers.
#[tokio::test]
async fn preferences_absent_returns_200_empty() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    // Deliberately do not write PREFERENCES.md.
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

/// PREFERENCES.md within `PREFERENCES_SIZE_CAP` (16 KiB) → full body returned,
/// no truncation headers.
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

/// PREFERENCES.md over `PREFERENCES_SIZE_CAP` → truncated body, truncation
/// headers set.
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

/// Missing X-Agent-Root header → 400.
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

/// X-Agent-Root present but path not in registry → 404.
#[tokio::test]
async fn preferences_unregistered_root_404() {
    let dir = tempfile::tempdir().unwrap();
    // Registry file is absent entirely — `RegistryCache::resolve` returns Ok(None).
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

// ── POST /api/v1/dream (WEG-70) ──────────────────────────────────────────────

/// Happy path: non-empty JSONL, POST /api/v1/dream → 200 {"status":"ok"}.
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
            b"{\"schema_version\":\"1.0.0\",\"id\":\"evt_01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"pain\":5.0,\"importance\":5.0,\"pinned\":false,\"skill_action\":\"rust.test\",\"source_harness\":\"test\",\"content\":\"test content\",\"recurrence\":0}\n",
        )
        .unwrap();

    // Real coordinator: the dream cycle routes through it (WEG-271), so the
    // mock backpressure supervisor would reject the dispatch.
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

/// 409 guard: state.json with "in_progress" → 409.
#[tokio::test]
async fn dream_in_progress_returns_409() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap();
    write_registry(&registry_path, root_str);

    let agent_root = AgentRoot::new(dir.path());
    std::fs::create_dir_all(agent_root.dreamd_dir()).unwrap();
    let state = make_test_state(registry_path);
    // The project's writer is live first: its runtime holds the project writer
    // lock, so the status below belongs to a running cycle, not a stale one
    // (taking the lock resets a stale `in_progress` with no WAL).
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    state
        .with_index_handle(&canonical, |_| ())
        .expect("open the project's runtime");

    // Write state.json with "in_progress" status to simulate a running cycle.
    std::fs::write(
        agent_root.state_json(),
        b"{\"schema_version\":\"1.0\",\"last_dream_cycle_status\":\"in_progress\"}\n",
    )
    .unwrap();

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

/// Empty JSONL: POST /api/v1/dream → 200, LESSONS.md not created.
///
/// An empty JSONL promotes no cluster, so the cycle's plan writes no
/// LESSONS.md.
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

    // Real coordinator: the dream cycle routes through it (WEG-271).
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

    // LESSONS.md must not have been created for an empty JSONL.
    let lessons_path = agent_root.semantic_dir().join("LESSONS.md");
    assert!(
        !lessons_path.exists(),
        "LESSONS.md must not be created when JSONL is empty"
    );
}

/// A dream cycle must not orphan the coordinator's append fd (WEG-271).
/// After the cycle replaces AGENT_LEARNINGS.jsonl by atomic rename, a
/// subsequent POST /learn must land on the live inode (the file at the
/// path), not the unlinked inode a stale fd would point at.
///
/// Learning #1 is dated > 90 days in the past so the decay pruner archives
/// it and the commit renames a new JSONL into place — the rename a stale fd
/// would miss. (A single fresh learning would not decay and would trigger no
/// rename, so the defect could not show.) Learning #2 is appended after the
/// cycle; through a stale fd it would land in the orphaned inode and never
/// appear in the on-disk file.
///
/// Learning #1 is seeded directly on disk: `POST /learn` server-stamps
/// `timestamp` with `Utc::now()`, which would defeat the >90d decay precondition.
#[tokio::test]
async fn dream_cycle_does_not_orphan_coordinator_append_fd() {
    let dir = tempfile::tempdir().unwrap();
    let agent_root = AgentRoot::new(dir.path());
    std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();
    std::fs::create_dir_all(agent_root.dreamd_dir()).unwrap();

    // Learning #1 — timestamp > 90 days old so the decay pruner archives it,
    // forcing the rewrite-by-rename that orphans the coordinator fd.
    let learning1 = AgentLearning {
        schema_version: "1.0.0".to_owned(),
        id: EventId::parse(&format!("evt_{SAMPLE_ULID}")).unwrap(),
        timestamp: DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        pain: 5.0,
        importance: 5.0,
        pinned: false,
        skill_action: "rust::regression".to_owned(),
        source_harness: "test-harness".to_owned(),
        content: "first-learning-pre-dream".to_owned(),
    };
    let line = serde_json::to_string(&learning1).unwrap();
    std::fs::write(agent_root.episodic_jsonl(), format!("{line}\n")).unwrap();

    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap().to_owned();
    write_registry(&registry_path, &root_str);
    let state = make_real_state(dir.path(), registry_path);
    let router = build_router(state);
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

    // Learning #2 — appended after the cycle. The marker is distinctive and
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

/// The coordinator's open cycle is the daemon's in-progress guard (BZR-1241).
/// While a prepared cycle's ticket is alive, `POST /dream` is 409; once the
/// ticket drops, the next one runs.
#[tokio::test]
async fn dream_while_a_prepared_cycle_is_live_returns_409() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root_str = dir.path().to_str().unwrap().to_owned();
    write_registry(&registry_path, &root_str);
    let agent_root = AgentRoot::new(dir.path());
    std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();
    std::fs::create_dir_all(agent_root.dreamd_dir()).unwrap();
    std::fs::write(agent_root.episodic_jsonl(), b"").unwrap();

    let state = make_real_state(dir.path(), registry_path);
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let supervisor = state
        .resolve_supervisor(&canonical)
        .expect("the project's supervisor");
    let (prepare_tx, prepare_rx) = oneshot::channel();
    supervisor
        .sender()
        .send(MemoryCoordinatorMsg::PrepareCycle {
            now_sec: 1_780_000_000,
            response_tx: prepare_tx,
        })
        .await
        .unwrap();
    let prepared = prepare_rx.await.unwrap().expect("prepare");

    let router = build_router(state);
    let dream = || {
        with_peer_uid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/dream")
                .header("x-agent-root", &root_str)
                .header(crate::client::NO_LLM_HEADER, "1")
                .body(Body::empty())
                .unwrap(),
        )
    };

    let resp = router
        .clone()
        .into_service()
        .oneshot(dream())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let json = body_json(resp).await;
    assert_eq!(json["error"], serde_json::json!("dream cycle in progress"));

    drop(prepared);
    let resp = router.into_service().oneshot(dream()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Daemon shares one index handle between append and recall (WEG-264) ───────

/// The daemon wires the coordinator's `indexer_tx` to the *same*
/// `TantivyIndexHandle` that recall reads (the pinned primary). A learning
/// appended through the coordinator therefore becomes visible to recall
/// once that handle commits — no second handle, no stale empty reader. With
/// `indexer_tx = None` the live append would never be indexed, and a recall
/// that opened its own handle would read an index the append never reaches.
///
/// The commit is driven by `flush().await` plus one reader reload, not by
/// polling the cadence ticker (BZR-170). The cadence is set far past the
/// test's lifetime so the only commit is the one this test asks for.
#[tokio::test]
async fn daemon_primary_handle_shares_index_between_append_and_recall() {
    use crate::server::tantivy_handle::TantivyIndexHandle;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let agent_root = AgentRoot::new(dir.path());
    std::fs::create_dir_all(agent_root.episodic_dir()).unwrap();
    let canonical_root = std::fs::canonicalize(dir.path()).unwrap();

    // Cadence far past the test's lifetime: the ticker never commits, so the
    // explicit `flush` below is the only commit.
    let lock = writer_lock(&agent_root);
    let primary = Arc::new(
        TantivyIndexHandle::open(&agent_root, &lock, Duration::from_secs(3600))
            .expect("open primary handle"),
    );
    // Coordinator is wired to the *same* handle's indexer.
    let supervisor = Supervisor::start(&agent_root, &lock, 8, Some(primary.sender()))
        .expect("start supervisor with indexer");

    let registry_path = dir.path().join("registry.toml");
    write_registry(&registry_path, &canonical_root.to_string_lossy());
    let state = AppState::new(
        registry_path,
        supervisor,
        Config::default(),
        nix::unistd::Uid::current().as_raw(),
    )
    .with_primary(canonical_root.clone(), Arc::clone(&primary));

    // Append a learning through the coordinator (durable JSONL + indexer).
    let learning = AgentLearning {
        schema_version: "1.0.0".to_string(),
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

    // The coordinator awaited its `IndexerMsg::Append` send before replying,
    // and the indexer channel is FIFO, so this flush commits that append.
    primary.flush().await.expect("flush primary index");

    let (_, schema_fields) = crate::index::build_schema();
    let now_sec = chrono::Utc::now().timestamp();
    let reader = state
        .with_index_handle(&canonical_root, |h| h.reader().clone())
        .expect("resolve primary handle");
    reader.reload().expect("reload reader after flush");
    let results = crate::recall(
        &reader,
        &schema_fields,
        "zlorp aarch64 ring-prebuilt",
        5,
        now_sec,
    )
    .expect("recall");
    let top = results
        .first()
        .expect("recall must surface the appended row after flush + reload");
    assert!(
        top.content.contains("ring-prebuilt"),
        "recall returned a row but not the expected content: {:?}",
        top.content
    );
}

// ── Per-project Supervisor routing (WEG-272) ─────────────────────────────────

/// Build a daemon-shaped state with two registered projects: the pinned
/// boot project A (real coordinator + pinned primary index, short commit
/// cadence) and a second project B reachable only via `runtimes`
/// routing. Mirrors how `run_watch` composes the daemon. Both projects have a
/// `.agent/`, as `dreamd init` leaves them. Returns
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

    // Registry holds both canonical roots (canonicalized to match the
    // middleware's `RegistryCache::resolve` lookup).
    let registry_path = dir_a.path().join("registry.toml");
    let body = format!(
        "[[projects]]\nroot = \"{}\"\n[[projects]]\nroot = \"{}\"\n",
        canonical_a.to_string_lossy(),
        canonical_b.to_string_lossy(),
    );
    std::fs::write(&registry_path, body).unwrap();

    std::fs::create_dir_all(AgentRoot::new(canonical_b.clone()).agent_dir()).unwrap();

    // Boot project A: pinned primary handle (short cadence so the daemon's
    // own recall path stays fast) + a real coordinator wired to it, under A's
    // project writer lock.
    let agent_root_a = AgentRoot::new(canonical_a.clone());
    std::fs::create_dir_all(agent_root_a.agent_dir()).unwrap();
    let lock_a = writer_lock(&agent_root_a);
    let primary = Arc::new(
        TantivyIndexHandle::open(&agent_root_a, &lock_a, Duration::from_millis(100))
            .expect("open primary handle for A"),
    );
    let supervisor = Supervisor::start(&agent_root_a, &lock_a, 8, Some(primary.sender()))
        .expect("start A coordinator");

    let state = AppState::new(
        registry_path,
        supervisor,
        Config::default(),
        nix::unistd::Uid::current().as_raw(),
    )
    .with_primary(canonical_a.clone(), primary);

    (dir_a, dir_b, canonical_a, canonical_b, state)
}

/// Plant a crash a pre-1.2.0 binary left on `project_root`: a legacy `intents`
/// WAL with a partial tmp and no `Commit`, which this binary still recovers the
/// old way.
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

/// A WAL opened after B's coordinator is live belongs to a running cycle, not
/// a crashed one. A later request for B must not recover it away.
#[tokio::test]
async fn resolve_supervisor_leaves_a_live_projects_wal_alone() {
    let (_dir_a, _dir_b, _canonical_a, canonical_b, state) = make_routing_state();
    state
        .resolve_supervisor(&canonical_b)
        .expect("first resolve starts B");
    let root_b = AgentRoot::new(&canonical_b);
    // B's running cycle has reached its commit point: its redo WAL is on disk.
    std::fs::write(
        root_b.wal_path(),
        r#"{"schema_version":"1.0","format":"redo/1","renames":[],"unlinks":[],"edges":[]}"#,
    )
    .expect("B's running cycle writes its redo WAL");
    state
        .resolve_supervisor(&canonical_b)
        .expect("second resolve reuses B");
    assert!(
        root_b.wal_path().exists(),
        "a request must not run recovery on a project whose coordinator is live"
    );
}

/// A live non-boot root resolves to the *same* coordinator on every request:
/// two coordinators on one JSONL corrupt it.
#[tokio::test]
async fn resolve_supervisor_returns_the_same_arc_for_a_live_root() {
    let (_dir_a, _dir_b, _canonical_a, canonical_b, state) = make_routing_state();

    let first = state
        .resolve_supervisor(&canonical_b)
        .expect("first resolve starts B");
    let second = state
        .resolve_supervisor(&canonical_b)
        .expect("second resolve reuses B");

    assert!(
        Arc::ptr_eq(&first, &second),
        "a live root must return the same Arc — two coordinators on one JSONL corrupt it"
    );
    assert_eq!(state.runtimes.lock().unwrap().len(), 1);
}

/// A `POST /learn` for project B appends to B's JSONL, leaves the boot
/// project A's JSONL untouched, and recall(B) surfaces it (WEG-272). A
/// dispatch to the boot coordinator regardless of `entry.root` would misfile
/// B's learning into A's JSONL.
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
        "the boot project A's JSONL must not contain B's learning; got: {a_jsonl:?}"
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
    let results = crate::recall(&b_reader, &schema_fields, MARKER, 5, now_sec).expect("recall B");
    assert!(
        results
            .iter()
            .any(|r| r.content.contains("distinctive marker")),
        "recall(B) must return the routed learning; got {} results",
        results.len()
    );
}

/// A `POST /learn` for the pinned boot project A still routes to the boot
/// coordinator (`state.supervisor`) and lands in A's JSONL — and must not
/// open a `runtimes` entry: the boot project has exactly one coordinator.
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

    // The boot project uses state.supervisor — routing must not create a
    // per-root coordinator for it.
    assert_eq!(
        state.runtimes.lock().unwrap().len(),
        0,
        "boot-project routing must not populate runtimes"
    );
}

/// Idempotency is per-coordinator (each owns its own LRU keyed by its
/// canonical root), so the same dedup key sent to A and to B yields two
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
        "same dedup key on different projects must not collide (per-coordinator LRU)"
    );
    assert_eq!(a["deduplicated"], false, "first A append is fresh");
    assert_eq!(b["deduplicated"], false, "first B append is fresh");
    assert_eq!(b["id"], b2["id"], "repeat on B must dedup within B");
    assert_eq!(
        b2["deduplicated"], true,
        "second B append is flagged deduplicated"
    );
}

// ── bearer_auth_middleware, the off-Unix auth layer (AILAB-192) ──────────────

/// The token `auth.json` holds for most of these tests.
const BEARER_TOKEN_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A *different* valid token, for the rotation proof. Differs from
/// `BEARER_TOKEN_A` in every nibble, so nothing about the comparison can pass
/// it by accident.
const BEARER_TOKEN_B: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// Write the AILAB-203 `~/.agent/auth.json` wire shape: `{"token":"<64 hex>"}`.
/// `std::fs::write`. These fixtures live in a tempdir and are not the daemon's
/// durable replace.
fn write_auth_json(path: &std::path::Path, token_hex: &str) {
    std::fs::write(path, format!("{{\"token\":\"{token_hex}\"}}\n")).unwrap();
}

/// `Authorization: Bearer <hex>`.
fn bearer_header(scheme_and_credential: &str) -> axum::http::HeaderValue {
    axum::http::HeaderValue::from_str(scheme_and_credential).unwrap()
}

/// A one-route router behind `bearer_auth_middleware`, mounted exactly the way
/// `build_router` mounts it off Unix: outermost, stated with the `auth.json`
/// path alone rather than the whole `AppState`.
///
/// Deliberately **not** `build_router`. Linux is a unix target — which is where
/// every AILAB-192 proof runs — so `build_router` here mounts
/// `peer_uid_middleware` and no bearer layer at all;
/// `unix_router_has_no_bearer_layer` below pins precisely that. The middleware
/// is `cfg`-free so it can be driven on its own from a unix test, which is the
/// whole reason it was written `cfg`-free.
fn bearer_router(auth_json: Option<PathBuf>) -> axum::Router {
    axum::Router::new()
        .route("/ping", axum::routing::get(|| async { StatusCode::OK }))
        .layer(axum::middleware::from_fn_with_state(
            auth_json,
            bearer_auth_middleware,
        ))
}

/// `GET /ping` through `router`, with `authorization` set if given. Returns the
/// status and the parsed body so the "every rejection looks identical" property
/// is assertable, not just the status.
async fn bearer_ping(
    router: &axum::Router,
    authorization: Option<axum::http::HeaderValue>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method("GET").uri("/ping");
    if let Some(value) = authorization {
        builder = builder.header(axum::http::header::AUTHORIZATION, value);
    }
    let resp = router
        .clone()
        .into_service()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    // A 200 from the `/ping` handler has an empty body; only the 401s carry
    // JSON, and only they are ever inspected.
    if status == StatusCode::OK {
        return (status, serde_json::Value::Null);
    }
    (status, body_json(resp).await)
}

/// AILAB-192: no `Authorization` header → 401. Not 403 — 403 is the
/// peer-UID verdict and this layer can never reach it.
#[tokio::test]
async fn bearer_missing_authorization_is_401() {
    let dir = tempfile::tempdir().unwrap();
    let auth_json = dir.path().join("auth.json");
    write_auth_json(&auth_json, BEARER_TOKEN_A);
    let router = bearer_router(Some(auth_json));

    let (status, _) = bearer_ping(&router, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_ne!(
        status,
        StatusCode::FORBIDDEN,
        "403 is peer-UID's verdict; the bearer layer must never issue it"
    );
}

/// AILAB-192: a well-formed `Bearer` header carrying the wrong 64-hex
/// token → 401.
#[tokio::test]
async fn bearer_wrong_token_is_401() {
    let dir = tempfile::tempdir().unwrap();
    let auth_json = dir.path().join("auth.json");
    write_auth_json(&auth_json, BEARER_TOKEN_A);
    let router = bearer_router(Some(auth_json));

    let (status, _) = bearer_ping(
        &router,
        Some(bearer_header(&format!("Bearer {BEARER_TOKEN_B}"))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// AILAB-192: the right token reaches the handler — the auth layer letting a
/// request through, deliberately on a bare `/ping` and not on
/// `GET /api/v1/health`, which is behind `agent_root_middleware` and opens the
/// project (BZR-1243).
#[tokio::test]
async fn bearer_right_token_is_200() {
    let dir = tempfile::tempdir().unwrap();
    let auth_json = dir.path().join("auth.json");
    write_auth_json(&auth_json, BEARER_TOKEN_A);
    let router = bearer_router(Some(auth_json));

    let (status, _) = bearer_ping(
        &router,
        Some(bearer_header(&format!("Bearer {BEARER_TOKEN_A}"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// AILAB-192: the per-request re-read proof, and the reason
/// `read_auth_token` is called in the middleware body with no cache, no
/// `OnceCell` and no memoized `AuthToken` anywhere on the path.
///
/// One router instance, one middleware layer, one state — only the file on disk
/// changes between the two requests. A token resolved once at mount time (or
/// memoized on first use) would keep honoring `BEARER_TOKEN_A` here and the
/// second request would still be 200, which is exactly the bug: a
/// `dreamd service install --force` rotation would not take effect until the
/// daemon restarted, leaving a revoked credential working.
#[tokio::test]
async fn bearer_rewritten_auth_json_401s_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let auth_json = dir.path().join("auth.json");
    write_auth_json(&auth_json, BEARER_TOKEN_A);
    let router = bearer_router(Some(auth_json.clone()));
    let header = bearer_header(&format!("Bearer {BEARER_TOKEN_A}"));

    let (before, _) = bearer_ping(&router, Some(header.clone())).await;
    assert_eq!(
        before,
        StatusCode::OK,
        "token A is live before the rotation"
    );

    // The `--force` rotation, as far as the running daemon can observe it.
    write_auth_json(&auth_json, BEARER_TOKEN_B);

    let (after, _) = bearer_ping(&router, Some(header)).await;
    assert_eq!(
        after,
        StatusCode::UNAUTHORIZED,
        "the same router instance must re-read auth.json and reject the rotated-out token"
    );

    // And the new token works through that same instance, so the rejection
    // above is a re-read and not the layer having simply broken.
    let (rotated, _) = bearer_ping(
        &router,
        Some(bearer_header(&format!("Bearer {BEARER_TOKEN_B}"))),
    )
    .await;
    assert_eq!(rotated, StatusCode::OK, "the rotated-in token is accepted");
}

/// AILAB-192: `auth.json` does not exist yet (nothing has run
/// `dreamd service install`) → 401, with a body byte-identical to the
/// wrong-token 401. Distinguishable answers here would be a probe oracle
/// reporting the daemon's install state to an unauthenticated caller.
#[tokio::test]
async fn bearer_absent_auth_file_is_401() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("auth.json");
    assert!(!absent.exists(), "fixture must start with no token file");
    let no_file_router = bearer_router(Some(absent));

    let (status, no_file_body) = bearer_ping(
        &no_file_router,
        Some(bearer_header(&format!("Bearer {BEARER_TOKEN_A}"))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Same request shape against a router whose file exists and holds a
    // *different* token.
    let present = dir.path().join("present.json");
    write_auth_json(&present, BEARER_TOKEN_B);
    let (mismatch_status, mismatch_body) = bearer_ping(
        &bearer_router(Some(present)),
        Some(bearer_header(&format!("Bearer {BEARER_TOKEN_A}"))),
    )
    .await;

    assert_eq!(mismatch_status, status, "same status for both failures");
    assert_eq!(
        no_file_body, mismatch_body,
        "\"no token file yet\" and \"wrong token\" must be indistinguishable"
    );
}

/// AILAB-192: `auth_json` is `None` → 401 on every request, fail-closed.
/// A router built without a token path has no way to authenticate anyone, so it
/// authenticates no one; reading `None` as "no auth required" would turn a
/// forgotten `with_auth_json` call into an open daemon.
#[tokio::test]
async fn bearer_none_auth_path_is_401() {
    let router = bearer_router(None);

    for authorization in [
        None,
        Some(bearer_header(&format!("Bearer {BEARER_TOKEN_A}"))),
        Some(bearer_header(&format!("Bearer {BEARER_TOKEN_B}"))),
    ] {
        let (status, _) = bearer_ping(&router, authorization).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a router with no auth.json path must never serve a request"
        );
    }
}

/// AILAB-192: the scheme grammar. Only `bearer <non-empty credential>`,
/// ASCII-case-insensitively and with exactly one space, is even considered — a
/// correct credential presented under any other shape is still 401.
#[tokio::test]
async fn bearer_malformed_scheme_is_401() {
    let dir = tempfile::tempdir().unwrap();
    let auth_json = dir.path().join("auth.json");
    write_auth_json(&auth_json, BEARER_TOKEN_A);
    let router = bearer_router(Some(auth_json));

    // Every one of these carries the *correct* token, so a 200 could only come
    // from the header parser being too permissive.
    for raw in [
        format!("Basic {BEARER_TOKEN_A}"),
        BEARER_TOKEN_A.to_string(),
        "Bearer".to_string(),
        "Bearer ".to_string(),
        format!("Bearer  {BEARER_TOKEN_A}"),
        format!("Bearer\t{BEARER_TOKEN_A}"),
    ] {
        let (status, _) = bearer_ping(&router, Some(bearer_header(&raw))).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "malformed Authorization {raw:?} must be 401"
        );
    }

    // A non-ASCII header value. `HeaderValue::from_bytes` accepts obs-text
    // (0x80..=0xFF), so this is a value a real client can put on the wire;
    // `HeaderValue::to_str` refuses it, and that refusal must be a 401 rather
    // than a panic or a 500.
    let non_ascii = axum::http::HeaderValue::from_bytes(b"Bearer \xff\xfe").unwrap();
    let (status, _) = bearer_ping(&router, Some(non_ascii)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a non-ASCII Authorization value must be 401"
    );
}

/// AILAB-192: the scheme match is ASCII-case-insensitive, so a client that
/// sends `bearer` (RFC 7235 makes the scheme token case-insensitive, and HTTP
/// clients differ) is accepted with the right credential.
#[tokio::test]
async fn bearer_lowercase_scheme_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let auth_json = dir.path().join("auth.json");
    write_auth_json(&auth_json, BEARER_TOKEN_A);
    let router = bearer_router(Some(auth_json));

    for scheme in ["bearer", "Bearer", "BEARER", "BeArEr"] {
        let (status, _) = bearer_ping(
            &router,
            Some(bearer_header(&format!("{scheme} {BEARER_TOKEN_A}"))),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "scheme {scheme:?} must be accepted");
    }
}

/// AILAB-192: the Unix router's outermost layer is still the peer-UID one
/// and **no** bearer layer leaked onto this target.
///
/// A real `build_router(state)` request with neither an `Authorization` header
/// nor a `PeerUid` extension is the one probe that separates the two: the
/// bearer layer would answer 401 (it sees no credential), the peer-UID layer
/// answers 403 (it sees no extension). 403 therefore pins both facts at once —
/// peer-UID is outermost, and the `#[cfg(not(unix))]` bearer arm is genuinely
/// absent here rather than merely mounted further in.
#[tokio::test]
async fn unix_router_has_no_bearer_layer() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_test_state(dir.path().join("registry.toml"));
    let router = build_router(state);

    let resp = router
        .into_service()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/recall?q=test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "the Unix router's outermost layer must be peer_uid_middleware"
    );
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a 401 here would mean the bearer layer is mounted on Unix"
    );
}

// ── POST /api/v1/migrate stub (BZR-187) ──────────────────────────────────────

#[tokio::test]
async fn migrate_without_peer_uid_is_403() {
    let (_dir, root_str, router) = mock_router_with_dir();

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/migrate")
        .header("x-agent-root", &root_str)
        .body(Body::empty())
        .unwrap();

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = body_json(resp).await;
    let error = json["error"].as_str().expect("error field");
    assert!(
        !error.contains("no schema migration available"),
        "auth must reject before the stub handler runs: {error}"
    );
}

#[tokio::test]
async fn migrate_missing_agent_root_is_400() {
    let (_dir, router) = test_router();

    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/migrate")
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn migrate_unregistered_root_is_404() {
    let (dir, router) = test_router();

    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/migrate")
            .header("x-agent-root", dir.path().to_str().unwrap())
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn migrate_registered_root_is_501() {
    let (_dir, root_str, router) = mock_router_with_dir();

    let req = with_peer_uid(
        Request::builder()
            .method("POST")
            .uri("/api/v1/migrate")
            .header("x-agent-root", &root_str)
            .body(Body::empty())
            .unwrap(),
    );

    let resp = router.into_service().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    let json = body_json(resp).await;
    assert_eq!(json["error"], "no schema migration available");
    assert_eq!(
        json["current_schema_version"],
        dreamd_protocol::RECORD_SCHEMA_VERSION
    );
}

// ── /health reads the live marks (BZR-1243) ──────────────────────────────────

/// `POST /api/v1/learn` to `root`; returns the minted id.
async fn post_learn(router: &axum::Router, root: &str, content: &str) -> String {
    let body = sample_body("rust::health", content);
    let resp = router
        .clone()
        .into_service()
        .oneshot(with_peer_uid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/learn")
                .header("x-agent-root", root)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    body_json(resp).await["id"].as_str().unwrap().to_owned()
}

/// `GET /api/v1/health` for `root`: the status and the body.
async fn health_of(router: &axum::Router, root: &str) -> (StatusCode, serde_json::Value) {
    let resp = router
        .clone()
        .into_service()
        .oneshot(with_peer_uid(
            Request::builder()
                .method("GET")
                .uri("/api/v1/health")
                .header("x-agent-root", root)
                .body(Body::empty())
                .unwrap(),
        ))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

/// The `index` object `/health` returns for these values.
fn health_index(
    stale: bool,
    tail: Option<&str>,
    last_indexed: Option<&str>,
    unindexed: usize,
) -> serde_json::Value {
    serde_json::json!({
        "stale": stale,
        "jsonl_tail_id": tail,
        "last_indexed_id": last_indexed,
        "unindexed_count": unindexed,
    })
}

fn primary_of(state: &AppState) -> Arc<crate::server::TantivyIndexHandle> {
    Arc::clone(&state.primary.as_ref().expect("a primary handle").1)
}

#[tokio::test]
async fn health_reads_live_marks_not_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root = dir.path().to_str().unwrap().to_owned();
    write_registry(&registry_path, &root);
    let state = make_real_state(dir.path(), registry_path);
    let router = build_router(state.clone());

    post_learn(&router, &root, "first health learning").await;
    let second = post_learn(&router, &root, "second health learning").await;
    primary_of(&state).flush().await.expect("flush");

    // A directory where the log goes: anything that reads the log fails.
    let jsonl = AgentRoot::new(dir.path()).episodic_jsonl();
    std::fs::rename(&jsonl, jsonl.with_extension("aside")).unwrap();
    std::fs::create_dir(&jsonl).unwrap();

    let (status, json) = health_of(&router, &root).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["index"],
        health_index(false, Some(&second), Some(&second), 0)
    );
}

#[tokio::test]
async fn health_counts_appends_the_indexer_has_not_committed() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root = dir.path().to_str().unwrap().to_owned();
    write_registry(&registry_path, &root);
    // The cadence never fires during the test: `flush` is the only commit.
    let state = make_real_state_with_cadence(
        dir.path(),
        registry_path,
        std::time::Duration::from_secs(3600),
    );
    let router = build_router(state.clone());

    post_learn(&router, &root, "one").await;
    post_learn(&router, &root, "two").await;
    let third = post_learn(&router, &root, "three").await;

    let (status, json) = health_of(&router, &root).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["index"], health_index(true, Some(&third), None, 3));

    primary_of(&state).flush().await.expect("flush");

    let (status, json) = health_of(&router, &root).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["index"],
        health_index(false, Some(&third), Some(&third), 0)
    );
}

#[tokio::test]
async fn health_matches_the_disk_check_when_quiescent() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root = dir.path().to_str().unwrap().to_owned();
    write_registry(&registry_path, &root);
    let state = make_real_state(dir.path(), registry_path);
    let router = build_router(state.clone());

    post_learn(&router, &root, "quiescent one").await;
    post_learn(&router, &root, "quiescent two").await;
    primary_of(&state).flush().await.expect("flush");

    let (status, json) = health_of(&router, &root).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let disk =
        crate::server::assess_index_freshness(&AgentRoot::new(dir.path())).expect("assess on disk");
    assert_eq!(json["index"], serde_json::to_value(disk).unwrap());
}

#[tokio::test]
async fn health_of_a_second_project_reads_its_own_runtime() {
    let (_dir_a, _dir_b, canonical_a, canonical_b, state) = make_routing_state();
    let router = build_router(state.clone());
    let (root_a, root_b) = (
        canonical_a.to_str().unwrap().to_owned(),
        canonical_b.to_str().unwrap().to_owned(),
    );

    post_learn(&router, &root_b, "bee one").await;
    let second_b = post_learn(&router, &root_b, "bee two").await;
    let (ack_tx, ack_rx) = oneshot::channel();
    state
        .with_index_handle(&canonical_b, |h| h.sender())
        .expect("B's index")
        .send(crate::server::IndexerMsg::Flush { ack: ack_tx })
        .await
        .expect("send flush to B");
    ack_rx.await.expect("flush ack").expect("flush ok");

    let (status, json) = health_of(&router, &root_b).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["index"],
        health_index(false, Some(&second_b), Some(&second_b), 0)
    );

    let (status, json) = health_of(&router, &root_a).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["index"], health_index(false, None, None, 0));
}

#[tokio::test]
async fn health_opens_a_cold_project_like_recall() {
    let (_dir_a, _dir_b, canonical_a, canonical_b, state) = make_routing_state();
    // A third registered root with no `.agent/`.
    let dir_c = tempfile::tempdir().unwrap();
    let canonical_c = std::fs::canonicalize(dir_c.path()).unwrap();
    std::fs::write(
        &state.registry_path,
        format!(
            "[[projects]]\nroot = \"{}\"\n[[projects]]\nroot = \"{}\"\n[[projects]]\nroot = \"{}\"\n",
            canonical_a.to_string_lossy(),
            canonical_b.to_string_lossy(),
            canonical_c.to_string_lossy(),
        ),
    )
    .unwrap();
    // B's store holds two records and has no live runtime.
    let agent_root_b = AgentRoot::new(canonical_b.clone());
    std::fs::create_dir_all(agent_root_b.episodic_dir()).unwrap();
    let ids = [
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA0",
        "evt_01ARZ3NDEKTSV4RRFFQ69G5FA1",
    ];
    let mut log = String::new();
    for id in ids {
        let learning = AgentLearning {
            id: EventId::parse(id).unwrap(),
            ..placeholder_learning("rust::cold")
        };
        log.push_str(&serde_json::to_string(&learning).unwrap());
        log.push('\n');
    }
    std::fs::write(agent_root_b.episodic_jsonl(), log).unwrap();
    assert!(
        state
            .runtimes
            .lock()
            .unwrap()
            .get_mut(&canonical_b)
            .is_none(),
        "precondition: B is not loaded"
    );
    let router = build_router(state.clone());

    let (status, json) = health_of(&router, canonical_b.to_str().unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["index"],
        health_index(false, Some(ids[1]), Some(ids[1]), 0)
    );
    assert!(
        state
            .runtimes
            .lock()
            .unwrap()
            .get_mut(&canonical_b)
            .is_some(),
        "health opened B's runtime"
    );

    let (status, json) = health_of(&router, canonical_c.to_str().unwrap()).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a registered root without `.agent/`: {json}"
    );
}

// ── BZR-1437: recall commits the appends acked before it ─────────────────────

/// `GET /api/v1/recall` right after `POST /api/v1/learn` finds the learning.
/// The primary index commits only every hour, so only a commit before the
/// recall can make it visible.
#[tokio::test]
async fn recall_sees_a_learn_acked_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root = dir.path().to_str().unwrap().to_owned();
    write_registry(&registry_path, &root);
    let state = make_real_state_with_cadence(
        dir.path(),
        registry_path,
        std::time::Duration::from_secs(3600),
    );
    let router = build_router(state.clone());

    let id = post_learn(&router, &root, "bzr1437 quillwhisk router").await;
    let resp = router
        .clone()
        .into_service()
        .oneshot(with_peer_uid(
            Request::builder()
                .method("GET")
                .uri("/api/v1/recall?q=quillwhisk&k=5")
                .header("x-agent-root", &root)
                .body(Body::empty())
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let hits = json["results"].as_array().expect("results array");
    assert!(
        hits.iter()
            .any(|r| r["content"] == "bzr1437 quillwhisk router"),
        "the first recall must find the learning: {json}"
    );
    assert_eq!(
        primary_of(&state)
            .commit_mark()
            .last_indexed_id
            .map(|i| i.as_str().to_owned()),
        Some(id),
        "the recall committed the learning"
    );
}

/// `GET /api/v1/recall` for `q` on `root`: the status and the body.
async fn recall_of(router: &axum::Router, root: &str, q: &str) -> (StatusCode, serde_json::Value) {
    let resp = router
        .clone()
        .into_service()
        .oneshot(with_peer_uid(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/recall?q={q}&k=5"))
                .header("x-agent-root", root)
                .body(Body::empty())
                .unwrap(),
        ))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

/// The content of every hit in a recall body.
fn hit_contents(json: &serde_json::Value) -> Vec<String> {
    json["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|r| r["content"].as_str().unwrap_or("").to_owned())
        .collect()
}

/// R1 in the daemon: a recall with nothing pending sends the indexer no
/// commit request. It reads the booted project's coordinator's append count.
#[tokio::test]
async fn recalls_with_nothing_pending_send_no_commit_request() {
    use std::sync::atomic::Ordering::SeqCst;

    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root = dir.path().to_str().unwrap().to_owned();
    write_registry(&registry_path, &root);
    let state = make_real_state_with_cadence(
        dir.path(),
        registry_path,
        std::time::Duration::from_secs(3600),
    );
    let router = build_router(state.clone());
    let primary = primary_of(&state);
    let flushes = || primary.counts().flush_requests.load(SeqCst);

    for _ in 0..3 {
        let (status, _) = recall_of(&router, &root, "nothingyet").await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(flushes(), 0, "no learn yet: no commit request");

    post_learn(&router, &root, "bzr1437 daemonpending").await;
    recall_of(&router, &root, "daemonpending").await;
    assert_eq!(flushes(), 1, "the first recall after a learn asks once");
    for _ in 0..3 {
        recall_of(&router, &root, "daemonpending").await;
    }
    assert_eq!(flushes(), 1, "the learn is committed: no further request");
}

/// R2 in the daemon: when the indexer does not commit within the recall wait,
/// `GET /api/v1/recall` answers `200` from the last commit.
#[tokio::test]
async fn recall_answers_from_the_last_commit_when_the_indexer_stalls() {
    use crate::server::indexer_actor::FLUSH_STALL;
    use crate::server::tantivy_handle::RECALL_FLUSH_TIMEOUT;

    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let root = dir.path().to_str().unwrap().to_owned();
    write_registry(&registry_path, &root);
    let state = make_real_state_with_cadence(
        dir.path(),
        registry_path,
        std::time::Duration::from_secs(3600),
    );
    let router = build_router(state.clone());

    post_learn(&router, &root, "bzr1437 daemonstall alpha").await;
    let (_, json) = recall_of(&router, &root, "daemonstall").await;
    assert!(
        hit_contents(&json).iter().any(|c| c.ends_with("alpha")),
        "the first recall commits alpha: {json}"
    );

    let stall = AgentRoot::new(dir.path())
        .dreamd_dir()
        .join("failpoint-flush-stall");
    std::fs::write(&stall, b"").unwrap();
    post_learn(&router, &root, "bzr1437 daemonstall beta").await;
    let started = std::time::Instant::now();
    let (status, json) = recall_of(&router, &root, "daemonstall").await;
    let waited = started.elapsed();
    std::fs::remove_file(&stall).unwrap();

    assert_eq!(
        status,
        StatusCode::OK,
        "a timed-out wait is not an error: {json}"
    );
    assert!(
        waited >= RECALL_FLUSH_TIMEOUT,
        "the recall waits: {waited:?}"
    );
    assert!(
        waited < FLUSH_STALL,
        "the recall must not wait out the stall: {waited:?}"
    );
    let hits = hit_contents(&json);
    assert!(hits.iter().any(|c| c.ends_with("alpha")), "{json}");
    assert!(!hits.iter().any(|c| c.ends_with("beta")), "{json}");
}
